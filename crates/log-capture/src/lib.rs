// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! The one way a test reads its own log lines (#3403).
//!
//! A test that asserts on what it logged installs a subscriber as its own
//! thread's default, for as long as it reads, and nothing else. This crate is
//! the only code that does that. Every capture in the workspace goes through
//! it, and the root `clippy.toml` refuses the four scoped-default entry points
//! of `tracing` anywhere else (`docs/testing.md`, "Reading a test's own log
//! lines").
//!
//! # Why a capture holds TWO registered dispatchers
//!
//! tracing caches a callsite's interest once per process, when the callsite is
//! first reached. A capture is per thread. With exactly one dispatcher
//! registered, tracing-core 0.1.36 builds a newly reached callsite's interest
//! from the default of whichever thread reached it first (`Rebuilder::JustOne`,
//! `callsite.rs:564`), and caches that answer for every thread. When that
//! thread belongs to a test running beside the capture and has no subscriber of
//! its own, the answer is `never`, and the capture misses that line for good,
//! though its own thread logged it.
//!
//! - **With two registered,** every registration asks every registered
//!   dispatcher (`has_just_one` is false: `callsite.rs:555`), so the capture's
//!   subscriber always has its say. A capture made here holds a second
//!   registered dispatcher for its whole life ([`Scope`]), and drops its scope
//!   before that second dispatcher.
//! - **[`tracing::Dispatch::none`] is no second dispatcher:** it never
//!   registers (`dispatcher.rs:463`). The probe test
//!   (`tests/another_thread_first.rs`) turns red with it.
//! - **Every line is exposed in CI.** Without `RUST_LOG`, the integration lane
//!   installs no global subscriber. tracing's max level then starts at `OFF`,
//!   and the event macros check it before they register a callsite, so no line
//!   in the process registers before the first capture does. A developer who
//!   runs with `RUST_LOG` set never sees the race.
//! - **Measured:**
//!   - #3317's extauth capture lost `extauth: login VERIFIED by the device` in
//!     two runs, while a test beside it logged in.
//!   - #4351: the integration lane's timeout-line test captured 0 of its 2
//!     lines while a test beside it timed out on the same `warn!`.
//!   - The node core's config save tests lost lines the same way.
//!
//! Each of those was fixed by hand, in its own copy and its own long comment,
//! until this crate replaced them. If a tracing upgrade changes this
//! behaviour, this file is the only code to change, and the probe test says
//! whether it must.
//!
//! # The positive control
//!
//! A capture that holds nothing passes every "this line is absent" assertion.
//! So a test that asserts a line is absent first asserts, with [`saw`] or
//! [`Captured::saw`], a line its own thread logged on the same path, and then
//! reads the absence from that same text. An empty capture then fails instead
//! of passing.
//!
//! # Shapes
//!
//! - [`with_default`]: `subscriber` for one closure, the common case.
//! - [`set_default`]: a [`Scope`] guard, held for as long as the test reads
//!   (an async test, or a capture kept in a struct).
//! - [`with_dispatch`] / [`set_dispatch`]: the same, for a test that already
//!   holds a [`tracing::Dispatch`].
//! - [`with_default_async`]: for a future, the default each time it is
//!   polled (tracing's `WithSubscriber`), never a guard held across an
//!   `.await`, which would lend the capture to whatever else that thread polls.
//! - [`Captured`] with [`fmt`] or [`layer`]: the plain-text buffer most
//!   captures read, with the fmt subscriber or layer that writes into it.

use std::io;
use std::sync::{Arc, Mutex, PoisonError};

use tracing::{Dispatch, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Format};
use tracing_subscriber::fmt::{MakeWriter, SubscriberBuilder};
use tracing_subscriber::registry::LookupSpan;

/// A capture's life: its subscriber is this thread's default until this
/// drops, beside a second registered dispatcher (see the crate docs).
///
/// It drops the scope first and the second dispatcher after it, so for every
/// moment the scope is this thread's default, two dispatchers are registered.
#[must_use = "the capture ends when this drops"]
pub struct Scope {
    scope: Option<tracing::dispatcher::DefaultGuard>,
    _second: Dispatch,
}

impl Drop for Scope {
    fn drop(&mut self) {
        // The scope first. The second dispatcher drops after this body.
        drop(self.scope.take());
    }
}

/// The second registered dispatcher a capture holds beside its own. A
/// `NoSubscriber` wants no line and keeps none from another dispatcher: it
/// only keeps two registered while the capture lives (see the crate docs).
fn second() -> Dispatch {
    Dispatch::new(tracing::subscriber::NoSubscriber::default())
}

/// `subscriber` as this thread's default until the returned [`Scope`] drops,
/// beside a second registered dispatcher.
pub fn set_default<S>(subscriber: S) -> Scope
where
    S: Subscriber + Send + Sync + 'static,
{
    let second = second();
    scoped(&Dispatch::new(subscriber), second)
}

/// As [`set_default`], for a test that already holds its [`Dispatch`].
pub fn set_dispatch(dispatch: &Dispatch) -> Scope {
    scoped(dispatch, second())
}

/// Runs `f` with `subscriber` as this thread's default, beside a second
/// registered dispatcher, and returns what `f` returns.
pub fn with_default<S, T>(subscriber: S, f: impl FnOnce() -> T) -> T
where
    S: Subscriber + Send + Sync + 'static,
{
    let _scope = set_default(subscriber);
    f()
}

/// As [`with_default`], for a test that already holds its [`Dispatch`].
pub fn with_dispatch<T>(dispatch: &Dispatch, f: impl FnOnce() -> T) -> T {
    let _scope = set_dispatch(dispatch);
    f()
}

/// As [`with_default`], for a future: `dispatch` (a subscriber, or a
/// [`Dispatch`]) is the default each time the future is polled, as tracing's
/// `WithSubscriber` makes it, beside a second registered dispatcher held
/// until the future completes. A task the future spawns logs to nobody,
/// unless it carries the capture too.
pub async fn with_default_async<D, F>(dispatch: D, f: F) -> F::Output
where
    D: Into<Dispatch>,
    F: Future,
{
    let _second = second();
    polled(f, dispatch.into()).await
}

fn scoped(dispatch: &Dispatch, second: Dispatch) -> Scope {
    Scope {
        scope: Some(tracing::dispatcher::set_default(dispatch)),
        _second: second,
    }
}

/// `f`, polled with `dispatch` as its thread's default each time.
fn polled<F: Future>(f: F, dispatch: Dispatch) -> tracing::instrument::WithDispatch<F> {
    tracing::instrument::WithSubscriber::with_subscriber(f, dispatch)
}

/// What a capture's subscriber wrote, as text: a shared buffer, and the
/// [`MakeWriter`] that hands it to [`fmt`], [`layer`] or any fmt builder.
/// Clones share one buffer.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// Everything the capture holds so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }

    /// The positive control: panics unless the capture holds `line`, a line
    /// this thread logged. Returns the text it read, so an absence assertion
    /// reads the same text.
    #[track_caller]
    pub fn saw(&self, line: &str) -> String {
        let text = self.text();
        saw(&text, line);
        text
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

/// The positive control, for a capture already read as text: panics unless
/// `text` holds `line`, a line this thread logged. Assert one before asserting
/// that a line is absent, or an empty capture passes (see the crate docs).
#[track_caller]
pub fn saw(text: &str, line: &str) {
    if text.contains(line) {
        return;
    }
    if text.is_empty() {
        panic!(
            "the capture never saw {line:?}: it holds NOTHING, so no line this thread logged \
             reached its subscriber"
        );
    }
    panic!("the capture never saw {line:?}. It holds:\n{text}");
}

/// The fmt subscriber builder a buffer capture starts from: plain text, never
/// coloured, written to `writer` (often a [`Captured`]). Each test adds its
/// own format (`with_target(false)`, `without_time()`, a level) and calls
/// `finish()`.
#[expect(
    clippy::disallowed_methods,
    reason = "a test's buffer capture, never a console, so never coloured (root clippy.toml, #3938)"
)]
pub fn fmt<W>(writer: W) -> SubscriberBuilder<DefaultFields, Format, LevelFilter, W>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
}

/// As [`fmt`], as a layer, for a capture built on a `registry()`.
#[expect(
    clippy::disallowed_methods,
    reason = "a test's buffer capture, never a console, so never coloured (root clippy.toml, #3938)"
)]
pub fn layer<S, W>(writer: W) -> tracing_subscriber::fmt::Layer<S, DefaultFields, Format, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + 'static,
{
    tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line logged while the scope lives is captured. One logged after the
    /// guard drops is not: the scope ended with it.
    #[test]
    fn a_scope_captures_until_it_drops() {
        let log = Captured::default();
        let scope = set_default(fmt(log.clone()).without_time().finish());
        tracing::warn!("inside the scope");
        drop(scope);
        tracing::warn!("after the scope");
        let text = log.saw("inside the scope");
        assert!(!text.contains("after the scope"), "{text}");
    }

    /// The closure forms return what the closure returns.
    #[test]
    fn with_default_and_with_dispatch_return_the_closures_value() {
        let log = Captured::default();
        let n = with_default(fmt(log.clone()).finish(), || {
            tracing::warn!("closure form");
            7
        });
        assert_eq!(n, 7);
        log.saw("closure form");

        let log = Captured::default();
        let dispatch = Dispatch::new(fmt(log.clone()).finish());
        let n = with_dispatch(&dispatch, || {
            tracing::warn!("dispatch form");
            8
        });
        assert_eq!(n, 8);
        log.saw("dispatch form");
    }

    /// The future form captures what the future logs while it is polled, and
    /// returns its output. A line logged after it completed is not captured.
    #[test]
    fn with_default_async_captures_while_the_future_is_polled() {
        fn block_on<F: Future>(f: F) -> F::Output {
            let mut f = std::pin::pin!(f);
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            loop {
                if let std::task::Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                    return out;
                }
            }
        }

        let log = Captured::default();
        let n = block_on(with_default_async(fmt(log.clone()).finish(), async {
            tracing::warn!("polled form");
            9
        }));
        assert_eq!(n, 9);
        tracing::warn!("after the future");
        let text = log.saw("polled form");
        assert!(!text.contains("after the future"), "{text}");

        let log = Captured::default();
        let dispatch = Dispatch::new(fmt(log.clone()).finish());
        block_on(with_default_async(dispatch, async {
            tracing::warn!("polled with a dispatch");
        }));
        log.saw("polled with a dispatch");
    }

    /// A buffer capture is never coloured.
    #[test]
    fn fmt_and_layer_write_plain_text() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let log = Captured::default();
        with_default(fmt(log.clone()).finish(), || tracing::warn!(k = 1, "plain"));
        let text = log.saw("plain k=1");
        assert!(!text.contains('\u{1b}'), "{text:?}");

        let log = Captured::default();
        with_default(
            tracing_subscriber::registry().with(layer(log.clone())),
            || tracing::warn!(k = 2, "layered"),
        );
        let text = log.saw("layered k=2");
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    /// The positive control fails an empty capture, and says it was empty.
    #[test]
    #[should_panic(expected = "it holds NOTHING")]
    fn saw_fails_an_empty_capture() {
        saw("", "a line");
    }

    /// The positive control fails a capture that lacks the line, and shows
    /// what it held.
    #[test]
    #[should_panic(expected = "It holds:\nsomething else")]
    fn saw_fails_a_capture_without_the_line() {
        saw("something else", "a line");
    }
}
