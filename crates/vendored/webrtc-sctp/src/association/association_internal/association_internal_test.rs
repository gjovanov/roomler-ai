use std::io;
use std::net::SocketAddr;

use super::*;

type Result<T> = std::result::Result<T, util::Error>;

impl From<Error> for util::Error {
    fn from(e: Error) -> Self {
        util::Error::from_std(e)
    }
}

struct DumbConn;

#[async_trait]
impl Conn for DumbConn {
    async fn connect(&self, _addr: SocketAddr) -> Result<()> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    async fn recv(&self, _b: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    async fn recv_from(&self, _buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    async fn send(&self, _b: &[u8]) -> Result<usize> {
        Ok(0)
    }

    async fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> Result<usize> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "Addr Not Available").into())
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn create_association_internal(config: Config) -> AssociationInternal {
    let (close_loop_ch_tx, _close_loop_ch_rx) = broadcast::channel(1);
    let (accept_ch_tx, _accept_ch_rx) = mpsc::channel(1);
    let (handshake_completed_ch_tx, _handshake_completed_ch_rx) = mpsc::channel(1);
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    AssociationInternal::new(
        config,
        close_loop_ch_tx,
        accept_ch_tx,
        handshake_completed_ch_tx,
        Arc::new(awake_write_loop_ch_tx),
    )
}

#[test]
fn test_create_forward_tsn_forward_one_abandoned() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 10;
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 10,
        stream_identifier: 1,
        stream_sequence_number: 2,
        user_data: Bytes::from_static(b"ABC"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(fwdtsn.new_cumulative_tsn, 10, "should be able to serialize");
    assert_eq!(fwdtsn.streams.len(), 1, "there should be one stream");
    assert_eq!(fwdtsn.streams[0].identifier, 1, "si should be 1");
    assert_eq!(fwdtsn.streams[0].sequence, 2, "ssn should be 2");

    Ok(())
}

#[test]
fn test_create_forward_tsn_forward_two_abandoned_with_the_same_si() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 12;
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 10,
        stream_identifier: 1,
        stream_sequence_number: 2,
        user_data: Bytes::from_static(b"ABC"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 11,
        stream_identifier: 1,
        stream_sequence_number: 3,
        user_data: Bytes::from_static(b"DEF"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 12,
        stream_identifier: 2,
        stream_sequence_number: 1,
        user_data: Bytes::from_static(b"123"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(fwdtsn.new_cumulative_tsn, 12, "should be able to serialize");
    assert_eq!(fwdtsn.streams.len(), 2, "there should be two stream");

    let mut si1ok = false;
    let mut si2ok = false;
    for s in &fwdtsn.streams {
        match s.identifier {
            1 => {
                assert_eq!(3, s.sequence, "ssn should be 3");
                si1ok = true;
            }
            2 => {
                assert_eq!(1, s.sequence, "ssn should be 1");
                si2ok = true;
            }
            _ => panic!("unexpected stream identifier"),
        }
    }
    assert!(si1ok, "si=1 should be present");
    assert!(si2ok, "si=2 should be present");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_3unreceived_chunks() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 3,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 0,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 3,
        "peerLastTSN should advance by 3 "
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_1for1_missing() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 2,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 2,
        "peerLastTSN should advance by 2"
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_1for2_missing() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 3,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 1,
        "peerLastTSN should advance by 1"
    );
    assert!(
        immediate_ack_triggered,
        "immediate sack should be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_dup_forward_tsn_chunk_should_generate_sack() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    assert_eq!(a.peer_last_tsn, prev_tsn, "peerLastTSN should not advance");
    assert_eq!(a.ack_state, AckState::Immediate, "sack should be requested");
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_assoc_create_new_stream() -> Result<()> {
    let (close_loop_ch_tx, _close_loop_ch_rx) = broadcast::channel(1);
    let (accept_ch_tx, _accept_ch_rx) = mpsc::channel(ACCEPT_CH_SIZE);
    let (handshake_completed_ch_tx, _handshake_completed_ch_rx) = mpsc::channel(1);
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    let mut a = AssociationInternal::new(
        Config {
            net_conn: Arc::new(DumbConn {}),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "client".to_owned(),
        },
        close_loop_ch_tx,
        accept_ch_tx,
        handshake_completed_ch_tx,
        Arc::new(awake_write_loop_ch_tx),
    );

    for i in 0..ACCEPT_CH_SIZE {
        let s = a.create_stream(i as u16, true);
        if let Some(s) = s {
            let result = a.streams.get(&s.stream_identifier);
            assert!(result.is_some(), "should be in a.streams map");
        } else {
            panic!("{i} should success");
        }
    }

    let new_si = ACCEPT_CH_SIZE as u16;
    let s = a.create_stream(new_si, true);
    assert!(s.is_none(), "should be none");
    let result = a.streams.get(&new_si);
    assert!(result.is_none(), "should NOT be in a.streams map");

    let to_be_ignored = ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: a.peer_last_tsn + 1,
        stream_identifier: new_si,
        user_data: Bytes::from_static(b"ABC"),
        ..Default::default()
    };

    let p = a.handle_data(&to_be_ignored).await?;
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

async fn handle_init_test(name: &str, initial_state: AssociationState, expect_err: bool) {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.set_state(initial_state);
    let pkt = Packet {
        source_port: 5001,
        destination_port: 5002,
        ..Default::default()
    };
    let mut init = ChunkInit {
        initial_tsn: 1234,
        num_outbound_streams: 1001,
        num_inbound_streams: 1002,
        initiate_tag: 5678,
        advertised_receiver_window_credit: 512 * 1024,
        ..Default::default()
    };
    init.set_supported_extensions();

    let result = a.handle_init(&pkt, &init).await;
    if expect_err {
        assert!(result.is_err(), "{name} should fail");
        return;
    } else {
        assert!(result.is_ok(), "{name} should be ok");
    }
    assert_eq!(
        a.peer_last_tsn,
        if init.initial_tsn == 0 {
            u32::MAX
        } else {
            init.initial_tsn - 1
        },
        "{name} should match"
    );
    assert_eq!(a.my_max_num_outbound_streams, 1001, "{name} should match");
    assert_eq!(a.my_max_num_inbound_streams, 1002, "{name} should match");
    assert_eq!(a.peer_verification_tag, 5678, "{name} should match");
    assert_eq!(a.destination_port, pkt.source_port, "{name} should match");
    assert_eq!(a.source_port, pkt.destination_port, "{name} should match");
    assert!(a.use_forward_tsn, "{name} should be set to true");
}

#[tokio::test]
async fn test_assoc_handle_init() -> Result<()> {
    handle_init_test("normal", AssociationState::Closed, false).await;

    handle_init_test(
        "unexpected state established",
        AssociationState::Established,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownAckSent",
        AssociationState::ShutdownAckSent,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownPending",
        AssociationState::ShutdownPending,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownReceived",
        AssociationState::ShutdownReceived,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownSent",
        AssociationState::ShutdownSent,
        true,
    )
    .await;

    Ok(())
}

#[tokio::test]
async fn test_assoc_max_message_size_default() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    assert_eq!(
        a.max_message_size.load(Ordering::SeqCst),
        65536,
        "should match"
    );

    let stream = a.create_stream(1, false);
    assert!(stream.is_some(), "should succeed");

    if let Some(s) = stream {
        let p = Bytes::from(vec![0u8; 65537]);
        let ppi = PayloadProtocolIdentifier::from(s.default_payload_type.load(Ordering::SeqCst));

        if let Err(err) = s.write_sctp(&p.slice(..65536), ppi).await {
            assert_ne!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }

        if let Err(err) = s.write_sctp(&p.slice(..65537), ppi).await {
            assert_eq!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_assoc_max_message_size_explicit() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 30000,
        name: "client".to_owned(),
    });

    assert_eq!(
        a.max_message_size.load(Ordering::SeqCst),
        30000,
        "should match"
    );

    let stream = a.create_stream(1, false);
    assert!(stream.is_some(), "should succeed");

    if let Some(s) = stream {
        let p = Bytes::from(vec![0u8; 30001]);
        let ppi = PayloadProtocolIdentifier::from(s.default_payload_type.load(Ordering::SeqCst));

        if let Err(err) = s.write_sctp(&p.slice(..30000), ppi).await {
            assert_ne!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }

        if let Err(err) = s.write_sctp(&p.slice(..30001), ppi).await {
            assert_eq!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }
    }

    Ok(())
}

// roomler (FR-148): the acknowledgement clock counts the bytes the peer
// received. Under `maxRetransmits: 0` every chunk is abandoned at its first
// send, and a FORWARD TSN lets the cumulative point pass a lost one; the clock
// leaves such a chunk out once a SACK reported it missing or T3-rtx expired
// while it was out.

fn delivery_association(next_tsn: u32) -> AssociationInternal {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.set_state(AssociationState::Established);
    a.use_forward_tsn = true;
    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 9;
    a.my_next_tsn = next_tsn;
    a
}

/// A chunk in flight. A partially reliable one is abandoned at its first
/// send, as `maxRetransmits: 0` leaves it.
fn sent(tsn: u32, len: usize, partially_reliable: bool) -> ChunkPayloadData {
    ChunkPayloadData {
        unordered: partially_reliable,
        beginning_fragment: true,
        ending_fragment: true,
        tsn,
        stream_identifier: 1,
        user_data: Bytes::from(vec![0u8; len]),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(partially_reliable)),
        all_inflight: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    }
}

fn sack(cumulative_tsn_ack: u32, gaps: &[(u16, u16)]) -> ChunkSelectiveAck {
    use crate::chunk::chunk_selective_ack::GapAckBlock;
    ChunkSelectiveAck {
        cumulative_tsn_ack,
        advertised_receiver_window_credit: 1 << 20,
        gap_ack_blocks: gaps
            .iter()
            .map(|&(start, end)| GapAckBlock { start, end })
            .collect(),
        duplicate_tsn: vec![],
    }
}

fn acked_bytes(a: &AssociationInternal) -> u64 {
    a.delivery_clock().read().acked_bytes
}

#[tokio::test]
async fn test_delivery_clock_counts_no_skipped_chunk_and_a_gap_acked_one_once() -> Result<()> {
    let mut a = delivery_association(13);
    for (tsn, len) in [(10, 100), (11, 200), (12, 400)] {
        a.inflight_queue.push_no_check(sent(tsn, len, true));
    }

    // 10 is lost; 11 and 12 arrive and are acknowledged by a gap block.
    a.handle_sack(&sack(9, &[(2, 3)])).await?;
    assert_eq!(acked_bytes(&a), 600, "the gap block's two chunks");

    // A FORWARD TSN skips 10, and the cumulative point passes all three.
    a.handle_sack(&sack(12, &[])).await?;
    assert_eq!(
        acked_bytes(&a),
        600,
        "10 never arrived, and 11 and 12 count once"
    );
    assert_eq!(a.cumulative_tsn_ack_point, 12);
    assert!(a.inflight_queue.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_delivery_clock_counts_a_partially_reliable_chunk_nothing_reported_missing(
) -> Result<()> {
    let mut a = delivery_association(13);
    a.inflight_queue.push_no_check(sent(10, 100, false));
    a.inflight_queue.push_no_check(sent(11, 200, true));
    a.inflight_queue.push_no_check(sent(12, 400, true));

    // 10, a reliable chunk, is reported missing: 11 is in a gap block.
    a.handle_sack(&sack(9, &[(2, 2)])).await?;
    assert_eq!(acked_bytes(&a), 200);

    // 10 was retransmitted until it arrived: a reliable chunk is never
    // skipped, so the cumulative point passing it means the peer has it. 12
    // is abandoned, as every chunk of its stream is, and nothing reported it
    // missing: it counts.
    a.handle_sack(&sack(12, &[])).await?;
    assert_eq!(acked_bytes(&a), 700);

    Ok(())
}

#[tokio::test]
async fn test_delivery_clock_counts_no_chunk_out_at_a_t3_expiry_that_a_forward_tsn_passes(
) -> Result<()> {
    let mut a = delivery_association(11);
    a.inflight_queue.push_no_check(sent(10, 100, true));

    // Nothing comes back for 10, and T3-rtx expires. A FORWARD TSN skips it.
    a.on_retransmission_timeout(RtxTimerId::T3RTX, 1).await;
    a.handle_sack(&sack(10, &[])).await?;
    assert_eq!(acked_bytes(&a), 0, "10 may never have arrived");

    // The mark covers what was out at the expiry, and nothing sent after it.
    a.my_next_tsn = 12;
    a.inflight_queue.push_no_check(sent(11, 200, true));
    a.handle_sack(&sack(11, &[])).await?;
    assert_eq!(acked_bytes(&a), 200);

    Ok(())
}

// roomler (FR-148 P0): the gauges. The association stores them and never reads
// them back, so a reader changes nothing it sends. The twin test runs one
// scripted exchange twice, the second time with a reader polling both sides'
// gauges as fast as it can, and compares every byte both sides sent, packet by
// packet, after rewriting what each association draws at random and nothing
// else: a verification tag becomes whose it is, a TSN its offset from its
// sender's initial TSN, the state cookie zeros. The checksum is zeroed too: it
// is a function of the other bytes, the drawn ones among them.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::association::gauges::{GaugeReader, Gauges};

/// What each association draws at random, which two runs cannot share.
#[derive(Clone, Copy)]
struct Drawn {
    agent_tag: u32,
    viewer_tag: u32,
    agent_tsn: u32,
    viewer_tsn: u32,
}

/// The 32-bit field at `at`, rewritten by `f`.
fn rewrite32(b: &mut [u8], at: usize, f: impl Fn(u32) -> u32) {
    let v = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    b[at..at + 4].copy_from_slice(&f(v).to_be_bytes());
}

/// One packet's bytes as they went on the wire, with the drawn values
/// rewritten (RFC 4960 §3 for every offset). Each chunk type the exchange can
/// send is walked; any other fails the test, so no byte goes unread.
fn normalised(from_agent: bool, raw: &Bytes, d: &Drawn) -> Vec<u8> {
    let mut b = raw.to_vec();
    let role = |t: u32| match t {
        0 => 0,
        t if t == d.agent_tag => 1,
        t if t == d.viewer_tag => 2,
        t => panic!("a verification tag of neither side: {t}"),
    };
    let (own_tsn, peer_tsn) = if from_agent {
        (d.agent_tsn, d.viewer_tsn)
    } else {
        (d.viewer_tsn, d.agent_tsn)
    };
    rewrite32(&mut b, 4, role);
    rewrite32(&mut b, 8, |_| 0); // the checksum
    let mut at = 12;
    while at < b.len() {
        let (typ, len) = (
            b[at],
            usize::from(u16::from_be_bytes([b[at + 2], b[at + 3]])),
        );
        match typ {
            // DATA: the TSN.
            0 => rewrite32(&mut b, at + 4, |t| t.wrapping_sub(own_tsn)),
            // INIT and INIT ACK: the initiate tag, the initial TSN, and a
            // state cookie among the parameters.
            1 | 2 => {
                rewrite32(&mut b, at + 4, role);
                rewrite32(&mut b, at + 16, |t| t.wrapping_sub(own_tsn));
                let mut p = at + 20;
                while p < at + len {
                    let (pt, pl) = (
                        u16::from_be_bytes([b[p], b[p + 1]]),
                        usize::from(u16::from_be_bytes([b[p + 2], b[p + 3]])),
                    );
                    if pt == 7 {
                        b[p + 4..p + pl].fill(0);
                    }
                    p += pl.div_ceil(4) * 4;
                }
            }
            // SACK: the cumulative TSN acknowledged, and each duplicate TSN.
            3 => {
                rewrite32(&mut b, at + 4, |t| t.wrapping_sub(peer_tsn));
                let gaps = usize::from(u16::from_be_bytes([b[at + 12], b[at + 13]]));
                let dups = usize::from(u16::from_be_bytes([b[at + 14], b[at + 15]]));
                for i in 0..dups {
                    rewrite32(&mut b, at + 16 + gaps * 4 + i * 4, |t| {
                        t.wrapping_sub(peer_tsn)
                    });
                }
            }
            // COOKIE ECHO: the cookie.
            10 => b[at + 4..at + len].fill(0),
            // COOKIE ACK carries nothing.
            11 => {}
            t => panic!("the twin exchange sends no chunk of type {t}"),
        }
        at += len.div_ceil(4) * 4;
    }
    b
}

/// Whether a packet's first chunk is DATA.
fn carries_data(raw: &Bytes) -> bool {
    raw.get(12) == Some(&0)
}

/// Which of the agent's DATA packets the path loses, by their order.
enum Loss {
    None,
    Packet(usize),
    From(usize),
}

/// The two ends of the twin exchange, joined by a wire this test drives: no
/// timer and no task, so the same script sends the same packets.
struct Twin {
    /// roomler (FR-148 P0, D35): behind a lock, for a window handle.
    agent: Arc<Mutex<AssociationInternal>>,
    viewer: AssociationInternal,
    drawn: Drawn,
    /// Every packet in the order it went on the wire: from the agent, lost on
    /// the way, its normalised bytes.
    sent: Vec<(bool, bool, Vec<u8>)>,
    data_packets: usize,
    loss: Loss,
    reads: Option<Arc<AtomicU64>>,
    /// The agent's congestion window after each step.
    windows: BTreeSet<u32>,
    /// roomler (FR-148 P0, D35): a window handle's snapshots (`WindowWatch`).
    watch: Option<WindowWatch>,
}

impl Twin {
    /// Both sides' packets onto the wire, then one off it to its peer, until
    /// the wire is empty and neither side has more to send. A quiet wire with
    /// the viewer holding a delayed acknowledgement fires its ack timer.
    async fn pump(&mut self) {
        let mut wire: VecDeque<(bool, Bytes)> = VecDeque::new();
        loop {
            for from_agent in [true, false] {
                let (packets, _) = if from_agent {
                    self.agent().gather_outbound().await
                } else {
                    self.viewer.gather_outbound().await
                };
                for p in packets {
                    wire.push_back((from_agent, p.marshal().expect("a packet marshals")));
                }
            }
            let Some((from_agent, raw)) = wire.pop_front() else {
                if self.viewer.ack_state == AckState::Delay {
                    self.viewer.on_ack_timeout().await;
                    continue;
                }
                return;
            };
            let lost = from_agent && carries_data(&raw) && {
                let n = self.data_packets;
                self.data_packets += 1;
                match self.loss {
                    Loss::None => false,
                    Loss::Packet(k) => n == k,
                    Loss::From(k) => n >= k,
                }
            };
            self.sent
                .push((from_agent, lost, normalised(from_agent, &raw, &self.drawn)));
            if !lost {
                if from_agent {
                    self.viewer.handle_inbound(&raw).await
                } else {
                    self.agent().handle_inbound(&raw).await
                }
                .expect("the peer takes it");
            }
            self.windows.insert(self.agent_gauges().cwnd);
            self.wait_for_reader();
            self.snapshot().await;
        }
    }

    /// With a reader, wait until it has read both sides' gauges since this
    /// step, so it saw every state the exchange went through.
    fn wait_for_reader(&self) {
        if let Some(reads) = &self.reads {
            let seen = reads.load(std::sync::atomic::Ordering::Acquire);
            while reads.load(std::sync::atomic::Ordering::Acquire) < seen + 2 {
                std::hint::spin_loop();
            }
        }
    }

    fn agent_gauges(&self) -> Gauges {
        self.agent().pending_queue.gauges().read(false)
    }

    /// The agent's gauges are what it holds.
    fn assert_gauges_hold_the_agent(&self) {
        let g = self.agent_gauges();
        let a = self.agent();
        assert_eq!(g.max_payload, a.max_payload_size);
        assert_eq!(g.max_payload, 1200, "1228 less 12 and 16");
        assert_eq!(g.cwnd, a.cwnd);
        assert_eq!(g.ssthresh, a.ssthresh);
        assert_eq!(g.rwnd, a.rwnd);
        assert_eq!(g.inflight_bytes, a.inflight_queue.get_num_bytes() as u64);
        assert_eq!(g.pending_bytes, a.pending_queue.get_num_bytes() as u64);
        assert_eq!(g.fast_retransmits, a.stats.get_num_fast_retrans());
        assert_eq!(g.t3_expiries, a.stats.get_num_t3timeouts());
    }
}

async fn write_messages(stream: &Stream, messages: std::ops::Range<u8>) {
    for m in messages {
        stream
            .write_sctp(
                &Bytes::from(vec![m; 1200]),
                PayloadProtocolIdentifier::Binary,
            )
            .await
            .expect("the queue has room");
    }
}

/// What the reader saw: how many times it read both sides, and every
/// distinct reading of each side.
struct ReaderLog {
    reads: u64,
    seen: HashSet<(usize, Gauges)>,
}

fn spawn_reader(
    gauges: [GaugeReader; 2],
    stop: Arc<AtomicBool>,
    reads: Arc<AtomicU64>,
) -> std::thread::JoinHandle<ReaderLog> {
    std::thread::spawn(move || {
        let mut seen = HashSet::new();
        while !stop.load(Ordering::Relaxed) {
            for (side, g) in gauges.iter().enumerate() {
                seen.insert((side, g.take()));
            }
            reads.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        ReaderLog {
            reads: reads.load(std::sync::atomic::Ordering::Acquire),
            seen,
        }
    })
}

/// The script: the handshake on the agent's INIT, 24 messages in slow start
/// with nothing lost, 24 more with one packet lost that the SACKs report (fast
/// retransmit), and 6 more whose whole flight is lost until T3-rtx expires.
async fn twin_exchange(
    watch: Watch,
    lost: usize,
) -> (Vec<(bool, bool, Vec<u8>)>, BTreeSet<u32>, Option<ReaderLog>) {
    let with_reader = matches!(watch, Watch::Reader);
    let agent = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "agent".to_owned(),
    });
    let viewer = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "viewer".to_owned(),
    });
    let drawn = Drawn {
        agent_tag: agent.my_verification_tag,
        viewer_tag: viewer.my_verification_tag,
        agent_tsn: agent.my_next_tsn,
        viewer_tsn: viewer.my_next_tsn,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let reader = with_reader.then(|| {
        spawn_reader(
            [agent.gauge_reader(), viewer.gauge_reader()],
            Arc::clone(&stop),
            Arc::clone(&reads),
        )
    });
    let agent = Arc::new(Mutex::new(agent));
    let mut twin = Twin {
        watch: WindowWatch::of(&agent, &watch),
        agent,
        viewer,
        drawn,
        sent: vec![],
        data_packets: 0,
        loss: Loss::None,
        reads: with_reader.then(|| Arc::clone(&reads)),
        windows: BTreeSet::new(),
    };
    twin.wait_for_reader();

    // The handshake, as `Association::client` starts it.
    let mut agent = twin.agent();
    let mut init = ChunkInit {
        initial_tsn: agent.my_next_tsn,
        num_outbound_streams: agent.my_max_num_outbound_streams,
        num_inbound_streams: agent.my_max_num_inbound_streams,
        initiate_tag: agent.my_verification_tag,
        advertised_receiver_window_credit: agent.max_receive_buffer_size,
        ..Default::default()
    };
    init.set_supported_extensions();
    agent.set_state(AssociationState::CookieWait);
    agent.stored_init = Some(init);
    agent.send_init().expect("an INIT to send");
    drop(agent);
    twin.pump().await;
    assert_eq!(twin.agent().get_state(), AssociationState::Established);
    let window = twin.viewer.max_receive_buffer_size;
    let g = twin.agent_gauges();
    assert_eq!(
        (
            g.cwnd,
            g.ssthresh,
            g.rwnd,
            g.inflight_bytes,
            g.pending_bytes
        ),
        (4380, window, window, 0, 0),
        "the viewer's INIT ACK set the slow start threshold to its window"
    );
    twin.viewer
        .open_stream(1, PayloadProtocolIdentifier::Binary)
        .expect("the viewer's stream");
    let stream = twin
        .agent()
        .open_stream(1, PayloadProtocolIdentifier::Binary)
        .expect("the agent's stream");

    // Slow start, nothing lost.
    write_messages(&stream, 0..24).await;
    assert_eq!(twin.agent_gauges().pending_bytes, 24 * 1200);
    twin.pump().await;
    twin.assert_gauges_hold_the_agent();
    let g = twin.agent_gauges();
    assert_eq!((g.inflight_bytes, g.pending_bytes), (0, 0));
    assert!(g.cwnd > 4380, "slow start grew the window: {}", g.cwnd);
    assert!(g.ack_delay.is_some());

    // One packet lost, which the SACKs report: fast retransmit.
    twin.loss = Loss::Packet(twin.data_packets + lost);
    write_messages(&stream, 24..48).await;
    twin.pump().await;
    twin.assert_gauges_hold_the_agent();
    let g = twin.agent_gauges();
    assert!(g.fast_retransmits >= 1, "{g:?}");
    assert!(g.ssthresh < window, "the loss cut the threshold: {g:?}");
    assert_eq!(
        (g.inflight_bytes, g.pending_bytes, g.t3_expiries),
        (0, 0, 0)
    );

    // A whole flight lost, which nothing reports: T3-rtx expires.
    twin.loss = Loss::From(twin.data_packets);
    write_messages(&stream, 48..54).await;
    twin.pump().await;
    let g = twin.agent_gauges();
    assert!(g.inflight_bytes > 0, "{g:?}");
    assert_eq!(g.inflight_bytes + g.pending_bytes, 6 * 1200);
    twin.agent()
        .on_retransmission_timeout(RtxTimerId::T3RTX, 1)
        .await;
    let g = twin.agent_gauges();
    assert_eq!((g.t3_expiries, g.cwnd), (1, twin.agent().mtu));
    twin.loss = Loss::None;
    twin.pump().await;
    twin.assert_gauges_hold_the_agent();
    let g = twin.agent_gauges();
    assert_eq!(
        (g.inflight_bytes, g.pending_bytes, g.t3_expiries),
        (0, 0, 1)
    );
    assert!(twin.sent.iter().any(|(_, lost, _)| *lost));

    stop.store(true, Ordering::Relaxed);
    let log = reader.map(|r| r.join().expect("the reader ends"));
    (twin.sent, twin.windows, log)
}

#[tokio::test]
async fn test_gauges_a_reader_changes_nothing_the_association_sends() {
    let (plain, _, _) = twin_exchange(Watch::Nothing, 2).await;
    let (read, windows, log) = twin_exchange(Watch::Reader, 2).await;
    let log = log.expect("the second run had a reader");

    for (i, (p, r)) in plain.iter().zip(&read).enumerate() {
        assert_eq!(p, r, "packet {i} differs");
    }
    assert_eq!(plain.len(), read.len(), "both runs sent as many packets");
    assert!(
        plain.len() > 100,
        "the exchange sent {} packets",
        plain.len()
    );
    let bytes: usize = plain.iter().map(|(_, _, b)| b.len()).sum();

    // The comparison sees a change of one packet: the same script losing the
    // fourth packet of the flight instead of the third sends differently.
    let (other, _, _) = twin_exchange(Watch::Nothing, 3).await;
    assert_ne!(other, plain);

    // The reader read both sides after every packet, so it saw every window
    // the agent went through.
    assert!(log.reads >= read.len() as u64 * 2, "{} reads", log.reads);
    let agent: Vec<&Gauges> = log
        .seen
        .iter()
        .filter(|(side, _)| *side == 0)
        .map(|(_, g)| g)
        .collect();
    let seen: BTreeSet<u32> = agent.iter().map(|g| g.cwnd).collect();
    assert!(seen.is_superset(&windows), "{seen:?} {windows:?}");
    eprintln!(
        "twin: {} packets ({bytes} bytes) each run, the same; the reader read {} times and saw {} agent readings, windows {windows:?}",
        read.len(),
        log.reads,
        agent.len(),
    );
    assert!(agent.iter().any(|g| g.fast_retransmits >= 1));
    assert!(agent.iter().any(|g| g.t3_expiries == 1));
    assert!(agent.iter().any(|g| g.ack_delay_min.is_some()));
    assert!(agent.iter().any(|g| g.pending_bytes > 0));
}

/// An association with chunks in flight sent `ago` before now, the newest
/// last, each `(tsn, sent how many times, how long ago)`.
fn ack_delay_association(chunks: &[(u32, u32, Duration)]) -> AssociationInternal {
    let mut a = delivery_association(chunks.last().map_or(10, |c| c.0 + 1));
    a.use_forward_tsn = false;
    for &(tsn, nsent, ago) in chunks {
        let mut c = sent(tsn, 100, false);
        c.nsent = nsent;
        c.since = SystemTime::now() - ago;
        a.inflight_queue.push_no_check(c);
    }
    a
}

#[tokio::test]
async fn test_gauges_ack_delay_is_the_newest_chunk_sent_once() -> Result<()> {
    let ms = Duration::from_millis;
    // 12, the newest, was sent twice (Karn): 11 is the one that counts.
    let mut a = ack_delay_association(&[(10, 1, ms(900)), (11, 1, ms(500)), (12, 2, ms(100))]);
    let gauges = a.gauge_reader();
    assert_eq!(gauges.take().ack_delay, None);

    a.handle_sack(&sack(12, &[])).await?;
    let g = gauges.take();
    let delay = g.ack_delay.expect("a delay");
    assert!(delay >= ms(500) && delay < ms(900), "{delay:?}");
    assert_eq!(g.ack_delay_min, g.ack_delay);
    assert_eq!(
        gauges.take().ack_delay_min,
        None,
        "the take reset the least"
    );
    assert_eq!(gauges.take().ack_delay, g.ack_delay, "the latest stays");

    // A gap block above the cumulative point holds the newest.
    let mut a = ack_delay_association(&[(10, 1, ms(900)), (11, 1, ms(700)), (12, 1, ms(300))]);
    let gauges = a.gauge_reader();
    a.handle_sack(&sack(10, &[(2, 2)])).await?;
    let delay = gauges.take().ack_delay.expect("a delay");
    assert!(delay >= ms(300) && delay < ms(700), "{delay:?}");

    // The least since the last take is the least of the SACKs since.
    a.handle_sack(&sack(11, &[(1, 1)])).await?;
    let g = gauges.take();
    assert!(g.ack_delay.expect("a delay") >= ms(700));
    assert_eq!(g.ack_delay_min, g.ack_delay);

    Ok(())
}

#[tokio::test]
async fn test_gauges_a_peek_leaves_the_least_delay_to_the_next_take() -> Result<()> {
    let ms = Duration::from_millis;
    // 10's SACK came 300 ms after its send, 11's 900 ms after (a delayed
    // acknowledgement): the least of the two is 10's.
    let chunks = [(10, 1, ms(300)), (11, 1, ms(900))];
    let mut a = ack_delay_association(&chunks);
    let gauges = a.gauge_reader();
    a.handle_sack(&sack(10, &[])).await?;
    let peeked = gauges.peek();
    let least = peeked.ack_delay_min.expect("a least delay");
    assert!(least >= ms(300) && least < ms(900), "{least:?}");
    assert_eq!(gauges.peek(), peeked, "a second peek reads the same");

    a.handle_sack(&sack(11, &[])).await?;
    let taken = gauges.take();
    assert!(taken.ack_delay.expect("a delay") >= ms(900), "{taken:?}");
    assert_eq!(
        taken.ack_delay_min,
        Some(least),
        "the take's least still covers the SACK before the peek"
    );
    assert_eq!(gauges.peek().ack_delay_min, None, "the take reset it");

    // A take where the peek was leaves the next take the later SACK alone.
    let mut a = ack_delay_association(&chunks);
    let gauges = a.gauge_reader();
    a.handle_sack(&sack(10, &[])).await?;
    assert_eq!(gauges.take().ack_delay_min.map(|d| d < ms(900)), Some(true));
    a.handle_sack(&sack(11, &[])).await?;
    let least = gauges.take().ack_delay_min.expect("a least delay");
    assert!(least >= ms(900), "{least:?}");

    Ok(())
}

#[tokio::test]
async fn test_gauges_a_clock_gone_backwards_stores_nothing() -> Result<()> {
    let ms = Duration::from_millis;
    let mut a = ack_delay_association(&[(10, 1, ms(50))]);
    let gauges = a.gauge_reader();
    a.handle_sack(&sack(10, &[])).await?;
    let before = gauges.take();
    assert!(before.ack_delay.is_some());

    // 11 was sent "after" this SACK: the clock went back between the two. The
    // association measured its round trip on 10, so the RTT path, which
    // errors on such a chunk, does not read it; the gauge must not either.
    a.my_next_tsn = 12;
    let mut c = sent(11, 100, false);
    c.since = SystemTime::now() + Duration::from_secs(3600);
    a.inflight_queue.push_no_check(c);
    a.min_tsn2measure_rtt = 12;
    let cwnd = a.cwnd;
    a.handle_sack(&sack(11, &[])).await?;

    let after = gauges.take();
    assert_eq!(after.ack_delay, before.ack_delay, "the latest is unchanged");
    assert_eq!(after.ack_delay_min, None, "nothing new since the take");
    assert_eq!(acked_bytes(&a), 200, "the SACK went on as it would");
    assert_eq!(after.cwnd, a.cwnd);
    assert!(a.cwnd >= cwnd);

    Ok(())
}

/// roomler (#4033): one DATA chunk, `tsn`, in a packet of its own, taken as
/// `handle_inbound` takes a packet.
async fn receive_lone_data(a: &mut AssociationInternal, tsn: u32) -> Result<()> {
    a.handle_chunk_start();
    a.handle_data(&ChunkPayloadData {
        unordered: true,
        beginning_fragment: true,
        ending_fragment: true,
        tsn,
        stream_identifier: 1,
        user_data: Bytes::from_static(b"lone"),
        ..Default::default()
    })
    .await?;
    a.handle_chunk_end();
    Ok(())
}

/// roomler (#4033): the cumulative TSN of the SACK the write loop would send
/// now, if one is owed.
async fn sack_owed(a: &mut AssociationInternal) -> Option<u32> {
    a.gather_outbound_sack_packets(vec![])
        .await
        .iter()
        .flat_map(|p| p.chunks.iter())
        .find_map(|c| {
            c.as_any()
                .downcast_ref::<ChunkSelectiveAck>()
                .map(|s| s.cumulative_tsn_ack)
        })
}

/// roomler (#4033): whether the ack timer's handle is set, so that its
/// `start` arms nothing.
fn ack_timer_set(a: &AssociationInternal) -> bool {
    a.ack_timer.as_ref().is_some_and(|t| t.is_running())
}

/// roomler (#4033): a timeout delivered late loses no SACK, and no timeout
/// leaves the ack timer's handle set. The race: a timer fires, and its task
/// waits for the association's lock; meanwhile a second packet is
/// acknowledged at once, which stops that timer, and a lone packet after it
/// arms a new one; then the old task's timeout runs. It stops the new timer,
/// whose SACK it sends at once. The timers here never fire on their own (an
/// hour, and no observer), so the test delivers each timeout in that order.
#[tokio::test]
async fn test_a_late_ack_timeout_loses_no_sack_and_leaves_the_timer_free() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "receiver".to_owned(),
    });
    a.ack_timer = Some(AckTimer::new(
        std::sync::Weak::new(),
        Duration::from_secs(3600),
    ));
    a.open_stream(1, PayloadProtocolIdentifier::Binary)?;

    // A lone packet: its SACK is delayed, and the timer armed.
    receive_lone_data(&mut a, 1).await?;
    assert_eq!(a.ack_state, AckState::Delay);
    assert!(ack_timer_set(&a), "the first lone packet armed no timer");
    assert_eq!(sack_owed(&mut a).await, None);

    // Its timer fires, and its task waits. A second packet is acknowledged
    // at once, which stops the timer.
    receive_lone_data(&mut a, 2).await?;
    assert_eq!(sack_owed(&mut a).await, Some(2));
    assert!(!ack_timer_set(&a));

    // A lone packet after it arms a new timer.
    receive_lone_data(&mut a, 3).await?;
    assert_eq!(a.ack_state, AckState::Delay);
    assert!(
        ack_timer_set(&a),
        "the lone packet after a stop armed no timer"
    );

    // The old task's timeout runs now.
    a.on_ack_timeout().await;
    assert_eq!(
        sack_owed(&mut a).await,
        Some(3),
        "the late timeout lost the SACK of the lone packet after it"
    );
    assert!(
        !ack_timer_set(&a),
        "a late timeout left the ack timer's handle set: the next lone packet arms no timer"
    );

    // The next lone packet arms a timer, and its own timeout sends its SACK
    // and leaves the timer free for the one after.
    receive_lone_data(&mut a, 4).await?;
    assert!(
        ack_timer_set(&a),
        "the lone packet after a late timeout armed no timer"
    );
    a.on_ack_timeout().await;
    assert_eq!(sack_owed(&mut a).await, Some(4));
    assert!(
        !ack_timer_set(&a),
        "a timeout left the ack timer's handle set: the next lone packet arms no timer"
    );

    Ok(())
}

use std::future::Future;
use std::task::Poll;

/// Roomler patch (#4116): the association's close ends a read of its
/// streams, whenever it lands. `unregister_stream` sets a stream's
/// `read_shutdown`, then wakes the waiters of that instant with
/// `notify_waiters()`, as the stream's own shutdown does, and upstream's
/// `read_sctp` made its wait only after its check: a close that landed
/// between the two was lost, and the read waited for good. The test holds
/// the stream's queue lock, so the read stops there, past its check, while
/// the association closes; then it lets the lock go. One thread, so the
/// order is the code's, not a race's.
#[tokio::test(flavor = "current_thread")]
async fn test_assoc_close_ends_a_read_between_its_check_and_its_wait() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.set_state(AssociationState::Established);
    let s = a.create_stream(1, false).expect("a new stream");
    let mut buf = [0u8; 64];

    let held = s.reassembly_queue.lock().await;
    let mut read = std::pin::pin!(s.read_sctp(&mut buf));
    let polled = std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await;
    assert!(
        polled.is_pending(),
        "the read must wait for the queue's lock, past its check of the flag"
    );
    a.close().await?;
    drop(held);

    match tokio::time::timeout(Duration::from_secs(5), read).await {
        Ok(read) => assert_eq!(
            read,
            Ok((0, PayloadProtocolIdentifier::Unknown)),
            "a read of a stream its association closed returns (0, Unknown)"
        ),
        Err(_) => panic!(
            "#4116: the association closed between the read's check and its wait, and the \
             read missed it: it waits for good for data that never comes"
        ),
    }
    Ok(())
}

// roomler (FR-148 P0, D43): the pacer in the association. It holds only new
// data, only while a writer has it on, and only once a SACK gave it a round
// trip; a retransmission never waits for it. Where a test needs the tokens
// to hold, the round trip is 1,000 s and the rate 12 bytes a second: a held
// chunk waits longer than any test runs, so the counts do not depend on how
// long a test takes.

/// An established association in slow start whose window holds ten
/// messages, with `pending` messages of 1,200 bytes waiting; chunk 10 is the
/// next to go.
async fn pacer_association(pending: usize) -> AssociationInternal {
    let mut a = delivery_association(10);
    a.use_forward_tsn = false;
    a.cwnd = 12_000;
    a.ssthresh = u32::MAX;
    a.rwnd = 1 << 20;
    for _ in 0..pending {
        let c = ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            stream_identifier: 1,
            user_data: Bytes::from(vec![0u8; 1200]),
            ..Default::default()
        };
        assert!(a.pending_queue.push(c).await);
    }
    a
}

/// How many new chunks one send moves into flight.
async fn new_chunks_sent(a: &mut AssociationInternal) -> usize {
    a.pop_pending_data_chunks_to_send().await.0.len()
}

#[tokio::test]
async fn test_pacer_off_or_with_no_round_trip_holds_nothing() {
    let mut a = pacer_association(12).await;
    // Off, a round trip is kept nowhere, and the window alone decides.
    a.pacer.on_rtt(Duration::from_secs(1_000));
    assert_eq!(new_chunks_sent(&mut a).await, 10);
    assert_eq!(a.take_pace_wake(), None);

    // On, with no round trip yet: the first flight leaves as it would.
    let mut a = pacer_association(12).await;
    a.pace_switch().set(true);
    assert_eq!(new_chunks_sent(&mut a).await, 10);
    assert_eq!(a.take_pace_wake(), None);
}

#[tokio::test]
async fn test_pacer_takes_its_round_trip_from_a_sack_and_holds_new_data() -> Result<()> {
    // Chunk 10 in flight, sent 1,000 s ago, then twelve messages waiting.
    let mut a = ack_delay_association(&[(10, 1, Duration::from_secs(1_000))]);
    let switch = a.pace_switch();
    switch.set(true);
    a.handle_sack(&sack(10, &[])).await?;
    for _ in 0..12 {
        let c = ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            stream_identifier: 1,
            user_data: Bytes::from(vec![0u8; 1200]),
            ..Default::default()
        };
        assert!(a.pending_queue.push(c).await);
    }
    (a.cwnd, a.ssthresh, a.rwnd) = (12_000, u32::MAX, 1 << 20);

    // 12 bytes a second: the first send's tokens let one chunk out, the
    // write loop gets a wake for the next, and a send before it moves none.
    assert_eq!(new_chunks_sent(&mut a).await, 1);
    assert!(a.take_pace_wake().is_some());
    assert_eq!(new_chunks_sent(&mut a).await, 0);
    assert!(a.take_pace_wake().is_some());

    // Turned off, it holds nothing: the window decides again.
    switch.set(false);
    assert_eq!(new_chunks_sent(&mut a).await, 9);
    assert_eq!(a.take_pace_wake(), None);
    Ok(())
}

#[tokio::test]
async fn test_pacer_the_cap_holds_new_data_and_a_retransmission_never_waits() {
    let mut a = pacer_association(12).await;
    let switch = a.pace_switch();
    switch.set(true);
    switch.steer(crate::association::pacer::WHOLE, Some(3_600));
    // A round trip of a microsecond: the tokens hold nothing, the cap does.
    a.pacer.on_rtt(Duration::from_micros(1));
    assert_eq!(new_chunks_sent(&mut a).await, 3);
    assert_eq!(new_chunks_sent(&mut a).await, 0);
    assert_eq!(a.take_pace_wake(), None, "a SACK wakes it, not a timer");

    // T3-rtx expires with the cap reached: the first chunk goes again, within
    // the window of one packet the expiry leaves.
    a.on_retransmission_timeout(RtxTimerId::T3RTX, 1).await;
    let (packets, _) = a.gather_outbound().await;
    let resent: Vec<(u32, u32)> = packets
        .iter()
        .flat_map(|p| p.chunks.iter())
        .filter_map(|c| c.as_any().downcast_ref::<ChunkPayloadData>())
        .map(|c| (c.tsn, c.nsent))
        .collect();
    assert_eq!(resent, [(10, 2)]);
}

// roomler (FR-148 P0, D35): the twin exchange, watched by a window handle.

use crate::association::windows::{WindowHandle, Windows};

/// What watches the twin's agent besides its packets.
enum Watch {
    Nothing,
    /// A thread reading both sides' gauges (D33).
    Reader,
    /// A window handle onto the agent, and a snapshot taken through it after
    /// every step, counted here.
    Windows(Arc<AtomicU64>),
}

/// A window handle onto the twin's agent, and the count of its snapshots.
struct WindowWatch {
    handle: WindowHandle,
    taken: Arc<AtomicU64>,
}

impl WindowWatch {
    /// For `Watch::Windows`, a handle onto `agent`, made as
    /// `Association::windows` makes one.
    fn of(agent: &Arc<Mutex<AssociationInternal>>, watch: &Watch) -> Option<Self> {
        match watch {
            Watch::Windows(taken) => Some(WindowWatch {
                handle: WindowHandle::new(Arc::downgrade(agent)),
                taken: Arc::clone(taken),
            }),
            Watch::Nothing | Watch::Reader => None,
        }
    }
}

impl Twin {
    /// The agent, locked. Only the twin's one task takes the lock, for a step
    /// at most, so it is free here.
    fn agent(&self) -> tokio::sync::MutexGuard<'_, AssociationInternal> {
        self.agent
            .try_lock()
            .expect("only the twin's task takes the agent's lock")
    }

    /// With a window handle, a snapshot through it, which reads what the
    /// agent holds.
    async fn snapshot(&self) {
        let Some(watch) = &self.watch else {
            return;
        };
        let snapshot = watch.handle.snapshot().await.expect("the agent is there");
        let a = self.agent();
        assert_eq!(
            snapshot,
            Windows {
                cwnd: a.cwnd,
                ssthresh: a.ssthresh,
                partial_bytes_acked: a.partial_bytes_acked,
            }
        );
        watch.taken.fetch_add(1, Ordering::Relaxed);
    }
}

/// roomler (FR-148 P0, D35): a window handle onto the agent, and a snapshot
/// taken through it after every packet of the exchange, change nothing the
/// association sends: the twin exchange with them and without, packet for
/// packet (D33's twin form). The script is the gauges' twin's, so the
/// snapshots read each kind of window write: the handshake's, slow start's
/// growth, fast recovery's cut and a T3-rtx expiry's.
#[tokio::test]
async fn test_windows_a_window_handle_changes_no_packet() {
    let (plain, plain_windows, _) = twin_exchange(Watch::Nothing, 2).await;
    let taken = Arc::new(AtomicU64::new(0));
    let (watched, windows, _) = twin_exchange(Watch::Windows(Arc::clone(&taken)), 2).await;

    for (i, (p, w)) in plain.iter().zip(&watched).enumerate() {
        assert_eq!(p, w, "packet {i} differs");
    }
    assert_eq!(plain.len(), watched.len(), "both runs sent as many packets");
    assert!(
        plain.len() > 100,
        "the exchange sent {} packets",
        plain.len()
    );
    assert_eq!(
        plain_windows, windows,
        "both runs went through the same windows"
    );
    let taken = taken.load(Ordering::Relaxed);
    assert_eq!(taken, watched.len() as u64, "a snapshot after every packet");
    eprintln!(
        "windows twin: {} packets each run, the same, with {taken} snapshots between them; windows {windows:?}",
        watched.len()
    );
}
