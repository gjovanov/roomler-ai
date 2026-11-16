// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-28 — the control socket's redial on the load balancer's 502, through the
//! real `signaling::run`. Enrolled against a real server; then the loop's `ws_url`
//! points at a scripted front, because what this asks is what the agent's own loop
//! does with each answer, and a real server cannot be told to answer 502.
//!
//! The ladder these read is 1, 2, 4, 8 s, so dials land at 0, 1, 3 and 7 s. Flat
//! waits are 0.5 to 1 s. Every count is taken from the first dial, because the loop
//! warms the encoder probe before it dials at all.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use roomlerd::{config::AgentConfig, signaling, ws_connect::FlatRedial};

use crate::agent_tests::{enrol_via_agent_lib, spawn_agent_signaling_as};
use crate::fixtures::seed::SeededTenant;
use crate::fixtures::test_app::TestApp;

/// What the scripted front does with one dial.
#[derive(Clone, Copy)]
enum Answer {
    /// Refuse the upgrade with this HTTP status line.
    Status(&'static str),
    /// Upgrade, read the hello, keep the socket this long, then drop it with no
    /// close: a pod that accepted and died, as the agent hears it.
    AcceptHello(Duration),
}

const LB_502: Answer = Answer::Status("502 Bad Gateway");

type Dials = Arc<Mutex<Vec<Instant>>>;

/// The front load balancer in one process: `answer` picks the reply from the
/// dial's index. Hands back the `ws://…/ws` URL and when each dial arrived.
async fn scripted_front(answer: impl Fn(usize) -> Answer + Send + 'static) -> (String, Dials) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let dials: Dials = Arc::default();
    let seen = dials.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(Instant::now());
                seen.len() - 1
            };
            let this = answer(n);
            tokio::spawn(async move {
                match this {
                    Answer::Status(line) => {
                        let mut buf = [0u8; 4096];
                        let _ = s.read(&mut buf).await;
                        let reply = format!(
                            "HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        let _ = s.write_all(reply.as_bytes()).await;
                    }
                    Answer::AcceptHello(keep) => {
                        let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
                        let hello = ws.next().await;
                        assert!(
                            matches!(hello, Some(Ok(_))),
                            "the loop sends its hello first: {hello:?}"
                        );
                        tokio::time::sleep(keep).await;
                        drop(ws);
                    }
                }
            });
        }
    });
    (url, dials)
}

async fn agent(app: &TestApp, seeded: &SeededTenant, machine: &str) -> AgentConfig {
    enrol_via_agent_lib(app, seeded, machine, machine).await
}

/// The real control loop dialing `url`, with `flat`.
fn dialing(
    mut cfg: AgentConfig,
    url: String,
    flat: FlatRedial,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Sender<bool>,
) {
    cfg.ws_url = Some(url);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let ctx = signaling::OrgCtx::primary(roomlerd::token_slot::TokenSlot::new(
        cfg.agent_token.clone(),
    ))
    .with_flat_redial(flat);
    (spawn_agent_signaling_as(ctx, cfg, stop_rx), stop_tx)
}

fn count(dials: &Dials) -> usize {
    dials.lock().unwrap().len()
}

/// Hold until `n` dials have arrived; a loop that stops dialing fails here.
async fn until_dialled(dials: &Dials, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while count(dials) < n {
        assert!(
            Instant::now() < deadline,
            "{} of {n} dials in 120 s",
            count(dials)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// How many dials `window` after the first one.
async fn dials_after(dials: &Dials, window: Duration) -> usize {
    until_dialled(dials, 1).await;
    let first = dials.lock().unwrap()[0];
    tokio::time::sleep_until((first + window).into()).await;
    count(dials)
}

/// The wait between dial `n` and the one after it.
fn gap_after(dials: &Dials, n: usize) -> Duration {
    let dials = dials.lock().unwrap();
    dials[n + 1] - dials[n]
}

/// Off is today: a 502 climbs the ladder. This is the line the switch must not
/// move on a default build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_a_502_climbs_the_ladder() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-off").await;
    let cfg = agent(&app, &seeded, "mach-redial-off").await;
    let (url, dials) = scripted_front(|_| LB_502).await;
    let (task, _stop) = dialing(cfg, url, FlatRedial::off());
    let seen = dials_after(&dials, Duration::from_secs(5)).await;
    task.abort();
    assert_eq!(seen, 3, "0, 1 and 3 s, and the next at 7 s");
}

/// On, the same front gets a dial at least every second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_502_is_redialled_flat() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-on").await;
    let cfg = agent(&app, &seeded, "mach-redial-on").await;
    let (url, dials) = scripted_front(|_| LB_502).await;
    let (task, _stop) = dialing(cfg, url, FlatRedial::on());
    let seen = dials_after(&dials, Duration::from_secs(5)).await;
    task.abort();
    assert!(seen >= 5, "at most 1 s apart, so 5 or more in 5 s: {seen}");
    for n in 0..seen - 1 {
        let gap = gap_after(&dials, n);
        assert!(gap <= Duration::from_millis(1500), "dial {n}: {gap:?}");
    }
}

/// Only the 502 is flat: a Mongo blip (500) and an unmounted module (503) keep
/// the ladder, with the switch on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_every_other_answer_climbs_the_ladder() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-other").await;
    let mut runs = Vec::new();
    for line in ["500 Internal Server Error", "503 Service Unavailable"] {
        let cfg = agent(&app, &seeded, &format!("mach-redial-{}", &line[..3])).await;
        let (url, dials) = scripted_front(move |_| Answer::Status(line)).await;
        let (task, stop) = dialing(cfg, url, FlatRedial::on());
        runs.push((line, dials, task, stop));
    }
    for (line, dials, task, _stop) in runs {
        let seen = dials_after(&dials, Duration::from_secs(5)).await;
        task.abort();
        assert_eq!(seen, 3, "{line} must climb");
    }
}

/// The allowance is the bound on a 502 that never ends: two flat redials, then
/// the ladder from the step it stood on. The fifth dial lands by 5 s at the latest
/// and the sixth at 8 s at the earliest; still flat would be 7 or more by now, and
/// never flat would be 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_502_that_outlasts_the_allowance_climbs_the_ladder() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-allowance").await;
    let cfg = agent(&app, &seeded, "mach-redial-allowance").await;
    let (url, dials) = scripted_front(|_| LB_502).await;
    let flat = FlatRedial::with_limits(2, Duration::from_secs(30));
    let (task, _stop) = dialing(cfg, url, flat);
    let seen = dials_after(&dials, Duration::from_millis(6500)).await;
    task.abort();
    assert_eq!(seen, 5);
}

/// #2221 binds with the switch on: a server that accepts and drops at once must
/// climb, or a fleet reconnects to it every second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_server_that_accepts_and_drops_climbs_the_ladder() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-drop").await;
    let cfg = agent(&app, &seeded, "mach-redial-drop").await;
    let (url, dials) = scripted_front(|_| Answer::AcceptHello(Duration::ZERO)).await;
    let (task, _stop) = dialing(cfg, url, FlatRedial::on());
    let seen = dials_after(&dials, Duration::from_secs(5)).await;
    task.abort();
    assert_eq!(seen, 3, "0, 1 and 3 s, and the next at 7 s");
}

/// And that server earns no more flat redials. The one allowed goes on the first
/// 502; the session after it does not last; so the next 502 takes the ladder's
/// 2 s. Refilled, it would have waited at most 1 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_session_that_does_not_last_refills_nothing() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-quick").await;
    let cfg = agent(&app, &seeded, "mach-redial-quick").await;
    let (url, dials) = scripted_front(|n| match n {
        1 => Answer::AcceptHello(Duration::ZERO),
        _ => LB_502,
    })
    .await;
    let flat = FlatRedial::with_limits(1, Duration::from_secs(30));
    let (task, _stop) = dialing(cfg, url, flat);
    until_dialled(&dials, 4).await;
    task.abort();
    let gap = gap_after(&dials, 2);
    assert!(gap >= Duration::from_millis(1800), "{gap:?}");
}

/// A session that lived gives the next outage its redials back. The bar is 300 ms
/// here so this does not wait out the real one's 30 s. Unrefilled, this 502 would
/// have waited the ladder's 2 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_a_session_that_lived_refills_the_allowance() {
    let app = TestApp::spawn().await;
    let seeded = app.seed_tenant("redial-lived").await;
    let cfg = agent(&app, &seeded, "mach-redial-lived").await;
    let (url, dials) = scripted_front(|n| match n {
        1 => Answer::AcceptHello(Duration::from_millis(600)),
        _ => LB_502,
    })
    .await;
    let flat = FlatRedial::with_limits(1, Duration::from_millis(300));
    let (task, _stop) = dialing(cfg, url, flat);
    until_dialled(&dials, 4).await;
    task.abort();
    let gap = gap_after(&dials, 2);
    assert!(gap <= Duration::from_millis(1500), "{gap:?}");
}
