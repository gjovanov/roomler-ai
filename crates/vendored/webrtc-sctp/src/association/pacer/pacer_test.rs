//! roomler (FR-148 P0, D43): the pacer's gate, on made-up times and windows.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;

const MS: Duration = Duration::from_millis(1);

/// A pacer, its switch, and the write loop's wake channel.
fn pacer() -> (Pacer, PaceSwitch, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel(1);
    let p = Pacer::default();
    let s = p.switch(Arc::new(tx));
    (p, s, rx)
}

/// How many chunks of 1,200 bytes the pacer lets out now, the first with
/// `inflight` bytes in flight and each after it with 1,200 more.
fn leave(p: &mut Pacer, inflight: usize) -> usize {
    let mut n = 0;
    while n < 1_000 && p.admit(1_200, inflight + n * 1_200) {
        n += 1;
    }
    n
}

#[test]
fn off_it_gates_nothing_and_keeps_nothing() {
    let (mut p, _s, _rx) = pacer();
    p.on_rtt(80 * MS);
    assert!(!p.begin(Instant::now(), 160_000, u32::MAX));
    assert!(p.admit(1_200, 1 << 30));
    assert_eq!(p.take_wake(), None);
    assert!(p.run.is_none());
}

#[test]
fn before_a_round_trip_it_gates_nothing() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    assert!(!p.begin(Instant::now(), 160_000, u32::MAX));
    assert_eq!(p.take_wake(), None);
}

/// In slow start, 500 thousandths of 160,000 bytes over 80 ms: 1 MB/s, so
/// the 2 ms of tokens a send starts with (2,000 bytes) let two chunks out.
/// The third waits until the 400 bytes the second overdrew, and one more,
/// have come: 401 µs at 1 MB/s.
#[test]
fn slow_start_lets_out_the_steered_share_of_the_window() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    s.steer(500, None);
    p.on_rtt(80 * MS);
    let t0 = Instant::now();
    assert!(p.begin(t0, 160_000, u32::MAX));
    assert_eq!(leave(&mut p, 0), 2);
    assert_eq!(p.take_wake(), Some(t0 + Duration::from_micros(401)));
    assert_eq!(p.take_wake(), None, "taking the wake clears it");
}

/// Above the slow start threshold the whole window, whatever the share:
/// 160,000 bytes over 80 ms is 2 MB/s, 4,000 bytes of tokens, four chunks.
#[test]
fn above_the_threshold_it_lets_out_the_whole_window() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    s.steer(250, None);
    p.on_rtt(80 * MS);
    assert!(p.begin(Instant::now(), 160_000, 100_000));
    assert_eq!(leave(&mut p, 0), 4);
}

/// The rate is over the least round trip: a longer sample changes nothing.
/// 80,000 bytes over 40 ms is 2 MB/s, four chunks.
#[test]
fn the_rate_is_over_the_least_round_trip() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    p.on_rtt(40 * MS);
    p.on_rtt(80 * MS);
    assert!(p.begin(Instant::now(), 80_000, 40_000));
    assert_eq!(leave(&mut p, 0), 4);
}

/// A pause saves up no burst: 50 ms with nothing sent leaves 2 ms of the
/// rate in tokens, not 50.
#[test]
fn a_pause_saves_up_two_milliseconds() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    p.on_rtt(80 * MS);
    let t0 = Instant::now();
    assert!(p.begin(t0, 160_000, 100_000));
    assert!(p.begin(t0 + 50 * MS, 160_000, 100_000));
    assert_eq!(leave(&mut p, 0), 4);
}

/// A chunk waited for its tokens and its wake came 30 ms late: what came due
/// leaves, up to 17 ms of the rate. At 2 MB/s that is 34,000 bytes: 29
/// chunks, the last overdrawing by 800.
#[test]
fn a_late_wake_lets_out_seventeen_milliseconds() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    p.on_rtt(80 * MS);
    let t0 = Instant::now();
    assert!(p.begin(t0, 160_000, 100_000));
    assert_eq!(leave(&mut p, 0), 4);
    assert_eq!(p.take_wake(), Some(t0 + Duration::from_micros(401)));
    assert!(p.begin(t0 + 30 * MS, 160_000, 100_000));
    assert_eq!(leave(&mut p, 0), 29);
}

/// While in flight is at or above the cap, new data waits for a SACK: it
/// spends no tokens and arms no wake. Below the cap the tokens decide.
#[test]
fn the_cap_holds_new_data_while_in_flight_is_at_or_above_it() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    s.steer(WHOLE, Some(12_000));
    p.on_rtt(80 * MS);
    assert!(p.begin(Instant::now(), 160_000, 100_000));
    assert!(!p.admit(1_200, 12_000));
    assert!(!p.admit(1_200, 13_200));
    assert_eq!(p.take_wake(), None);
    // The held chunks spent nothing: of the four chunks' worth of tokens,
    // two fit under the cap from 9,600 bytes in flight, and the other two
    // leave from an empty flight.
    assert_eq!(leave(&mut p, 9_600), 2);
    assert_eq!(leave(&mut p, 0), 2);
}

/// A cap that rises, or goes, wakes the write loop, so what waited for it
/// leaves; one that is set, falls or stays does not.
#[test]
fn a_cap_that_rises_or_goes_wakes_the_write_loop() {
    let (_p, s, mut rx) = pacer();
    s.set(true);
    assert!(rx.try_recv().is_err(), "turning on wakes nothing");
    s.steer(WHOLE, Some(12_000));
    assert!(rx.try_recv().is_err(), "a cap set where there was none");
    s.steer(WHOLE, Some(24_000));
    assert!(rx.try_recv().is_ok(), "a cap that rose");
    s.steer(WHOLE, Some(24_000));
    assert!(rx.try_recv().is_err(), "a cap that stayed");
    s.steer(WHOLE, Some(18_000));
    assert!(rx.try_recv().is_err(), "a cap that fell");
    s.steer(WHOLE, None);
    assert!(rx.try_recv().is_ok(), "a cap that went");
}

/// Turned off, the pacer wakes the write loop so what it held leaves, and
/// forgets how it was steered and what it kept: turned on again, it starts
/// at the whole window with no cap and waits for a round trip.
#[test]
fn turned_off_it_wakes_the_write_loop_and_forgets() {
    let (mut p, s, mut rx) = pacer();
    s.set(true);
    s.steer(300, Some(12_000));
    p.on_rtt(80 * MS);
    assert!(p.begin(Instant::now(), 160_000, u32::MAX));
    s.set(false);
    assert!(rx.try_recv().is_ok());
    assert!(!p.begin(Instant::now(), 160_000, u32::MAX));
    assert!(p.run.is_none());
    assert_eq!((s.share(), s.cap()), (WHOLE, None));
    s.set(true);
    assert!(rx.try_recv().is_err());
    let t1 = Instant::now();
    assert!(!p.begin(t1, 160_000, u32::MAX), "no round trip yet");
    p.on_rtt(80 * MS);
    assert!(p.begin(t1 + 2 * MS, 160_000, u32::MAX));
    // The whole window, 2 MB/s: the 2 ms since the send before carry 4,000
    // bytes, and no cap holds them however much is in flight. At the 300
    // thousandths it was steered to before, one chunk would leave.
    assert_eq!(leave(&mut p, 1 << 20), 4);
}

/// The tokens build from the send before, even one before the first round
/// trip: the first paced send after a sample lets out what came due since,
/// not a whole burst. 1 ms at 2 MB/s is 2,000 bytes: two chunks.
#[test]
fn the_tokens_build_from_the_send_before() {
    let (mut p, s, _rx) = pacer();
    s.set(true);
    let t0 = Instant::now();
    assert!(!p.begin(t0, 160_000, 100_000), "no round trip yet");
    p.on_rtt(80 * MS);
    assert!(p.begin(t0 + MS, 160_000, 100_000));
    assert_eq!(leave(&mut p, 0), 2);
}

/// A switch of no association keeps what it is set and steered to, within
/// the share's bounds, and reaches nothing.
#[test]
fn a_switch_of_no_association_reaches_nothing() {
    let s = PaceSwitch::default();
    assert!(!s.is_on());
    s.set(true);
    s.steer(0, Some(4_800));
    assert!(s.is_on());
    assert_eq!((s.share(), s.cap()), (1, Some(4_800)));
    s.steer(5_000, Some(9_600));
    assert_eq!((s.share(), s.cap()), (WHOLE, Some(9_600)));
    s.set(false);
    assert!(!s.is_on());
    assert!(!s.same(&PaceSwitch::default()));
    assert!(s.same(&s.clone()));
}
