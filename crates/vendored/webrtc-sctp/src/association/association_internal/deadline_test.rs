// roomler (FR-148 P4b-1, D55): the deadline mark, run between twin
// associations.
//
// The deadline-limited video channel is unordered with `maxRetransmits: 0`,
// on carriers that deliver in order. The agent marks its stream
// (`AssociationInternal::mark_deadline_stream`), and then:
// - the association serves ordered and unordered messages in the order they
//   were written;
// - a chunk of the marked stream is abandoned when a SACK reports it missing,
//   or when T3-rtx expires while it is out, never at its first send; it is
//   never retransmitted, and its loss never halves the window;
// - a FORWARD TSN covers only what was abandoned, and goes again only when it
//   carries something new or was lost.
// An association with no marked stream sends what it sent before, byte for
// byte.
//
// Each test runs both ends of the stack: an agent and a viewer joined by a
// pipe that keeps order and delays each packet `DELAY` ticks (a tick is a
// millisecond), on a clock this file drives. The viewer's delayed
// acknowledgement and the agent's T3-rtx run on that clock too. No timer and
// no task runs, so one script sends the same packets every time.

use std::collections::{BTreeSet, HashSet, VecDeque};

use super::*;
use crate::association::gauges::Gauges;

/// The pipe's one-way delay, ticks.
const DELAY: u64 = 20;
/// The viewer's delayed acknowledgement, ticks (`ACK_INTERVAL`).
const ACK_DELAY: u64 = 200;
/// T3-rtx, ticks: the least RTO (`RTO_MIN`), doubled at each expiry in a row.
const RTO: u64 = RTO_MIN;
/// One message a packet: a DATA chunk's payload at the initial MTU.
const ONE_CHUNK: usize = 1200;

/// An ordered, reliable stream: `control`'s shape.
const CONTROL: u16 = 1;
/// An unordered stream with `maxRetransmits: 0`: the deadline channel's shape.
const VIDEO: u16 = 2;

/// A connection that carries nothing: the twin's pipe is this file's.
struct NoConn;

#[async_trait]
impl Conn for NoConn {
    async fn connect(&self, _addr: std::net::SocketAddr) -> std::result::Result<(), util::Error> {
        Err(std::io::Error::other("not applicable").into())
    }

    async fn recv(&self, _b: &mut [u8]) -> std::result::Result<usize, util::Error> {
        Ok(0)
    }

    async fn recv_from(
        &self,
        _buf: &mut [u8],
    ) -> std::result::Result<(usize, std::net::SocketAddr), util::Error> {
        Err(std::io::Error::other("not applicable").into())
    }

    async fn send(&self, _b: &[u8]) -> std::result::Result<usize, util::Error> {
        Ok(0)
    }

    async fn send_to(
        &self,
        _buf: &[u8],
        _target: std::net::SocketAddr,
    ) -> std::result::Result<usize, util::Error> {
        Err(std::io::Error::other("not applicable").into())
    }

    fn local_addr(&self) -> std::result::Result<std::net::SocketAddr, util::Error> {
        Err(std::io::Error::other("not applicable").into())
    }

    fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        None
    }

    async fn close(&self) -> std::result::Result<(), util::Error> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn association(name: &str) -> AssociationInternal {
    let (close_loop_ch_tx, _) = broadcast::channel(1);
    let (accept_ch_tx, _) = mpsc::channel(1);
    let (handshake_completed_ch_tx, _) = mpsc::channel(1);
    let (awake_write_loop_ch_tx, _) = mpsc::channel(1);
    AssociationInternal::new(
        Config {
            net_conn: Arc::new(NoConn),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: name.to_owned(),
        },
        close_loop_ch_tx,
        accept_ch_tx,
        handshake_completed_ch_tx,
        Arc::new(awake_write_loop_ch_tx),
    )
}

/// A seeded xorshift, for the randomised traffic.
#[derive(Clone, Copy)]
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the pipe loses.
enum Loss {
    None,
    /// The agent's DATA packets at these places, in the order they were sent.
    Data(BTreeSet<usize>),
    /// Drawn: per mille of the agent's DATA packets, of the viewer's packets,
    /// and of the agent's DATA packets that start a burst of 5 to 20 lost in
    /// a row.
    Random {
        rng: XorShift,
        data: u64,
        viewer: u64,
        burst: u64,
        in_burst: u64,
    },
}

impl Loss {
    /// Whether the pipe loses a packet; `data` is the place of an agent's DATA
    /// packet.
    fn drops(&mut self, from_agent: bool, data: Option<usize>) -> bool {
        match self {
            Loss::None => false,
            Loss::Data(lost) => data.is_some_and(|n| lost.contains(&n)),
            Loss::Random {
                rng,
                data: per_mille,
                viewer,
                burst,
                in_burst,
            } => {
                if !from_agent {
                    return rng.below(1000) < *viewer;
                }
                if data.is_none() {
                    return false;
                }
                if *in_burst > 0 {
                    *in_burst -= 1;
                    return true;
                }
                if rng.below(1000) < *burst {
                    *in_burst = 4 + rng.below(16);
                    return true;
                }
                rng.below(1000) < *per_mille
            }
        }
    }
}

/// What each association draws at random, which two runs cannot share.
#[derive(Clone, Copy)]
struct Drawn {
    agent_tag: u32,
    viewer_tag: u32,
    agent_tsn: u32,
    viewer_tsn: u32,
}

/// One DATA chunk the agent sent.
#[derive(Debug, Clone, Copy)]
struct Data {
    tick: u64,
    tsn: u32,
    sid: u16,
    beginning: bool,
    ending: bool,
    /// The message's tag, on its first fragment.
    tag: Option<u32>,
    lost: bool,
}

/// One packet on the wire.
struct Sent {
    tick: u64,
    from_agent: bool,
    lost: bool,
    raw: Bytes,
}

/// The two ends, the pipe and the clock.
struct Twin {
    agent: AssociationInternal,
    viewer: AssociationInternal,
    drawn: Drawn,
    now: u64,
    /// In order: (the tick it arrives, from the agent, its bytes).
    pipe: VecDeque<(u64, bool, Bytes)>,
    log: Vec<Sent>,
    data_packets: usize,
    loss: Loss,
    /// Since when the viewer holds a delayed acknowledgement.
    ack_since: Option<u64>,
    /// T3-rtx: when it expires, and the expiries in a row so far.
    t3: Option<(u64, usize)>,
    readers: Vec<Arc<Stream>>,
    /// What the viewer read: (stream, tag, length).
    read: Vec<(u16, u32, usize)>,
    /// The agent's (ssthresh, cwnd) when the handshake was done.
    start: (u32, u32),
    /// The agent's (ssthresh, cwnd) after each packet it took.
    windows: Vec<(u32, u32)>,
    /// The agent's (ssthresh, cwnd) just after each T3-rtx expiry.
    t3_windows: Vec<(u32, u32)>,
    fast_recovery_seen: bool,
}

impl Twin {
    /// Both ends, past the handshake that the agent's INIT starts, as
    /// `Association::client` does.
    async fn new() -> Self {
        let agent = association("agent");
        let viewer = association("viewer");
        let drawn = Drawn {
            agent_tag: agent.my_verification_tag,
            viewer_tag: viewer.my_verification_tag,
            agent_tsn: agent.my_next_tsn,
            viewer_tsn: viewer.my_next_tsn,
        };
        let mut twin = Twin {
            agent,
            viewer,
            drawn,
            now: 0,
            pipe: VecDeque::new(),
            log: vec![],
            data_packets: 0,
            loss: Loss::None,
            ack_since: None,
            t3: None,
            readers: vec![],
            read: vec![],
            start: (0, 0),
            windows: vec![],
            t3_windows: vec![],
            fast_recovery_seen: false,
        };
        let mut init = ChunkInit {
            initial_tsn: twin.agent.my_next_tsn,
            num_outbound_streams: twin.agent.my_max_num_outbound_streams,
            num_inbound_streams: twin.agent.my_max_num_inbound_streams,
            initiate_tag: twin.agent.my_verification_tag,
            advertised_receiver_window_credit: twin.agent.max_receive_buffer_size,
            ..Default::default()
        };
        init.set_supported_extensions();
        twin.agent.set_state(AssociationState::CookieWait);
        twin.agent.stored_init = Some(init);
        twin.agent.send_init().expect("an INIT to send");
        twin.settle(1_000).await;
        assert_eq!(twin.agent.get_state(), AssociationState::Established);
        assert_eq!(twin.viewer.get_state(), AssociationState::Established);
        assert!(
            twin.agent.use_forward_tsn && twin.viewer.use_forward_tsn,
            "both ends took FORWARD TSN"
        );
        twin.start = (twin.agent.ssthresh, twin.agent.cwnd);
        twin.windows.clear();
        twin
    }

    /// A stream on both ends, with the same reliability; the viewer's is read
    /// every tick.
    fn open(
        &mut self,
        si: u16,
        unordered: bool,
        rel_type: ReliabilityType,
        rel_val: u32,
    ) -> Arc<Stream> {
        let viewer = self
            .viewer
            .open_stream(si, PayloadProtocolIdentifier::Binary)
            .expect("the viewer's stream");
        viewer.set_reliability_params(unordered, rel_type, rel_val);
        self.readers.push(viewer);
        let agent = self
            .agent
            .open_stream(si, PayloadProtocolIdentifier::Binary)
            .expect("the agent's stream");
        agent.set_reliability_params(unordered, rel_type, rel_val);
        agent
    }

    /// One tick: what arrives is taken, the timers that are due fire, each
    /// end sends what it has, and the viewer reads what it can.
    async fn step(&mut self) {
        while self.pipe.front().is_some_and(|(at, _, _)| *at <= self.now) {
            let (_, from_agent, raw) = self.pipe.pop_front().expect("a packet");
            if from_agent {
                self.viewer
                    .handle_inbound(&raw)
                    .await
                    .expect("the viewer takes it");
                continue;
            }
            let cumulative = self.agent.cumulative_tsn_ack_point;
            self.agent
                .handle_inbound(&raw)
                .await
                .expect("the agent takes it");
            // T3-rtx: stopped once nothing is out (R2), restarted when the
            // cumulative point moves (R3), and started by a SACK that leaves
            // data out.
            if self.agent.inflight_queue.is_empty() {
                self.t3 = None;
            } else if self.agent.cumulative_tsn_ack_point != cumulative || self.t3.is_none() {
                self.t3 = Some((self.now + RTO, 0));
            }
            self.windows.push((self.agent.ssthresh, self.agent.cwnd));
            self.fast_recovery_seen |= self.agent.in_fast_recovery;
        }

        match (self.viewer.ack_state == AckState::Delay, self.ack_since) {
            (false, _) => self.ack_since = None,
            (true, None) => self.ack_since = Some(self.now),
            (true, Some(since)) if self.now >= since + ACK_DELAY => {
                self.viewer.on_ack_timeout().await;
                self.ack_since = None;
            }
            (true, Some(_)) => {}
        }

        if let Some((at, n)) = self.t3 {
            if self.now >= at {
                self.agent
                    .on_retransmission_timeout(RtxTimerId::T3RTX, n + 1)
                    .await;
                self.t3_windows.push((self.agent.ssthresh, self.agent.cwnd));
                self.t3 = Some((self.now + (RTO << (n + 1).min(6)), n + 1));
            }
        }

        for from_agent in [true, false] {
            let (packets, _) = if from_agent {
                self.agent.gather_outbound().await
            } else {
                self.viewer.gather_outbound().await
            };
            for p in packets {
                let raw = p.marshal().expect("a packet marshals");
                let data = (from_agent && raw.get(12) == Some(&0)).then(|| {
                    self.data_packets += 1;
                    self.data_packets - 1
                });
                if data.is_some() && self.t3.is_none() {
                    self.t3 = Some((self.now + RTO, 0));
                }
                let lost = self.loss.drops(from_agent, data);
                if !lost {
                    self.pipe
                        .push_back((self.now + DELAY, from_agent, raw.clone()));
                }
                self.log.push(Sent {
                    tick: self.now,
                    from_agent,
                    lost,
                    raw,
                });
            }
        }

        let mut buf = vec![0u8; 1 << 16];
        for s in &self.readers {
            let mut q = s.reassembly_queue.try_lock().expect("no one else reads");
            while let Ok((n, _)) = q.read(&mut buf) {
                let tag = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                self.read.push((s.stream_identifier, tag, n));
            }
        }

        self.now += 1;
    }

    /// Nothing on the pipe, nothing to send, nothing out, no acknowledgement
    /// owed.
    fn quiet(&self) -> bool {
        self.pipe.is_empty()
            && self.agent.control_queue.is_empty()
            && self.viewer.control_queue.is_empty()
            && self.agent.pending_queue.is_empty()
            && self.agent.inflight_queue.is_empty()
            && self.viewer.ack_state == AckState::Idle
    }

    /// Steps until quiet, at most `cap` ticks.
    async fn settle(&mut self, cap: u64) {
        for _ in 0..cap {
            if self.quiet() {
                return;
            }
            self.step().await;
        }
        panic!(
            "not quiet after {cap} ticks: pipe {} pending {} in flight {}",
            self.pipe.len(),
            self.agent.pending_queue.len(),
            self.agent.inflight_queue.len()
        );
    }

    fn gauges(&self) -> Gauges {
        self.agent.pending_queue.gauges().read(false)
    }

    fn acked_bytes(&self) -> u64 {
        self.agent.delivery_clock().read().acked_bytes
    }

    /// Every chunk on the wire: (tick, from the agent, lost, the chunk).
    fn chunks(&self) -> Vec<(u64, bool, bool, Box<dyn Chunk + Send + Sync>)> {
        self.log
            .iter()
            .flat_map(|s| {
                Packet::unmarshal(&s.raw)
                    .expect("a packet parses")
                    .chunks
                    .into_iter()
                    .map(move |c| (s.tick, s.from_agent, s.lost, c))
            })
            .collect()
    }

    /// The agent's DATA chunks, in the order they went on the wire.
    fn data(&self) -> Vec<Data> {
        self.chunks()
            .iter()
            .filter(|(_, from_agent, _, _)| *from_agent)
            .filter_map(|(tick, _, lost, c)| {
                c.as_any().downcast_ref::<ChunkPayloadData>().map(|d| Data {
                    tick: *tick,
                    tsn: d.tsn,
                    sid: d.stream_identifier,
                    beginning: d.beginning_fragment,
                    ending: d.ending_fragment,
                    tag: d.beginning_fragment.then(|| {
                        u32::from_be_bytes([
                            d.user_data[0],
                            d.user_data[1],
                            d.user_data[2],
                            d.user_data[3],
                        ])
                    }),
                    lost: *lost,
                })
            })
            .collect()
    }

    /// The agent's DATA chunks sent again.
    fn retransmissions(&self) -> Vec<Data> {
        let mut seen = HashSet::new();
        self.data()
            .into_iter()
            .filter(|d| !seen.insert(d.tsn))
            .collect()
    }

    /// The agent's FORWARD TSN chunks: (tick, new cumulative TSN, lost).
    fn forward_tsns(&self) -> Vec<(u64, u32, bool)> {
        self.chunks()
            .iter()
            .filter(|(_, from_agent, _, _)| *from_agent)
            .filter_map(|(tick, _, lost, c)| {
                c.as_any()
                    .downcast_ref::<ChunkForwardTsn>()
                    .map(|f| (*tick, f.new_cumulative_tsn, *lost))
            })
            .collect()
    }

    /// The SACKs that reached the agent.
    fn sacks_taken(&self) -> usize {
        self.chunks()
            .iter()
            .filter(|(_, from_agent, lost, c)| {
                !*from_agent && !*lost && c.as_any().downcast_ref::<ChunkSelectiveAck>().is_some()
            })
            .count()
    }

    /// The tags the viewer read on stream `si`, in the order it read them.
    fn read_on(&self, si: u16) -> Vec<u32> {
        self.read
            .iter()
            .filter(|(s, _, _)| *s == si)
            .map(|(_, tag, _)| *tag)
            .collect()
    }

    /// Every packet, as `normalised` rewrites it, folded into one FNV-1a
    /// digest with the tick it was sent at and whether the pipe lost it.
    fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for s in &self.log {
            let b = normalised(s.from_agent, &s.raw, &self.drawn);
            eat(&s.tick.to_be_bytes());
            eat(&[u8::from(s.from_agent), u8::from(s.lost)]);
            eat(&(b.len() as u64).to_be_bytes());
            eat(&b);
        }
        h
    }
}

/// A message on `stream`: `len` bytes whose first four are `tag`.
async fn write(stream: &Stream, tag: u32, len: usize) {
    let mut b = vec![0u8; len.max(4)];
    b[..4].copy_from_slice(&tag.to_be_bytes());
    stream
        .write_sctp(&Bytes::from(b), PayloadProtocolIdentifier::Binary)
        .await
        .expect("the queue has room");
}

/// The 32-bit field at `at`, rewritten by `f`.
fn rewrite32(b: &mut [u8], at: usize, f: impl Fn(u32) -> u32) {
    let v = u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    b[at..at + 4].copy_from_slice(&f(v).to_be_bytes());
}

/// One packet's bytes as they went on the wire, with what each association
/// drew at random rewritten (RFC 4960 §3, RFC 3758 §3.2): a verification tag
/// becomes whose it is, a TSN its offset from its sender's initial TSN, the
/// state cookie zeros, and the checksum, a function of the rest, zero. A
/// FORWARD TSN's stream entries are sorted: the association lists them from a
/// `HashMap`. Each chunk type the twin sends is walked; any other fails the
/// test, so no byte goes unread.
fn normalised(from_agent: bool, raw: &Bytes, d: &Drawn) -> Vec<u8> {
    let mut b = raw.to_vec();
    let role = |t: u32| match t {
        0 => 0,
        t if t == d.agent_tag => 1,
        t if t == d.viewer_tag => 2,
        t => panic!("a verification tag of neither end: {t}"),
    };
    let (own_tsn, peer_tsn) = if from_agent {
        (d.agent_tsn, d.viewer_tsn)
    } else {
        (d.viewer_tsn, d.agent_tsn)
    };
    rewrite32(&mut b, 4, role);
    rewrite32(&mut b, 8, |_| 0);
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
            // FORWARD TSN: the new cumulative TSN, and the stream entries in
            // a fixed order.
            192 => {
                rewrite32(&mut b, at + 4, |t| t.wrapping_sub(own_tsn));
                let mut entries: Vec<[u8; 4]> = b[at + 8..at + len]
                    .chunks_exact(4)
                    .map(|e| [e[0], e[1], e[2], e[3]])
                    .collect();
                entries.sort_unstable();
                for (i, e) in entries.iter().enumerate() {
                    b[at + 8 + i * 4..at + 12 + i * 4].copy_from_slice(e);
                }
            }
            t => panic!("the twin sends no chunk of type {t}"),
        }
        at += len.div_ceil(4) * 4;
    }
    b
}

/// The agent with one marked or unmarked `VIDEO` stream, one 1,200-byte
/// message a tick for `ticks` ticks, the pipe losing the agent's DATA packets
/// at the places `lost` names, then quiet.
async fn steady(marked: bool, lost: &[usize], ticks: u32) -> Twin {
    let mut twin = Twin::new().await;
    let video = twin.open(VIDEO, true, ReliabilityType::Rexmit, 0);
    if marked {
        assert!(twin.agent.mark_deadline_stream(VIDEO));
    }
    twin.loss = Loss::Data(lost.iter().copied().collect());
    for tag in 0..ticks {
        write(&video, tag, ONE_CHUNK).await;
        twin.step().await;
    }
    twin.settle(20_000).await;
    twin
}

// (a) The order the messages leave in.

/// Ordered C1 (two chunks), unordered V1, ordered C2, written in that order
/// and then sent: the agent's DATA chunks as (stream, first fragment, last
/// fragment), in TSN order, and what the viewer read.
async fn order(marked: bool) -> (Vec<(u16, bool, bool)>, Twin) {
    let mut twin = Twin::new().await;
    let control = twin.open(CONTROL, false, ReliabilityType::Reliable, 0);
    let video = twin.open(VIDEO, true, ReliabilityType::Rexmit, 0);
    if marked {
        assert!(twin.agent.mark_deadline_stream(VIDEO));
    }
    write(&control, 1, 2 * ONE_CHUNK).await;
    write(&video, 2, ONE_CHUNK).await;
    write(&control, 3, 100).await;
    twin.settle(5_000).await;

    let data = twin.data();
    let mut tsns: Vec<u32> = data.iter().map(|d| d.tsn).collect();
    tsns.dedup();
    assert!(
        tsns.windows(2).all(|w| sna32lt(w[0], w[1])),
        "the wire carries them in TSN order: {tsns:?}"
    );
    assert_eq!(twin.retransmissions().len(), 0);
    assert_eq!(twin.read_on(CONTROL), [1, 3], "the viewer read C1 and C2");
    assert_eq!(twin.read_on(VIDEO), [2], "and V1");
    let sent = data
        .iter()
        .map(|d| (d.sid, d.beginning, d.ending))
        .collect();
    (sent, twin)
}

const C1: [(u16, bool, bool); 2] = [(CONTROL, true, false), (CONTROL, false, true)];
const V1: (u16, bool, bool) = (VIDEO, true, true);
const C2: (u16, bool, bool) = (CONTROL, true, true);

#[tokio::test]
async fn test_deadline_order_without_a_mark_is_unordered_first() {
    let (sent, _) = order(false).await;
    assert_eq!(sent, [V1, C1[0], C1[1], C2], "today's order: V1, C1, C2");
}

#[tokio::test]
async fn test_deadline_order_with_a_mark_is_the_order_written() {
    let (sent, _) = order(true).await;
    assert_eq!(
        sent,
        [C1[0], C1[1], V1, C2],
        "the order written: C1, V1, C2"
    );
}

// (b) Loss on a marked Rexmit-0 stream.

#[tokio::test]
async fn test_deadline_nothing_lost_sends_no_forward_tsn() {
    let twin = steady(true, &[], 400).await;
    assert_eq!(twin.forward_tsns(), [], "no FORWARD TSN at all");
    assert_eq!(twin.retransmissions().len(), 0);
    assert_eq!(twin.read_on(VIDEO), (0..400).collect::<Vec<_>>());
    assert_eq!(twin.acked_bytes(), 400 * ONE_CHUNK as u64);
    assert_eq!(twin.gauges().forward_tsns, 0);
}

#[tokio::test]
async fn test_deadline_a_lost_chunk_is_skipped_by_one_forward_tsn() {
    let twin = steady(true, &[200], 400).await;
    let lost: Vec<Data> = twin.data().into_iter().filter(|d| d.lost).collect();
    assert_eq!(lost.len(), 1);
    let (lost_tsn, lost_tag) = (lost[0].tsn, lost[0].tag.expect("one chunk a message"));

    // One FORWARD TSN, which skips the hole and nothing still in flight.
    let forward = twin.forward_tsns();
    assert_eq!(
        forward.iter().map(|f| f.1).collect::<Vec<_>>(),
        [lost_tsn],
        "exactly one FORWARD TSN, to the lost chunk: {forward:?}"
    );
    assert!(
        forward[0].0 >= lost[0].tick + 2 * DELAY,
        "sent on the SACK, a round trip after the loss: {forward:?} {lost:?}"
    );

    // Nothing sent again, and the loss moved no window.
    assert_eq!(twin.retransmissions().len(), 0, "nothing retransmitted");
    let (ssthresh, cwnd) = twin.start;
    assert!(
        twin.windows.iter().all(|w| w.0 == ssthresh),
        "ssthresh held at {ssthresh}"
    );
    assert!(
        twin.windows.windows(2).all(|w| w[0].1 <= w[1].1) && twin.windows[0].1 >= cwnd,
        "cwnd never went down"
    );
    assert!(!twin.fast_recovery_seen, "no fast recovery");
    let g = twin.gauges();
    assert_eq!((g.fast_retransmits, g.t3_expiries), (0, 0));

    // The viewer skipped the hole and read everything else.
    assert_eq!(twin.viewer.peer_last_tsn, twin.agent.my_next_tsn - 1);
    assert!(twin.viewer.payload_queue.is_empty());
    let read = twin.read_on(VIDEO);
    assert_eq!(read.len(), 399);
    assert!(!read.contains(&lost_tag));
    // #3980's clock: the lost chunk is not delivered.
    assert_eq!(twin.acked_bytes(), 399 * ONE_CHUNK as u64);
    assert_eq!(g.forward_tsns, 1, "the gauge counts it");
}

// (c) T3-rtx on a Rexmit-0 stream.

/// Fifty video messages with nothing lost; then three video messages and a
/// control one, all lost, which no SACK reports, until T3-rtx expires.
async fn t3_scene(marked: bool) -> Twin {
    let mut twin = Twin::new().await;
    let control = twin.open(CONTROL, false, ReliabilityType::Reliable, 0);
    let video = twin.open(VIDEO, true, ReliabilityType::Rexmit, 0);
    if marked {
        assert!(twin.agent.mark_deadline_stream(VIDEO));
    }
    for tag in 0..50 {
        write(&video, tag, ONE_CHUNK).await;
        twin.step().await;
    }
    twin.settle(5_000).await;
    let quiet_forward_tsns = twin.forward_tsns().len();
    if marked {
        assert_eq!(quiet_forward_tsns, 0, "nothing lost, no FORWARD TSN");
    } else {
        assert!(quiet_forward_tsns > 0, "today: one after nearly every SACK");
    }

    let from = twin.data_packets;
    twin.loss = Loss::Data((from..from + 4).collect());
    for tag in 50..53 {
        write(&video, tag, ONE_CHUNK).await;
    }
    write(&control, 0, ONE_CHUNK).await;
    while twin.gauges().t3_expiries == 0 {
        assert!(twin.now < 10_000, "T3-rtx expires");
        twin.step().await;
    }
    twin.settle(10_000).await;

    let lost: Vec<Data> = twin.data().into_iter().filter(|d| d.lost).collect();
    assert_eq!(
        lost.iter().map(|d| (d.sid, d.tag)).collect::<Vec<_>>(),
        [
            (VIDEO, Some(50)),
            (VIDEO, Some(51)),
            (VIDEO, Some(52)),
            (CONTROL, Some(0))
        ]
    );
    twin
}

/// What both runs of (c) show alike.
fn assert_t3_scene(twin: &Twin) {
    let lost: Vec<Data> = twin.data().into_iter().filter(|d| d.lost).collect();
    let g = twin.gauges();
    assert_eq!(g.t3_expiries, 1);
    assert_eq!(
        twin.t3_windows[0].1, twin.agent.mtu,
        "the expiry cut the window to one MTU, as before"
    );
    // The video chunks are never sent again; the control chunk is, once.
    let again = twin.retransmissions();
    assert_eq!(
        again.iter().map(|d| (d.sid, d.tsn)).collect::<Vec<_>>(),
        [(CONTROL, lost[3].tsn)],
        "only the reliable chunk is retransmitted"
    );
    // A FORWARD TSN after the expiry skips the three video chunks.
    let expiry = again[0].tick;
    assert!(
        twin.forward_tsns()
            .iter()
            .any(|f| f.0 >= expiry && f.1 == lost[2].tsn),
        "{:?}",
        twin.forward_tsns()
    );
    // The viewer read all but the three lost video messages.
    assert_eq!(twin.read_on(VIDEO), (0..50).collect::<Vec<_>>());
    assert_eq!(twin.read_on(CONTROL), [0]);
    assert_eq!(twin.viewer.peer_last_tsn, twin.agent.my_next_tsn - 1);
    // #3980's clock leaves the three out: 50 video messages and the control one.
    assert_eq!(twin.acked_bytes(), 51 * ONE_CHUNK as u64);
}

#[tokio::test]
async fn test_deadline_t3_retransmits_no_marked_chunk() {
    let twin = t3_scene(true).await;
    assert_t3_scene(&twin);
    assert_eq!(
        twin.forward_tsns().len(),
        1,
        "the one FORWARD TSN is the expiry's"
    );
}

#[tokio::test]
async fn test_deadline_t3_without_a_mark_is_todays() {
    let twin = t3_scene(false).await;
    assert_t3_scene(&twin);
}

// (d) FORWARD TSN packets per SACK under a steady load (O-k's measure).

#[tokio::test]
async fn test_deadline_forward_tsns_per_sack_under_a_steady_load() {
    let lost: Vec<usize> = (1..=10).map(|i| i * 150).collect();
    let today = steady(false, &lost, 1600).await;
    let marked = steady(true, &lost, 1600).await;
    let (today_forward, today_sacks) = (today.forward_tsns().len(), today.sacks_taken());
    let (marked_forward, marked_sacks) = (marked.forward_tsns().len(), marked.sacks_taken());
    eprintln!(
        "FR-148 P4b-1 (d): FORWARD TSN packets per SACK, 1,600 messages, {} lost: \
         today {today_forward}/{today_sacks} = {:.3}, marked {marked_forward}/{marked_sacks} = {:.3}",
        lost.len(),
        today_forward as f64 / today_sacks as f64,
        marked_forward as f64 / marked_sacks as f64,
    );
    assert!(
        marked_forward * today_sacks < today_forward * marked_sacks,
        "marked sends fewer FORWARD TSNs per SACK than today"
    );
    assert_eq!(marked_forward, lost.len(), "one a loss");
    assert_eq!(marked.retransmissions().len(), 0);
    assert_eq!(marked.read_on(VIDEO).len(), 1600 - lost.len());
    assert_eq!(today.read_on(VIDEO).len(), 1600 - lost.len());
    // The gauge counts what went on the wire, marked or not.
    assert_eq!(today.gauges().forward_tsns, today_forward as u64);
    assert_eq!(marked.gauges().forward_tsns, marked_forward as u64);
}

// (f) Byte for byte: an association with no marked stream sends what it sent
// before.

/// What one seed's run exercised.
#[derive(Debug, Default)]
struct Exercised {
    packets: usize,
    data: usize,
    lost: usize,
    retransmitted: usize,
    sacks: usize,
    forward_tsns: usize,
    fast_retransmits: u64,
    t3_expiries: u64,
}

/// Four streams (ordered and unordered, reliable, `maxRetransmits: 0` and 2),
/// messages of 4 to 4,000 bytes on random ticks, and a pipe that loses agent
/// DATA packets, alone and in bursts, and viewer packets, all drawn from
/// `seed`; then quiet, with nothing lost.
async fn random_traffic(seed: u64) -> (u64, Exercised) {
    let mut twin = Twin::new().await;
    let streams = [
        twin.open(1, false, ReliabilityType::Reliable, 0),
        twin.open(2, true, ReliabilityType::Reliable, 0),
        twin.open(3, true, ReliabilityType::Rexmit, 0),
        twin.open(4, false, ReliabilityType::Rexmit, 2),
    ];
    let mut rng = XorShift(seed);
    twin.loss = Loss::Random {
        rng: XorShift(seed.rotate_left(17) | 1),
        data: 20,
        viewer: 10,
        burst: 3,
        in_burst: 0,
    };
    let mut written = [0u32; 4];
    for _ in 0..3_000 {
        if rng.below(100) < 40 {
            let s = rng.below(4) as usize;
            write(&streams[s], written[s], 4 + rng.below(3_997) as usize).await;
            written[s] += 1;
        }
        twin.step().await;
    }
    twin.loss = Loss::None;
    twin.settle(60_000).await;

    // The reliable streams delivered everything, the ordered one in order.
    assert_eq!(twin.read_on(1), (0..written[0]).collect::<Vec<_>>());
    let mut unordered = twin.read_on(2);
    unordered.sort_unstable();
    assert_eq!(unordered, (0..written[1]).collect::<Vec<_>>());

    let data = twin.data();
    let g = twin.gauges();
    let exercised = Exercised {
        packets: twin.log.len(),
        data: data.len(),
        lost: data.iter().filter(|d| d.lost).count(),
        retransmitted: twin.retransmissions().len(),
        sacks: twin.sacks_taken(),
        forward_tsns: twin.forward_tsns().len(),
        fast_retransmits: g.fast_retransmits,
        t3_expiries: g.t3_expiries,
    };
    (twin.digest(), exercised)
}

/// Each seed and the digest of its run on the crate as it was before FR-148
/// P4b-1. A change that moves what an unmarked association sends changes
/// these; the test prints the new ones.
const BEFORE: [(u64, u64); 4] = [
    (0x9e37_79b9_7f4a_7c15, 0x5c68_2abb_2769_f09e),
    (0x2545_f491_4f6c_dd1d, 0x9351_eb2d_8b5a_80ca),
    (0x5851_f42d_4c95_7f2d, 0x49fa_7741_8743_63b3),
    (0x1405_7b7e_f767_814f, 0x0c9e_d382_b43c_c2e5),
];

#[tokio::test]
async fn test_deadline_an_unmarked_association_sends_what_it_sent_before() {
    let mut differ = vec![];
    let mut total = Exercised::default();
    for (seed, before) in BEFORE {
        let (digest, e) = random_traffic(seed).await;
        eprintln!("FR-148 P4b-1 (f): seed {seed:#018x} digest {digest:#018x}: {e:?}");
        if digest != before {
            differ.push((seed, digest));
        }
        assert!(
            e.lost > 0 && e.retransmitted > 0 && e.forward_tsns > 0,
            "{e:?}"
        );
        total.packets += e.packets;
        total.fast_retransmits += e.fast_retransmits;
        total.t3_expiries += e.t3_expiries;
    }
    assert!(
        total.fast_retransmits > 0 && total.t3_expiries > 0,
        "the seeds reach fast retransmission and T3-rtx: {total:?}"
    );
    assert!(
        differ.is_empty(),
        "what an unmarked association sends changed: (seed, digest) {differ:x?}"
    );
}

// The mark's own rules.

#[tokio::test]
async fn test_deadline_a_mark_needs_its_stream() {
    let mut twin = Twin::new().await;
    assert!(
        !twin.agent.mark_deadline_stream(VIDEO),
        "no such stream yet"
    );
    let video = twin.open(VIDEO, true, ReliabilityType::Rexmit, 0);
    // Nothing was marked: the association still sends today's FORWARD TSNs.
    for tag in 0..50 {
        write(&video, tag, ONE_CHUNK).await;
        twin.step().await;
    }
    twin.settle(5_000).await;
    assert!(!twin.forward_tsns().is_empty());
    assert!(twin.agent.mark_deadline_stream(VIDEO));
    assert!(
        twin.agent.mark_deadline_stream(VIDEO),
        "marking again is a no-op"
    );
}

// roomler (FR-148 P4b): the read of the marks names the stream marked and
// no other, on the end that marked it.
#[tokio::test]
async fn test_deadline_streams_names_the_stream_marked_and_no_other() {
    let mut twin = Twin::new().await;
    assert!(twin.agent.deadline_streams().is_empty(), "no stream yet");
    let _control = twin.open(CONTROL, false, ReliabilityType::Reliable, 0);
    let _video = twin.open(VIDEO, true, ReliabilityType::Rexmit, 0);
    assert!(
        twin.agent.deadline_streams().is_empty(),
        "opened, never marked"
    );
    assert!(
        !twin.agent.mark_deadline_stream(VIDEO + 2),
        "no such stream"
    );
    assert!(
        twin.agent.deadline_streams().is_empty(),
        "a refusal marks nothing"
    );
    assert!(twin.agent.mark_deadline_stream(VIDEO));
    assert_eq!(twin.agent.deadline_streams(), [VIDEO]);
    assert!(twin.agent.mark_deadline_stream(VIDEO));
    assert_eq!(twin.agent.deadline_streams(), [VIDEO], "marked once");
    assert!(twin.agent.mark_deadline_stream(CONTROL));
    assert_eq!(twin.agent.deadline_streams(), [CONTROL, VIDEO], "in order");
    assert!(
        twin.viewer.deadline_streams().is_empty(),
        "the far end marked nothing"
    );
}
