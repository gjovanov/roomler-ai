---
paths:
  - "crates/tests/**"
  - "ui/e2e/**"
  - "ui/playwright.config.*"
  - "scripts/e2e-*.sh"
  - "scripts/e2e-expected-failures.txt"
  - ".github/workflows/integration-tests.yml"
---

# Test harnesses — the integration lane and the E2E rig

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps how to run each suite. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

Suites, harnesses and the nightly lane: `docs/testing.md`. The counts below were true when written; the lane asserts its own minimum.

## Test Setup

Suites, harnesses and the nightly lane: **`docs/testing.md`**.

- **Integration** (`crates/tests/`, ~294 tests across 34 modules): a unique
  UUID-named database per test, real Axum servers on random ports. Needs MongoDB
  `:27019` + Redis `:6379`. Run with `RUST_MIN_STACK=8388608` — several tests build
  seven servers in one body and overflow the harness's default 2 MiB thread
  otherwise, and a `SIGABRT` with `has overflowed its stack` is what a perfectly
  green suite looks like. Set `RUST_LOG` and `--nocapture`; without a subscriber
  every `info!`/`warn!` explaining a refusal is discarded.
- **CI runs this lane** — `.github/workflows/integration-tests.yml`, on master
  pushes touching `crates/**` or `agents/roomlerd/**`, on PRs touching
  `crates/tests/**`, and on dispatch (optional `filter`). Deliberately **not** on
  every PR: a ~20 min job on every push trains people to ignore it.
  ⚠️ The lane asserts a **minimum test count** and a **leaked-database ceiling**,
  because a job that silently runs nothing is indistinguishable from a passing one
  — `cargo test` reports success for a filter matching no test. Treat the count as
  approximate: `pull_request` checks out the branch *merged with master* while a
  dispatch checks out the branch alone, so two green runs of one commit differ.
  ⚠️ The two skips were **measured on the runner**, not inherited from the build
  host: `conference_tests::call_leave_cleans_up_participant_media` (no mediasoup
  worker there) and `rate_limit_tests::rate_limit_returns_429_after_burst`
  (timing-sensitive). A third skip should feel expensive.
- **E2E** (`ui/e2e/`, Playwright, 24 specs). Set `E2E_API_URL` (defaults to the dev
  port `:5001`) or every API-driven spec fails ECONNREFUSED; Mailpit specs need
  `E2E_MAILPIT_URL`. 🔑 **The browser runs as the `pwrunner` sidecar inside the app
  pod** — this host has no route to the pod network (a port-forward carries one TCP
  port and no RTP ever arrives), a Service URL is not a secure context so
  `navigator.mediaDevices` is undefined, and in the pod the app is
  `http://127.0.0.1`, which is both a secure context and the address mediasoup
  announces. A Job cannot replace the sidecar — it cannot share the app's network
  namespace, which is the whole mechanism. ⚠️ `ROOMLER__APP__FRONTEND_URL` must
  equal the browser's origin (**a port is part of an Origin**) or the
  cookie-authenticated `/ws` upgrade is refused 403 and every realtime spec fails
  for that one reason. ⚠️ An entry in `e2e-expected-failures.txt` is a claim that
  you understand the failure — re-test it or delete it.
- **Unit**: Vitest (`ui/src/`), plus in-crate `#[cfg(test)]` tests in
  `remote_control`, `roomlerd` and `services::auth`.
  `roomlerd`'s CLI tests (`src/app.rs`, the former `main.rs`) run under `--lib` as
  `app::tests` since FR-123 P2; before that the bin's tests ran in no lane.
