use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use portable_atomic::AtomicU32;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

fn create_test_stream() -> Stream {
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    Stream::new(
        "client".to_owned(),
        0,
        0,
        Arc::new(AtomicU32::default()),
        Arc::new(AtomicU8::default()),
        Arc::new(awake_write_loop_ch_tx),
        Arc::new(PendingQueue::default()),
    )
}

#[test]
fn test_stream_buffered_amount() -> Result<()> {
    let s = create_test_stream();

    assert_eq!(s.buffered_amount(), 0);
    assert_eq!(s.buffered_amount_low_threshold(), 0);

    s.buffered_amount.store(8192, Ordering::SeqCst);
    s.set_buffered_amount_low_threshold(2048);
    assert_eq!(s.buffered_amount(), 8192, "unexpected bufferedAmount");
    assert_eq!(
        s.buffered_amount_low_threshold(),
        2048,
        "unexpected threshold"
    );

    Ok(())
}

#[tokio::test]
async fn test_stream_amount_on_buffered_amount_low() -> Result<()> {
    let s = create_test_stream();

    s.buffered_amount.store(4096, Ordering::SeqCst);
    s.set_buffered_amount_low_threshold(2048);

    let n_cbs = Arc::new(AtomicU32::new(0));
    let n_cbs2 = n_cbs.clone();

    s.on_buffered_amount_low(Box::new(move || {
        n_cbs2.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }));

    // Negative value should be ignored (by design)
    s.on_buffer_released(-32).await; // bufferedAmount = 3072
    assert_eq!(s.buffered_amount(), 4096, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 0, "callback count mismatch");

    // Above to above, no callback
    s.on_buffer_released(1024).await; // bufferedAmount = 3072
    assert_eq!(s.buffered_amount(), 3072, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 0, "callback count mismatch");

    // Above to equal, callback should be made
    s.on_buffer_released(1024).await; // bufferedAmount = 2048
    assert_eq!(s.buffered_amount(), 2048, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 1, "callback count mismatch");

    // Eaual to below, no callback
    s.on_buffer_released(1024).await; // bufferedAmount = 1024
    assert_eq!(s.buffered_amount(), 1024, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 1, "callback count mismatch");

    // Blow to below, no callback
    s.on_buffer_released(1024).await; // bufferedAmount = 0
    assert_eq!(s.buffered_amount(), 0, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 1, "callback count mismatch");

    // Capped at 0, no callback
    s.on_buffer_released(1024).await; // bufferedAmount = 0
    assert_eq!(s.buffered_amount(), 0, "unexpected bufferedAmount");
    assert_eq!(n_cbs.load(Ordering::SeqCst), 1, "callback count mismatch");

    Ok(())
}

#[tokio::test]
async fn test_stream() -> std::result::Result<(), io::Error> {
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    let s = Stream::new(
        "test_poll_stream".to_owned(),
        0,
        4096,
        Arc::new(AtomicU32::new(4096)),
        Arc::new(AtomicU8::new(AssociationState::Established as u8)),
        Arc::new(awake_write_loop_ch_tx),
        Arc::new(PendingQueue::new()),
    );

    // getters
    assert_eq!(s.stream_identifier(), 0);
    assert_eq!(s.buffered_amount(), 0);
    assert_eq!(s.buffered_amount_low_threshold(), 0);
    assert_eq!(s.get_num_bytes_in_reassembly_queue().await, 0);

    // setters
    s.set_default_payload_type(PayloadProtocolIdentifier::Binary);
    s.set_reliability_params(true, ReliabilityType::Reliable, 0);

    // write
    let n = s.write(&Bytes::from("Hello ")).await?;
    assert_eq!(n, 6);
    assert_eq!(s.buffered_amount(), 6);
    let n = s
        .write_sctp(&Bytes::from("world"), PayloadProtocolIdentifier::Binary)
        .await?;
    assert_eq!(n, 5);
    assert_eq!(s.buffered_amount(), 11);

    // async read
    //  1. pretend that we've received a chunk
    s.handle_data(ChunkPayloadData {
        unordered: true,
        beginning_fragment: true,
        ending_fragment: true,
        user_data: Bytes::from_static(&[0, 1, 2, 3, 4]),
        payload_type: PayloadProtocolIdentifier::Binary,
        ..Default::default()
    })
    .await;
    //  2. read it
    let mut buf = [0; 5];
    s.read(&mut buf).await?;
    assert_eq!(buf, [0, 1, 2, 3, 4]);

    // shutdown write
    s.shutdown(Shutdown::Write).await?;
    // write must fail
    assert!(s.write(&Bytes::from("error")).await.is_err());
    // read should continue working
    s.handle_data(ChunkPayloadData {
        unordered: true,
        beginning_fragment: true,
        ending_fragment: true,
        user_data: Bytes::from_static(&[5, 6, 7, 8, 9]),
        payload_type: PayloadProtocolIdentifier::Binary,
        ..Default::default()
    })
    .await;
    let mut buf = [0; 5];
    s.read(&mut buf).await?;
    assert_eq!(buf, [5, 6, 7, 8, 9]);

    // shutdown read
    s.shutdown(Shutdown::Read).await?;
    // read must return 0
    assert_eq!(s.read(&mut buf).await, Ok(0));

    Ok(())
}

#[tokio::test]
async fn test_poll_stream() -> std::result::Result<(), io::Error> {
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    let s = Arc::new(Stream::new(
        "test_poll_stream".to_owned(),
        0,
        4096,
        Arc::new(AtomicU32::new(4096)),
        Arc::new(AtomicU8::new(AssociationState::Established as u8)),
        Arc::new(awake_write_loop_ch_tx),
        Arc::new(PendingQueue::new()),
    ));
    let mut poll_stream = PollStream::new(s.clone());

    // getters
    assert_eq!(poll_stream.stream_identifier(), 0);
    assert_eq!(poll_stream.buffered_amount(), 0);
    assert_eq!(poll_stream.buffered_amount_low_threshold(), 0);
    assert_eq!(poll_stream.get_num_bytes_in_reassembly_queue().await, 0);

    // async write
    let n = poll_stream.write(&[1, 2, 3]).await?;
    assert_eq!(n, 3);
    poll_stream.flush().await?;
    assert_eq!(poll_stream.buffered_amount(), 3);

    // async read
    //  1. pretend that we've received a chunk
    let sc = s.clone();
    sc.handle_data(ChunkPayloadData {
        unordered: true,
        beginning_fragment: true,
        ending_fragment: true,
        user_data: Bytes::from_static(&[0, 1, 2, 3, 4]),
        payload_type: PayloadProtocolIdentifier::Binary,
        ..Default::default()
    })
    .await;
    //  2. read it
    let mut buf = [0; 5];
    poll_stream.read_exact(&mut buf).await?;
    assert_eq!(buf, [0, 1, 2, 3, 4]);

    // shutdown write
    poll_stream.shutdown().await?;
    // write must fail
    assert!(poll_stream.write(&[1, 2, 3]).await.is_err());
    // read should continue working
    sc.handle_data(ChunkPayloadData {
        unordered: true,
        beginning_fragment: true,
        ending_fragment: true,
        user_data: Bytes::from_static(&[5, 6, 7, 8, 9]),
        payload_type: PayloadProtocolIdentifier::Binary,
        ..Default::default()
    })
    .await;
    let mut buf = [0; 5];
    poll_stream.read_exact(&mut buf).await?;
    assert_eq!(buf, [5, 6, 7, 8, 9]);

    // misc.
    let clone = poll_stream.clone();
    assert_eq!(clone.stream_identifier(), poll_stream.stream_identifier());

    Ok(())
}

/// roomler (#3716): a write that passed its shutdown check holds its stream's
/// `write_turn` until its chunks are queued. The stream's reset waits for that
/// turn on a task of its own: the shutdown returns at once and queues nothing
/// yet, and the reset goes in right behind the write's data, never ahead of
/// it. One thread, so the reset's task runs only where the test yields: the
/// order is the code's, not a race's.
#[tokio::test(flavor = "current_thread")]
async fn test_stream_reset_waits_for_a_write_turn() -> Result<()> {
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    let s = Stream::new(
        "test_stream_reset_waits_for_a_write_turn".to_owned(),
        0,
        4096,
        Arc::new(AtomicU32::new(4096)),
        Arc::new(AtomicU8::new(AssociationState::Established as u8)),
        Arc::new(awake_write_loop_ch_tx),
        Arc::new(PendingQueue::new()),
    );

    // A write passed its shutdown check and holds its turn; its chunks are
    // not queued yet.
    let turn = s.write_turn.lock().await;

    // The channel closes, and its shutdown does not wait for that write.
    let mut shutdown = Box::pin(s.shutdown(Shutdown::Both));
    let polled = std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx))).await;
    assert!(
        matches!(polled, Poll::Ready(Ok(()))),
        "the shutdown waited for a write's turn: {polled:?}"
    );
    assert!(
        s.pending_queue.is_empty(),
        "the reset was queued while a write that passed its shutdown check held its turn"
    );

    // Everything else on the runtime runs, the reset's task with it, while the
    // write packetizes and queues its message. Then it gives up its turn.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    let data = Bytes::from_static(b"the last frame");
    // roomler (#3755): `append` takes the write loop's wake.
    assert!(
        s.pending_queue
            .append(
                s.packetize(&data, PayloadProtocolIdentifier::Binary),
                &s.awake_write_loop_ch
            )
            .await
    );
    drop(turn);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while s.pending_queue.len() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let mut queued = Vec::new();
    while let Some(c) = s.pending_queue.pop(true, false) {
        queued.push(c.user_data);
    }
    assert_eq!(
        queued,
        [data, Bytes::new()],
        "the queue must end as [the write's data, the zero-length reset]"
    );

    Ok(())
}

/// Polls `fut` until it has waited `waits` times and drops it there: a cut at a
/// chosen await, the way a timeout, a `select!` arm or a task's abort cuts
/// one. `Some` when it finished first.
async fn cut_at_wait<F: Future>(fut: F, waits: usize) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut left = waits;
    std::future::poll_fn(move |cx| match fut.as_mut().poll(cx) {
        Poll::Ready(out) => Poll::Ready(Some(out)),
        Poll::Pending if left <= 1 => Poll::Ready(None),
        Poll::Pending => {
            left -= 1;
            Poll::Pending
        }
    })
    .await
}

/// What a write may leave behind: its stream's next sequence number and
/// `buffered_amount`, the queue's chunks and bytes, and its room.
fn trace_of(s: &Stream, queue: &PendingQueue) -> (u16, usize, usize, usize, usize) {
    (
        s.sequence_number.load(Ordering::SeqCst),
        s.buffered_amount(),
        queue.len(),
        queue.get_num_bytes(),
        queue.room(),
    )
}

/// Takes everything out of `queue` the way the association's write loop does.
fn drain(queue: &PendingQueue) -> Vec<ChunkPayloadData> {
    let mut out = Vec::new();
    while let Some(c) = queue.peek() {
        match queue.pop(c.beginning_fragment, c.unordered) {
            Some(c) => out.push(c),
            None => break,
        }
    }
    out
}

/// roomler (#3755): at each await of `write_sctp`, a write dropped there is
/// one never made: its stream's sequence number and `buffered_amount`, and the
/// queue, are as they were before it. Its awaits, in order: the stream's turn;
/// the queue's lock, which a large write of another stream holds while it
/// waits for room part way; room for a small message; room for the first
/// chunk of a large one. Dropped after the first chunk of a large message was
/// queued, the message is begun: it is finished whole, in its place. No write
/// loop runs here; the test takes chunks out of the queue itself.
#[tokio::test(flavor = "current_thread")]
async fn test_stream_a_write_dropped_at_any_await_leaves_no_trace() -> Result<()> {
    // A message of 2730 bytes or more is queued chunk by chunk.
    const QUEUE: usize = 4096;
    let queue = Arc::new(PendingQueue::with_limit(QUEUE));
    let (awake_tx, _awake_rx) = mpsc::channel(1);
    let awake = Arc::new(awake_tx);
    let stream = |si: u16| {
        Arc::new(Stream::new(
            format!("test_stream_a_write_dropped_at_any_await_leaves_no_trace/{si}"),
            si,
            1200,
            Arc::new(AtomicU32::new(65536)),
            Arc::new(AtomicU8::new(AssociationState::Established as u8)),
            Arc::clone(&awake),
            Arc::clone(&queue),
        ))
    };
    let s = stream(1);
    let other = stream(2);
    let small = Bytes::from(vec![b's'; 1500]);
    let mut findings: Vec<String> = Vec::new();

    // 1. Its turn: another write of the stream holds it.
    let before = trace_of(&s, &queue);
    {
        let _turn = s.write_turn.lock().await;
        let cut = cut_at_wait(s.write_sctp(&small, PayloadProtocolIdentifier::Binary), 1).await;
        assert!(cut.is_none(), "the write never waited for its turn");
    }
    let after = trace_of(&s, &queue);
    if after != before {
        findings.push(format!(
            "dropped waiting for its turn: {before:?} became {after:?}"
        ));
    }

    // 2. The queue's lock: a large write of another stream holds it, part way,
    // waiting for room that only the write loop makes.
    let larger_than_the_queue = Bytes::from(vec![b'o'; 6000]);
    let parked = tokio::spawn({
        let other = Arc::clone(&other);
        let m = larger_than_the_queue.clone();
        async move {
            other
                .write_sctp(&m, PayloadProtocolIdentifier::Binary)
                .await
        }
    });
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(
        !parked.is_finished() && queue.room() < 1200,
        "the other stream's write must wait part way, holding the queue's lock"
    );
    let before = trace_of(&s, &queue);
    let cut = cut_at_wait(s.write_sctp(&small, PayloadProtocolIdentifier::Binary), 1).await;
    assert!(cut.is_none(), "the write never waited for the queue's lock");
    let after = trace_of(&s, &queue);
    if after != before {
        findings.push(format!(
            "dropped waiting for the queue's lock: {before:?} became {after:?}"
        ));
    }
    while !parked.is_finished() {
        drain(&queue);
        tokio::task::yield_now().await;
    }
    parked.await.expect("the other stream's write")?;
    drain(&queue);

    // 3. Room for a small message: the other stream filled the queue.
    let fill = Bytes::from(vec![b'f'; 1000]);
    while queue.room() >= fill.len() {
        other
            .write_sctp(&fill, PayloadProtocolIdentifier::Binary)
            .await?;
    }
    let before = trace_of(&s, &queue);
    let cut = cut_at_wait(s.write_sctp(&small, PayloadProtocolIdentifier::Binary), 1).await;
    assert!(cut.is_none(), "the write never waited for room");
    let after = trace_of(&s, &queue);
    if after != before {
        findings.push(format!(
            "dropped waiting for room for a small message: {before:?} became {after:?}"
        ));
    }

    // 4. Room for the first chunk of a large one, in the same full queue.
    let large = Bytes::from(vec![b'l'; 3000]);
    let before = trace_of(&s, &queue);
    let cut = cut_at_wait(s.write_sctp(&large, PayloadProtocolIdentifier::Binary), 1).await;
    assert!(cut.is_none(), "the large write never waited for room");
    let after = trace_of(&s, &queue);
    if after != before {
        findings.push(format!(
            "dropped waiting for room for the first chunk of a large message: {before:?} became {after:?}"
        ));
    }

    // 5. Part way: room for its first chunk only. The message is begun, and
    // finished whole once there is room; the next message of the stream comes
    // after it, numbered next.
    drain(&queue);
    while queue.room() >= 2 * 1200 {
        other
            .write_sctp(&fill, PayloadProtocolIdentifier::Binary)
            .await?;
    }
    assert!(queue.room() >= 1200, "room for the first chunk only");
    let filler = queue.len();
    let ssn = s.sequence_number.load(Ordering::SeqCst);
    let cut = cut_at_wait(s.write_sctp(&large, PayloadProtocolIdentifier::Binary), 1).await;
    assert!(cut.is_none(), "the large write never waited part way");
    assert!(
        queue.len() > filler,
        "the large write must be cut part way, its first chunk queued"
    );
    let next = Bytes::from_static(b"next");
    let writer = tokio::spawn({
        let s = Arc::clone(&s);
        async move { s.write_sctp(&next, PayloadProtocolIdentifier::Binary).await }
    });
    let mut out = Vec::new();
    for _ in 0..100 {
        out.extend(drain(&queue));
        if writer.is_finished() && queue.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    if !writer.is_finished() {
        findings.push("the next write of the stream never got into the queue".to_owned());
    }
    let mine: Vec<&ChunkPayloadData> = out.iter().filter(|c| c.stream_identifier == 1).collect();
    let large_chunks: Vec<&&ChunkPayloadData> = mine
        .iter()
        .filter(|c| c.stream_sequence_number == ssn)
        .collect();
    let large_bytes: usize = large_chunks.iter().map(|c| c.user_data.len()).sum();
    let whole = large_bytes == large.len()
        && large_chunks.first().is_some_and(|c| c.beginning_fragment)
        && large_chunks.last().is_some_and(|c| c.ending_fragment);
    if !whole {
        findings.push(format!(
            "the large message cut part way went out as {} of its {} bytes",
            large_bytes,
            large.len()
        ));
    }
    let after_large: Vec<u16> = mine
        .iter()
        .filter(|c| c.stream_sequence_number != ssn)
        .map(|c| c.stream_sequence_number)
        .collect();
    if after_large != [ssn.wrapping_add(1)] {
        findings.push(format!(
            "after the large message (sequence number {ssn}) the stream sent {after_large:?}, \
             not the next message numbered {}",
            ssn.wrapping_add(1)
        ));
    }

    assert!(
        findings.is_empty(),
        "#3755, a write dropped at its awaits:\n  - {}",
        findings.join("\n  - ")
    );
    Ok(())
}
