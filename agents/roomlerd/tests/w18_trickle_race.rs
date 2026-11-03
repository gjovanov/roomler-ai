//! W-18 throwaway proof for #3078, NEVER for merge. The harness that
//! `control_dc_record`, `privacy_dc` and `file_dc` used before #3166 (trickle:
//! each peer's `on_ice_candidate` straight into the other's
//! `add_ice_candidate`, the result discarded), with the stalled runner made
//! certain, against the exchange they use since (every candidate in the SDP).
//! Refusals are counted here instead of discarded.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, oneshot};
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;

async fn pcs() -> Result<(Arc<RTCPeerConnection>, Arc<RTCPeerConnection>)> {
    let api = || -> Result<_> {
        let mut me = MediaEngine::default();
        me.register_default_codecs()?;
        Ok(APIBuilder::new().with_media_engine(me).build())
    };
    let cfg = RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    };
    let browser_pc = Arc::new(api()?.new_peer_connection(cfg.clone()).await?);
    let agent_pc = Arc::new(api()?.new_peer_connection(cfg).await?);
    Ok((browser_pc, agent_pc))
}

#[derive(Default)]
struct Counts {
    sent: AtomicUsize,
    refused: AtomicUsize,
}

/// One direction of the pre-#3166 wiring: `from`'s candidates into `to`.
fn bridge(from: &Arc<RTCPeerConnection>, to: &Arc<RTCPeerConnection>) -> Arc<Counts> {
    let counts = Arc::new(Counts::default());
    let (to, c) = (to.clone(), counts.clone());
    from.on_ice_candidate(Box::new(move |cand: Option<RTCIceCandidate>| {
        let (to, c) = (to.clone(), c.clone());
        Box::pin(async move {
            if let Some(cand) = cand
                && let Ok(j) = cand.to_json()
            {
                c.sent.fetch_add(1, Ordering::SeqCst);
                if to.add_ice_candidate(j).await.is_err() {
                    c.refused.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
    }));
    counts
}

/// `exchange` as it was before #3166. `stall_browser` / `stall_agent` hold a
/// side's local description back from the far side until that side's own
/// gathering has finished: what a descheduled test task does on a busy
/// runner, made certain.
async fn exchange_trickle(
    browser_pc: &RTCPeerConnection,
    agent_pc: &RTCPeerConnection,
    stall_browser: bool,
    stall_agent: bool,
) -> Result<()> {
    let offer = browser_pc.create_offer(None).await?;
    let mut gathered = browser_pc.gathering_complete_promise().await;
    browser_pc.set_local_description(offer.clone()).await?;
    if stall_browser {
        let _ = gathered.recv().await;
    }
    agent_pc.set_remote_description(offer).await?;
    let answer = agent_pc.create_answer(None).await?;
    let mut gathered = agent_pc.gathering_complete_promise().await;
    agent_pc.set_local_description(answer.clone()).await?;
    if stall_agent {
        let _ = gathered.recv().await;
    }
    browser_pc.set_remote_description(answer).await?;
    Ok(())
}

/// `exchange` since #3166 (privacy_dc.rs): SDPs after gathering completes.
async fn exchange_complete(
    browser_pc: &RTCPeerConnection,
    agent_pc: &RTCPeerConnection,
) -> Result<()> {
    let offer = browser_pc.create_offer(None).await?;
    let mut gathered = browser_pc.gathering_complete_promise().await;
    browser_pc.set_local_description(offer).await?;
    let _ = gathered.recv().await;
    let offer = browser_pc.local_description().await.expect("local offer");
    agent_pc.set_remote_description(offer).await?;
    let answer = agent_pc.create_answer(None).await?;
    let mut gathered = agent_pc.gathering_complete_promise().await;
    agent_pc.set_local_description(answer).await?;
    let _ = gathered.recv().await;
    let answer = agent_pc.local_description().await.expect("local answer");
    browser_pc.set_remote_description(answer).await?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Harness {
    Trickle {
        stall_browser: bool,
        stall_agent: bool,
    },
    Complete,
}

/// Runs one pair through `harness`: whether the agent saw the DataChannel
/// within `wait`, and a line that says why.
async fn one(harness: Harness, wait: Duration) -> (bool, String) {
    let (browser_pc, agent_pc) = pcs().await.unwrap();
    let counts = match harness {
        Harness::Trickle { .. } => Some((
            bridge(&browser_pc, &agent_pc),
            bridge(&agent_pc, &browser_pc),
        )),
        Harness::Complete => None,
    };
    let _browser_dc = browser_pc.create_data_channel("probe", None).await.unwrap();
    let (tx, rx) = oneshot::channel::<Arc<RTCDataChannel>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    agent_pc.on_data_channel(Box::new(move |dc| {
        let tx = tx.clone();
        Box::pin(async move {
            if let Some(tx) = tx.lock().await.take() {
                let _ = tx.send(dc);
            }
        })
    }));
    match harness {
        Harness::Trickle {
            stall_browser,
            stall_agent,
        } => exchange_trickle(&browser_pc, &agent_pc, stall_browser, stall_agent)
            .await
            .unwrap(),
        Harness::Complete => exchange_complete(&browser_pc, &agent_pc).await.unwrap(),
    }
    let opened = tokio::time::timeout(wait, rx).await.is_ok();
    let line = match &counts {
        Some((to_agent, to_browser)) => format!(
            "{harness:?}: opened={opened}; browser->agent candidates sent={} refused={}; \
             agent->browser sent={} refused={}; ICE agent={} browser={}",
            to_agent.sent.load(Ordering::SeqCst),
            to_agent.refused.load(Ordering::SeqCst),
            to_browser.sent.load(Ordering::SeqCst),
            to_browser.refused.load(Ordering::SeqCst),
            agent_pc.ice_connection_state(),
            browser_pc.ice_connection_state(),
        ),
        None => format!(
            "{harness:?}: opened={opened}; ICE agent={} browser={}",
            agent_pc.ice_connection_state(),
            browser_pc.ice_connection_state(),
        ),
    };
    let _ = browser_pc.close().await;
    let _ = agent_pc.close().await;
    (opened, line)
}

/// Both sides stalled: every candidate both ways arrives before the far
/// side's remote description, is refused, and nothing is left to check.
#[tokio::test(flavor = "multi_thread")]
async fn the_old_harness_never_opens_when_both_sides_stall() {
    let (opened, line) = one(
        Harness::Trickle {
            stall_browser: true,
            stall_agent: true,
        },
        Duration::from_secs(30),
    )
    .await;
    println!("W18 {line}");
    assert!(!opened, "{line}");
}

/// One side stalled: its candidates are all refused, but the other side's
/// arrive, its checks reach the stalled side, and peer-reflexive candidates
/// carry the pair.
#[tokio::test(flavor = "multi_thread")]
async fn the_old_harness_survives_one_side_stalling() {
    for (stall_browser, stall_agent) in [(true, false), (false, true)] {
        let (opened, line) = one(
            Harness::Trickle {
                stall_browser,
                stall_agent,
            },
            Duration::from_secs(30),
        )
        .await;
        println!("W18 {line}");
        assert!(opened, "{line}");
    }
}

/// The exchange since #3166 waits for gathering by construction: the stall
/// is already in it, and every candidate rides the SDP.
#[tokio::test(flavor = "multi_thread")]
async fn the_harness_since_3166_opens_every_time() {
    let n: usize = std::env::var("W18_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut failed = 0;
    for _ in 0..n {
        let (opened, line) = one(Harness::Complete, Duration::from_secs(30)).await;
        if !opened {
            failed += 1;
            println!("W18 {line}");
        }
    }
    println!("W18 Complete: {n} pairs, {failed} never opened");
    assert_eq!(failed, 0);
}
