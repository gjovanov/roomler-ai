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

/// roomler patch (FR-148 P0): a transport whose slot holds the client of an
/// association pair handshaken over an in-memory pipe, which binds no socket,
/// and the pair.
async fn transport_over_a_pipe() -> Result<(RTCSctpTransport, Arc<Association>, Association)> {
    let (client_conn, server_conn) = util::conn::conn_pipe::pipe();
    let server = tokio::spawn(async move {
        Association::server(sctp::association::Config {
            net_conn: Arc::new(server_conn),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "server".to_owned(),
        })
        .await
    });
    let client = Association::client(sctp::association::Config {
        net_conn: Arc::new(client_conn),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    })
    .await?;
    let server = server.await.expect("the server's task ends")?;
    let client = Arc::new(client);
    let transport = RTCSctpTransport {
        sctp_association: Mutex::new(Some(Arc::clone(&client))),
        ..Default::default()
    };
    Ok((transport, client, server))
}

/// roomler patch (FR-148 P0): `call`, asked again each millisecond, a second
/// at most, while the association's own loops hold its lock (`Busy`).
async fn past_busy<T>(
    mut call: impl FnMut() -> std::result::Result<T, sctp::association::windows::RestoreRefusal>,
) -> std::result::Result<T, sctp::association::windows::RestoreRefusal> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    loop {
        match call() {
            Err(sctp::association::windows::RestoreRefusal::Busy)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            answer => return answer,
        }
    }
}

/// roomler patch (FR-148 P0): before the transport has an association, the
/// pacer's switch and the window handle answer `None`.
#[test]
fn test_pacer_and_windows_answer_none_before_the_association() {
    let idle = RTCSctpTransport::default();
    assert!(idle.pacer().is_none(), "a pacer with no association");
    assert!(idle.windows().is_none(), "windows with no association");
}

/// roomler patch (FR-148 P0): while the association's slot is held, as
/// `stop()` holds it across the association's close, both answer `None`, at
/// once: five times each, and the fastest of each five in under a
/// millisecond. Five, because a loaded runner can deschedule one call for a
/// time slice mid-call: a call that waits or spins for the slot is slow all
/// five times, and one preemption slows one.
#[tokio::test]
async fn test_pacer_and_windows_answer_none_while_stop_holds_the_slot() -> Result<()> {
    let (transport, client, server) = transport_over_a_pipe().await?;
    let free = (transport.pacer().is_some(), transport.windows().is_some());
    let (pacers, windows) = {
        let _held = transport.sctp_association.lock().await;
        let mut pacers = Vec::new();
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let pacer = transport.pacer();
            pacers.push((pacer.is_none(), t.elapsed()));
        }
        let mut windows = Vec::new();
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let handle = transport.windows();
            windows.push((handle.is_none(), t.elapsed()));
        }
        (pacers, windows)
    };
    client.close().await?;
    server.close().await?;
    let pacer_took: Vec<Duration> = pacers.iter().map(|&(_, took)| took).collect();
    let windows_took: Vec<Duration> = windows.iter().map(|&(_, took)| took).collect();
    println!(
        "FR-148 P0: with the slot held, five pacer() calls took {pacer_took:?}, five windows() \
         calls {windows_took:?}"
    );
    assert_eq!(free, (true, true), "a free slot reaches the association");
    assert!(
        pacers.iter().all(|&(none, _)| none),
        "a held slot answered with a pacer"
    );
    assert!(
        windows.iter().all(|&(none, _)| none),
        "a held slot answered with windows"
    );
    let fastest = |took: &[Duration]| *took.iter().min().expect("five calls");
    assert!(
        fastest(&pacer_took) < Duration::from_millis(1),
        "the fastest of five pacer() calls took {:?}: {pacer_took:?}",
        fastest(&pacer_took)
    );
    assert!(
        fastest(&windows_took) < Duration::from_millis(1),
        "the fastest of five windows() calls took {:?}: {windows_took:?}",
        fastest(&windows_took)
    );
    Ok(())
}

/// roomler patch (FR-148 P0): the pacer's switch the transport reaches is the
/// association's own: `PaceSwitch::same` holds against it, and not against the
/// peer's, and turned on through the transport the association's pacer is on.
#[tokio::test]
async fn test_the_pacer_reached_is_the_associations() -> Result<()> {
    let (transport, client, server) = transport_over_a_pipe().await?;
    let pacer = transport.pacer().expect("the slot holds the association");
    let same = pacer.same(&client.pacer());
    let other = pacer.same(&server.pacer());
    pacer.set(true);
    let on = client.pacer().is_on();
    pacer.set(false);
    client.close().await?;
    server.close().await?;
    assert!(same, "the transport's switch is not the association's");
    assert!(!other, "the transport's switch is the peer's");
    assert!(
        on,
        "turned on through the transport, the association's pacer is off"
    );
    Ok(())
}

/// roomler patch (FR-148 P0): the window handle the transport reaches is the
/// association's own: a lowering through it moves the association's window,
/// as the association's own handle and its gauges read it.
#[tokio::test]
async fn test_the_windows_reached_are_the_associations() -> Result<()> {
    let (transport, client, server) = transport_over_a_pipe().await?;
    let handle = transport.windows().expect("the slot holds the association");
    let before = client.windows().snapshot().await;
    let to = before.map(|w| sctp::association::windows::Windows {
        cwnd: w.cwnd / 2,
        ..w
    });
    let lowered = match &to {
        Some(to) => Some(past_busy(|| handle.try_lower(to)).await),
        None => None,
    };
    let after = client.windows().snapshot().await;
    let gauged = client.gauges().peek().cwnd;
    client.close().await?;
    server.close().await?;
    let to = to.expect("the association's own handle took a snapshot");
    let lowered = lowered.expect("a lowering was asked");
    assert_eq!(lowered.map(|l| l.after), Ok(to.cwnd));
    assert_eq!(after, Some(to), "the association's window did not move");
    assert_eq!(
        gauged, to.cwnd,
        "the association's gauges read another window"
    );
    Ok(())
}
