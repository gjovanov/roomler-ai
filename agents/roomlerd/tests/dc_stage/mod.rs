// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3078 — which stage a loopback pair's setup stuck at.
//!
//! The DataChannel harnesses (`control_dc_record`, `privacy_dc`, `file_dc`)
//! wait for the agent's `on_data_channel`, and a timeout used to say only
//! that: "agent on_data_channel timed out". The channel reaches the agent
//! only after four stages, each on both peers:
//!
//! | stage | up when |
//! |---|---|
//! | ICE | a candidate pair answers its checks and is selected |
//! | DTLS | the handshake over that pair completes |
//! | SCTP | the association over DTLS is established |
//! | DCEP | the agent reads the browser's `DATA_CHANNEL_OPEN` |
//!
//! [`Stages::watch`] keeps a timeline of both peers from the moment the pair
//! is made. A timed-out wait's error then names the first stage that is not
//! up on both peers, with each peer's states, candidates, candidate pairs and
//! that timeline.
//!
//! ⚠️ webrtc-ice drops every candidate when ICE fails
//! (`delete_all_candidates`), and it fails 30 s into checking (its 5 s
//! disconnected plus 25 s failed timeouts), which is about when a 30 s wait
//! ends. So [`agent_channel`] also takes a report halfway through its wait,
//! while the candidates are still there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::sync::oneshot;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_transport_state::RTCDtlsTransportState;
use webrtc::ice_transport::ice_candidate_pair::RTCIceCandidatePair;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::sctp_transport::sctp_transport_state::RTCSctpTransportState;
use webrtc::stats::StatsReportType;

/// How long a report waits for one peer's selected pair or stats. A peer that
/// cannot answer in this time is holding a lock, which is a finding in itself.
const ASK: Duration = Duration::from_secs(5);

/// The two peers of a loopback pair, and the timeline of their states.
pub struct Stages {
    t0: Instant,
    timeline: Arc<Mutex<Vec<(Duration, String)>>>,
    browser: Arc<RTCPeerConnection>,
    agent: Arc<RTCPeerConnection>,
}

impl Stages {
    /// Starts the timeline. From now on it records each peer's ICE, DTLS and
    /// connection-state transitions, and the pair its ICE selects.
    ///
    /// ⚠️ It sets only hooks that the peer connection leaves free. The ICE
    /// TRANSPORT's state hook is what drives the peer's own states, so it is
    /// never touched. The DTLS transport's hook and the selected-pair hook are
    /// unused by the stack.
    pub fn watch(browser: &Arc<RTCPeerConnection>, agent: &Arc<RTCPeerConnection>) -> Self {
        let stages = Self {
            t0: Instant::now(),
            timeline: Arc::default(),
            browser: browser.clone(),
            agent: agent.clone(),
        };
        for (side, pc) in [("browser", browser), ("agent", agent)] {
            let note = stages.note(side);
            pc.on_ice_connection_state_change(Box::new(move |s: RTCIceConnectionState| {
                note(format!("ice={s}"));
                Box::pin(async {})
            }));
            let note = stages.note(side);
            pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                note(format!("pc={s}"));
                Box::pin(async {})
            }));
            let note = stages.note(side);
            pc.dtls_transport()
                .on_state_change(Box::new(move |s: RTCDtlsTransportState| {
                    note(format!("dtls={s}"));
                    Box::pin(async {})
                }));
            let note = stages.note(side);
            pc.dtls_transport()
                .ice_transport()
                .on_selected_candidate_pair_change(Box::new(move |p: RTCIceCandidatePair| {
                    note(format!("selected {p}"));
                    Box::pin(async {})
                }));
        }
        stages
    }

    /// A step of the harness's own exchange, on the same timeline.
    pub fn mark(&self, what: &str) {
        self.note("harness")(what.to_owned());
    }

    /// A description is complete: how many candidates it carries.
    pub fn gathered(&self, what: &str, desc: &RTCSessionDescription) {
        let n = desc
            .sdp
            .lines()
            .filter(|l| l.starts_with("a=candidate:"))
            .count();
        self.mark(&format!("{what} gathered, {n} candidates"));
    }

    fn note(&self, side: &'static str) -> impl Fn(String) + Send + Sync + 'static {
        let (t0, timeline) = (self.t0, self.timeline.clone());
        move |what| {
            if let Ok(mut t) = timeline.lock() {
                t.push((t0.elapsed(), format!("{side} {what}")));
            }
        }
    }

    /// `what` timed out: an error that keeps `what` as its first words, so
    /// the lane's census still finds it, and names the stage the pair is
    /// stuck at.
    pub async fn timed_out(&self, what: &str, browser_dc: &RTCDataChannel) -> anyhow::Error {
        anyhow!("{what}, {}", self.report(browser_dc).await)
    }

    /// "stuck at <stage>", then each peer's states, candidates and pairs, the
    /// browser's channel, and the timeline.
    async fn report(&self, browser_dc: &RTCDataChannel) -> String {
        let b = peer("browser", &self.browser).await;
        let a = peer("agent", &self.agent).await;
        let mut out = format!(
            "stuck at {} (+{} ms)\n{}\n{}\n  the browser's channel: {}\n  timeline:",
            stage(&b, &a),
            self.t0.elapsed().as_millis(),
            b.text,
            a.text,
            browser_dc.ready_state(),
        );
        if let Ok(t) = self.timeline.lock() {
            for (at, what) in t.iter() {
                out += &format!("\n    +{} ms {what}", at.as_millis());
            }
        }
        out
    }
}

/// Waits up to `within` for the agent's `on_data_channel`, as the harnesses
/// always have, and reports halfway as well as at the end (see the module's
/// ⚠️). A channel that arrives after the halfway report is still taken.
pub async fn agent_channel(
    mut rx: oneshot::Receiver<Arc<RTCDataChannel>>,
    stages: &Stages,
    browser_dc: &RTCDataChannel,
    within: Duration,
) -> Result<Arc<RTCDataChannel>> {
    let half = within / 2;
    if let Ok(dc) = tokio::time::timeout(half, &mut rx).await {
        return Ok(dc?);
    }
    let early = stages.report(browser_dc).await;
    match tokio::time::timeout(within - half, &mut rx).await {
        Ok(dc) => Ok(dc?),
        Err(_) => Err(anyhow!(
            "agent on_data_channel timed out after {within:?}, {}\n-- halfway, at {half:?}, {early}",
            stages.report(browser_dc).await
        )),
    }
}

/// One peer at a report.
struct Peer {
    ice: RTCIceConnectionState,
    dtls: RTCDtlsTransportState,
    sctp: RTCSctpTransportState,
    text: String,
}

async fn peer(side: &str, pc: &RTCPeerConnection) -> Peer {
    let (ice, dtls, sctp) = (
        pc.ice_connection_state(),
        pc.dtls_transport().state(),
        pc.sctp().state(),
    );
    let mut text = format!(
        "  {side}: ice={ice} dtls={dtls} sctp={sctp} pc={} signaling={} gathering={}",
        pc.connection_state(),
        pc.signaling_state(),
        pc.ice_gathering_state(),
    );
    let dtls_transport = pc.dtls_transport();
    match tokio::time::timeout(
        ASK,
        dtls_transport.ice_transport().get_selected_candidate_pair(),
    )
    .await
    {
        Ok(Some(p)) => text += &format!("\n    selected: {p}"),
        Ok(None) => text += "\n    selected: none",
        Err(_) => text += &format!("\n    selected: no answer in {ASK:?}"),
    }
    let Ok(stats) = tokio::time::timeout(ASK, pc.get_stats()).await else {
        text += &format!("\n    stats: no answer in {ASK:?}");
        return Peer {
            ice,
            dtls,
            sctp,
            text,
        };
    };
    let mut names = HashMap::new();
    let (mut local, mut remote, mut pairs) = (Vec::new(), Vec::new(), Vec::new());
    for r in stats.reports.values() {
        match r {
            StatsReportType::LocalCandidate(c) | StatsReportType::RemoteCandidate(c) => {
                let name = format!("{}:{} {}", c.ip, c.port, c.candidate_type);
                names.insert(c.id.clone(), name.clone());
                if matches!(r, StatsReportType::LocalCandidate(_)) {
                    local.push(name);
                } else {
                    remote.push(name);
                }
            }
            StatsReportType::CandidatePair(p) => pairs.push(p),
            _ => {}
        }
    }
    let name = |id: &str| names.get(id).cloned().unwrap_or_else(|| format!("({id})"));
    let mut pairs: Vec<String> = pairs
        .into_iter()
        .map(|p| {
            format!(
                "{} -> {} {}{}",
                name(&p.local_candidate_id),
                name(&p.remote_candidate_id),
                p.state,
                if p.nominated { " nominated" } else { "" },
            )
        })
        .collect();
    local.sort();
    remote.sort();
    pairs.sort();
    text += &format!(
        "\n    local ({}): {}\n    remote ({}): {}\n    pairs ({}): {}",
        local.len(),
        local.join(", "),
        remote.len(),
        remote.join(", "),
        pairs.len(),
        pairs.join("; "),
    );
    Peer {
        ice,
        dtls,
        sctp,
        text,
    }
}

/// The first stage that is not up on both peers.
fn stage(b: &Peer, a: &Peer) -> &'static str {
    let ice_up = |p: &Peer| {
        matches!(
            p.ice,
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
        )
    };
    if !(ice_up(b) && ice_up(a)) {
        "ICE"
    } else if b.dtls != RTCDtlsTransportState::Connected
        || a.dtls != RTCDtlsTransportState::Connected
    {
        "DTLS"
    } else if b.sctp != RTCSctpTransportState::Connected
        || a.sctp != RTCSctpTransportState::Connected
    {
        "SCTP"
    } else {
        "DCEP: the association is up, and the agent never read the channel's open"
    }
}
