use portable_atomic::AtomicU16;
use tokio::sync::mpsc;
use tokio::time::Duration;

use super::*;
use crate::api::media_engine::MediaEngine;
use crate::api::APIBuilder;
use crate::data_channel::data_channel_init::RTCDataChannelInit;
use crate::peer_connection::peer_connection_test::{close_pair_now, new_pair, signal_pair};

#[tokio::test]
async fn test_generate_data_channel_id() -> Result<()> {
    let sctp_transport_with_channels = |ids: &[u16]| -> RTCSctpTransport {
        let mut data_channels = vec![];
        for id in ids {
            data_channels.push(Arc::new(RTCDataChannel {
                id: AtomicU16::new(*id),
                ..Default::default()
            }));
        }

        RTCSctpTransport {
            data_channels: Arc::new(Mutex::new(data_channels)),
            ..Default::default()
        }
    };

    let tests = vec![
        (DTLSRole::Client, sctp_transport_with_channels(&[]), 0),
        (DTLSRole::Client, sctp_transport_with_channels(&[1]), 0),
        (DTLSRole::Client, sctp_transport_with_channels(&[0]), 2),
        (DTLSRole::Client, sctp_transport_with_channels(&[0, 2]), 4),
        (DTLSRole::Client, sctp_transport_with_channels(&[0, 4]), 2),
        (DTLSRole::Server, sctp_transport_with_channels(&[]), 1),
        (DTLSRole::Server, sctp_transport_with_channels(&[0]), 1),
        (DTLSRole::Server, sctp_transport_with_channels(&[1]), 3),
        (DTLSRole::Server, sctp_transport_with_channels(&[1, 3]), 5),
        (DTLSRole::Server, sctp_transport_with_channels(&[1, 5]), 3),
    ];

    for (role, s, expected) in tests {
        match s.generate_and_set_data_channel_id(role).await {
            Ok(actual) => assert_eq!(actual, expected),
            Err(err) => panic!("failed to generate id: {err}"),
        };
    }

    Ok(())
}

/// roomler patch (FR-148 P4b): a refused mark says why, each cause on its
/// own: no association before the transport starts and after it stops, its
/// slot held, no such stream. And an accepted channel's id marks the stream
/// under it, and no other.
#[tokio::test]
async fn test_mark_deadline_stream_says_why_it_refused() -> Result<()> {
    let idle = RTCSctpTransport::default();
    assert_eq!(
        idle.mark_deadline_stream(0).await,
        Err(DeadlineMarkRefusal::NoAssociation),
        "not started"
    );
    assert_eq!(idle.deadline_streams().await, None);

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let api = APIBuilder::new().with_media_engine(m).build();
    let (mut offer_pc, mut answer_pc) = new_pair(&api).await?;
    let init = RTCDataChannelInit {
        ordered: Some(false),
        max_retransmits: Some(0),
        ..Default::default()
    };
    offer_pc
        .create_data_channel("video-deadline", Some(init))
        .await?;
    let (open_tx, mut open_rx) = mpsc::channel::<Arc<RTCDataChannel>>(4);
    answer_pc.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
        let open_tx = open_tx.clone();
        let opened = Arc::clone(&d);
        d.on_open(Box::new(move || {
            Box::pin(async move {
                let _ = open_tx.send(opened).await;
            })
        }));
        Box::pin(async {})
    }));
    signal_pair(&mut offer_pc, &mut answer_pc).await?;
    // `signal_pair`'s own channel is accepted too.
    let mut accepted = None;
    for _ in 0..2 {
        let dc = tokio::time::timeout(Duration::from_secs(10), open_rx.recv())
            .await
            .expect("both channels open within 10 s")
            .expect("the handler holds a sender");
        if dc.label() == "video-deadline" {
            accepted = Some(dc);
        }
    }
    let stream = accepted.expect("video-deadline was accepted").id();
    let sctp = answer_pc.sctp();
    {
        let _held = sctp.sctp_association.lock().await;
        assert_eq!(
            sctp.mark_deadline_stream(stream).await,
            Err(DeadlineMarkRefusal::SlotHeld)
        );
        assert_eq!(sctp.deadline_streams().await, None);
    }
    let absent = 999;
    assert_ne!(stream, absent);
    assert_eq!(
        sctp.mark_deadline_stream(absent).await,
        Err(DeadlineMarkRefusal::NoStream)
    );
    assert_eq!(
        sctp.deadline_streams().await,
        Some(vec![]),
        "a refusal marks nothing"
    );
    assert_eq!(sctp.mark_deadline_stream(stream).await, Ok(()));
    assert_eq!(
        sctp.deadline_streams().await,
        Some(vec![stream]),
        "the accepted channel's stream, and no other"
    );
    close_pair_now(&offer_pc, &answer_pc).await;
    assert_eq!(
        sctp.mark_deadline_stream(stream).await,
        Err(DeadlineMarkRefusal::NoAssociation),
        "stopped"
    );
    Ok(())
}

/// roomler patch (FR-148 P4b): the refusals' words, as the agent's line
/// writes them.
#[test]
fn test_deadline_mark_refusal_words() {
    let words: Vec<String> = [
        DeadlineMarkRefusal::NoAssociation,
        DeadlineMarkRefusal::SlotHeld,
        DeadlineMarkRefusal::NoStream,
    ]
    .iter()
    .map(|r| r.to_string())
    .collect();
    assert_eq!(words, ["no_association", "slot_held", "no_stream"]);
}
