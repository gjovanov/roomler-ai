// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use bson::oid::ObjectId;
use roomler_ai_remote_control::hive::{
    HiveJoinRefusal, HiveRunState, HiveStreamPath, HiveViewRefusal,
};
use roomler_ai_remote_control::knowhow::ProbeOutcome;
use roomler_ai_remote_control::signaling::ClientMsg;
use roomler_node_core::probe_reach;

use super::*;

fn id() -> ObjectId {
    ObjectId::new()
}

/// Decision 69: the link carries the frames the viewer's peer and the
/// stream speak themselves, and none of the daemon's.
#[test]
fn the_parts_speak_their_own_frames_and_no_other() {
    let theirs = [
        ClientMsg::HiveViewAnswer {
            grant_id: id(),
            sdp: "v=0".into(),
        },
        ClientMsg::HiveViewIce {
            grant_id: id(),
            candidate: serde_json::json!({"candidate": "candidate:1"}),
        },
        ClientMsg::HiveViewClosed {
            grant_id: id(),
            reason: "expired".into(),
        },
        ClientMsg::HiveReplicaReady {
            grant_id: id(),
            refused: None,
            detail: None,
            cert_fingerprint: Some("ab".into()),
            addrs: vec!["127.0.0.1:1".into()],
            derp_pubkey: None,
        },
        ClientMsg::HiveReplicaCandidate {
            grant_id: id(),
            addr: "127.0.0.1:1".into(),
        },
        ClientMsg::HiveReplicaClose {
            grant_id: id(),
            reason: "renewed".into(),
        },
        ClientMsg::HiveReplicaWant {
            session_id: id(),
            fence: 1,
            seq: 0,
            hash: "00".into(),
            path: Some(HiveStreamPath::Direct),
            derp_pubkey: None,
        },
        ClientMsg::HiveReplicaTip {
            session_id: id(),
            fence: 1,
            seq: 0,
            hash: "00".into(),
            checkpoint: None,
        },
    ];
    for msg in &theirs {
        assert!(speaks(msg), "the parts speak {msg:?}");
    }
    let the_daemons = [
        ClientMsg::HiveState {
            session_id: id(),
            fence: 1,
            state: Some(HiveRunState::Idle),
            detail: None,
        },
        ClientMsg::HiveViewGrantAck {
            grant_id: id(),
            refused: Some(HiveViewRefusal::AtCapacity),
            detail: None,
        },
        ClientMsg::HiveReplicaJoinAck {
            session_id: id(),
            fence: 1,
            refused: Some(HiveJoinRefusal::Other),
            detail: None,
        },
        ClientMsg::KnowhowProbed {
            probe_id: id(),
            outcome: ProbeOutcome::Ok,
            rtt_ms: None,
            address: None,
            tls: None,
            detail: None,
        },
        ClientMsg::HiveManifest {
            sessions: Vec::new(),
        },
    ];
    for msg in &the_daemons {
        assert!(!speaks(msg), "the daemon speaks {msg:?}, never the parts");
    }
}

/// The prober's seam, as the daemon runs it today: what the device's parts
/// measure is the probe itself, and the frame the daemon speaks from it is
/// the one the probe's own answer was.
#[tokio::test]
async fn the_daemon_s_measure_is_the_probe() {
    let rules = probe_reach::rules(&["127.0.0.1".to_string()]);
    let cases = [
        // A kind this build does not probe: `failed`, never a connect.
        Asked {
            id: id(),
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port: 80,
            sni: None,
        },
        // Outside the list: `not_allowed`.
        Asked {
            id: id(),
            kind: "tcp".into(),
            host: "10.9.8.7".into(),
            port: 80,
            sni: None,
        },
    ];
    for asked in cases {
        let measured = InDaemon.measure(asked.clone(), rules.clone()).await;
        let probed = crate::hive::knowhow_probe::probe(&asked, &rules).await;
        assert_eq!(measured, probed);
        assert_ne!(measured.outcome, ProbeOutcome::Ok);
        let frame = measured.answer(asked.id);
        let direct = crate::hive::knowhow_probe::measure(&asked, &rules).await;
        assert_eq!(format!("{frame:?}"), format!("{direct:?}"));
    }
}

/// FR-90 P8-2d-0 — a device's call and its answer cross the channel as
/// themselves: their own frames, never read as a session worker's.
#[cfg(worker_calls)]
#[test]
fn a_device_call_crosses_as_its_own_frame() {
    use crate::hive::worker::{Call, Msg};

    let asked = Asked {
        id: id(),
        kind: "tls".into(),
        host: "db.internal".into(),
        port: 5432,
        sni: Some("db.internal".into()),
    };
    let call = Msg::DeviceCall {
        id: 7,
        call: DeviceCall::Measure {
            asked,
            rules: probe_reach::rules(&["db.internal:5432".to_string()]),
        },
    };
    let json = serde_json::to_value(&call).unwrap();
    assert_eq!(json["t"], "device_call");
    assert_eq!(json["call"]["d"], "measure");
    assert_eq!(serde_json::from_value::<Msg>(json.clone()).unwrap(), call);
    assert!(
        serde_json::from_value::<Call>(json["call"].clone()).is_err(),
        "a device's call is no session worker's"
    );

    for answer in [
        DeviceAnswer::Probed {
            probed: Probed {
                outcome: ProbeOutcome::Unreachable,
                rtt_ms: None,
                address: Some("10.0.0.1".parse().unwrap()),
                tls: None,
                detail: Some("no answer in time".into()),
            },
        },
        DeviceAnswer::Refused {
            why: "not a call this worker takes".into(),
        },
    ] {
        let frame = Msg::DeviceAnswer { id: 7, answer };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["t"], "device_answer");
        assert_eq!(serde_json::from_value::<Msg>(json).unwrap(), frame);
    }
}

/// FR-90 P8-2d-0 — a device's link carries the device's calls end to end:
/// the call framed as the device's, answered by its id with the device's
/// answer, over a real byte channel. Here the far side measures in process,
/// as P8-2d-1's worker will measure in its own.
#[cfg(worker_calls)]
#[tokio::test]
async fn a_device_link_carries_a_measure_and_its_answer() {
    use crate::hive::worker::link::DeviceLink;
    use crate::hive::worker::{Msg, recv};

    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (our_rd, our_wr) = tokio::io::split(ours);
    let (their_rd, their_wr) = tokio::io::split(theirs);
    let daemon = Arc::new(DeviceLink::worker(our_wr));
    let reading = Arc::clone(&daemon);
    tokio::spawn(async move {
        let mut rd = our_rd;
        while let Ok(Some(m)) = recv(&mut rd).await {
            if let Msg::DeviceAnswer { id, answer } = m {
                reading.answered(id, answer);
            }
        }
        reading.end();
    });
    let worker = Arc::new(DeviceLink::worker(their_wr));
    tokio::spawn(async move {
        let mut rd = their_rd;
        while let Ok(Some(m)) = recv(&mut rd).await {
            if let Msg::DeviceCall {
                id,
                call: DeviceCall::Measure { asked, rules },
            } = m
            {
                let probed = InDaemon.measure(asked, rules).await;
                let _ = worker.answer(id, DeviceAnswer::Probed { probed }).await;
            }
        }
    });

    // Refused before any connect: the answer is the probe's own, and no
    // network is needed to have it.
    let asked = Asked {
        id: id(),
        kind: "tcp".into(),
        host: "127.0.0.1".into(),
        port: 9,
        sni: None,
    };
    let call = DeviceCall::Measure {
        asked: asked.clone(),
        rules: Vec::new(),
    };
    #[cfg(unix)]
    let answer = daemon.call(call, Vec::new()).await.unwrap();
    #[cfg(windows)]
    let answer = daemon.call(call).await.unwrap();
    let expected = crate::hive::knowhow_probe::probe(&asked, &[]).await;
    assert_eq!(expected.outcome, ProbeOutcome::NotAllowed);
    assert_eq!(answer, DeviceAnswer::Probed { probed: expected });
}
