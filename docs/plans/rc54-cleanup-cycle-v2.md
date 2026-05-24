# rc.54 cleanup cycle v2 — wizard probe, TLS unify, structured attention, hub race verify

> **Status**: v2 GO. Folds in all 12 issues from the independent critique
> (`docs/plans/rc54-critique.md`, dated 2026-05-24). Reader of v2 should
> NOT need to consult v0 or the critique.
>
> **Single tag**: `agent-v0.3.0-rc.54`. ~460 LOC (~50 over v0 from inline
> critique fixes), ~22 new tests, ~2 ED realistic.
>
> **Phase order unchanged from v0**: A → B → D → C → C-SPA. Each phase
> ships independently testable; the rc.54 tag fires after Phases A + B + D
> (+ D' if needed) land. Phase C + C-SPA can roll to a `rc.54.1` minor tag
> if wizard SPA runs over.

---

## Changes from v0 (maps each delta to a critique item)

Critique items referenced by `[CR#n]`.

| Item | Phase | Change |
|---|---|---|
| `[CR#1 HIGH]` | A | **Pre-commit** `web-push = { version = "0.10", default-features = false, features = ["isahc-client"] }` in the same diff as the reqwest swap. Document `a2`/2-stack fallback if isahc client doesn't satisfy the push consumer at integration time. |
| `[CR#2 HIGH]` | B.5 | Resolve `StatusReport.attention: Option<String>` collision at `commands.rs:30` with **additive option (b)**: keep `attention: Option<String>` for back-compat, add NEW `attention_info: Option<AttentionInfo>` field. Comment documents why additive over replace (avoids coordinated tray SPA migration). |
| `[CR#3 HIGH]` | C | Add sparse index `(tenant_id, name, deleted_at)` to `crates/db/src/indexes.rs:192-201` in the same commit as Phase C. New compound index sits alongside the existing `(tenant_id, machine_id)` unique. |
| `[CR#4 MED]` | C-SPA | **Move probe call** from `probeAndRender()` bootstrap to the **Token-step submit handler**. ~10 LOC plumbs `state.probe.deleted_matches` into the Welcome step view (which renders the banner AFTER the operator transitions back, or banner moves to a new "Confirm" gate between Token and Install). |
| `[CR#5 MED]` | C-SPA | **Add `cmd_derive_machine_id() -> String` Tauri command** (or extend `cmd_default_device_name`). Wizard cannot supply machine_id to the probe without it. ~15 LOC. |
| `[CR#6 MED]` | B | `Path::with_extension("txt.json")` is empirically correct (`needs-attention.txt` → `needs-attention.txt.json`) because `with_extension` strips after the LAST `.` and appends the arg verbatim. Test MUST assert the absolute filename: `assert_eq!(json_path.file_name().unwrap(), "needs-attention.txt.json")`. If the assertion fails, swap the implementation to `text.with_file_name(format!("{}.json", text.file_name().unwrap().to_string_lossy()))`. |
| `[CR#7 MED]` | C-SPA | Replace ad-hoc `reqwest::Client::new()` with `reqwest::ClientBuilder` matching `asset_resolver.rs:77` and `:117` (user-agent `concat!("roomler-installer/", env!("CARGO_PKG_VERSION"))`, `timeout(Duration::from_secs(30))`). |
| `[CR#8 LOW]` | D | Concrete Path A/B threshold: "0 failures in **200 iterations** under `cargo test -p roomler-ai-remote-control --lib hub::tests::concurrent_three_registers -- --test-threads=8` repeated 200× (loop in a shell). If even **1** failure → Path B." |
| `[CR#9 LOW]` | B.4 | `FatalGoodbye` `unreachable!()` arm gets an inline comment cross-referencing the dispatch site at `signaling.rs:190` so a future refactor cannot silently route `ReplacedByNewerConnection` through `FatalGoodbye` and panic. |
| `[CR#10 LOW]` | A risk text | Correct the lettre claim: "lettre is already feature-tuned with **NO** TLS feature (`Cargo.toml:111`); SMTP transport ships without TLS today. Unaffected by the reqwest swap because lettre doesn't pull native-tls. If STARTTLS becomes required, that's a separate ticket." |
| `[CR#11 LOW]` | D Path B | Enumerate `unregister_agent` call-sites. `grep -rn "unregister_agent" crates/` returns: `routes/remote_control.rs::kick_agent` + `routes/ws.rs` (disconnect path). Both must capture `ConnectionId` at `register_agent` time if Path B fires. |
| `[CR#12 LOW]` | B tests | Add sequential write-race test: `raise → clear → raise` rapid sequence; assert no leftover JSON sidecar from the first raise bleeds into the second raise's reason field. New test `sidecar_does_not_bleed_across_clear`. |

Net LOC delta: ~+50 over v0's ~410 = **~460 LOC** total (commit-eligible).
TODO comments referenced in code snippets count toward this LOC budget; the
critique-only items (#8, #9, #11) live as TODO/doc comments and add <10 LOC.

---

## Problem this cycle solves

Three small drifts + one verification carried over from rc.53:

1. **Soft-delete-revive operator surprise.** When the wizard enrols a host
   whose `derive_machine_id` matches a *soft-deleted* row server-side, the
   existing enroll handler `rehydrate()`s the row silently
   (`crates/api/src/routes/remote_control.rs:91-108`). Same-`machine_id`
   continuity is preserved — but when machine_id has **changed**
   (cross-flavour reinstall, `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` flip,
   re-imaged host that shares the hostname), the operator gets a NEW row,
   silently. No banner, no "you used to have N deleted enrolments here, OK?".
2. **Three TLS stacks in one agent binary.** rc.53 swapped tungstenite to
   `rustls-tls-native-roots` (`Cargo.toml:176`); vendored `webrtc-ice` is on
   rustls + native roots (rc.32); `reqwest` is still on `native-tls`
   (Schannel) via the implicit `default-tls` feature (`Cargo.toml:103`).
   One cert-chain bug surface per stack × per OS — and a corp-MITM CA that
   only landed in the Windows trust store is now trusted by 2/3 paths but
   not the 3rd (file upload + GitHub releases proxy + enrollment HTTP).
3. **`needs-attention.txt` is freeform text.** rc.53 added 3 fatal-Goodbye
   call-sites all writing multi-line operator prose
   (`agents/roomler-agent/src/signaling.rs:159-188`, `:190-225`, plus
   `:136-151` from the pre-rc.53 auth-rejection case). The tray companion's
   `notify::has_attention()` call (`agents/roomler-agent-tray/src/commands.rs:51-52`)
   only knows "yes" / "no" — it can't render a coloured chip per reason.
   Field demand: structured reason on disk so the tray + future admin UI
   can differentiate `AgentDeleted` from `PolicyRejected` from
   `ReplacedByNewerEscalated`.
4. **rc.53 Phase 2b's `unregister_agent` race fix may be incomplete.**
   `hub.rs:192-211` reads the registered tx under DashMap and removes if
   `ptr_eq` matches. We need an investigation pass — is the read-then-remove
   atomic under DashMap entry locks? Is `ClientTx::same_channel` actually
   pointer-equal? Either pass = no-op rc.54, or pivot to a `ConnectionId`
   UUID stamp (~50 LOC).

## Goals

- Wizard surfaces deleted-row history per hostname before the operator
  commits to a new agent identity.
- One TLS stack in the agent binary (rustls + native roots everywhere).
- `notify::AttentionReason` enum + JSON sidecar — tray + future admin can
  parse, text sentinel kept verbatim for back-compat.
- Hub displacement race conclusively verified or fixed.

Non-goals (explicit defers to rc.55):

- Admin UI surface for `last_attention_reason` on the agent row (the agent
  doesn't ship the reason to the server today; wire-format addition + admin
  Vue chip pushed to rc.55).
- Wizard SPA revive UX for *non-soft-deleted* duplicates (rc.54 is
  soft-delete-only; the hostname-collision case for live rows is a different
  ticket).
- Migrating tunnel-client's `TunnelRevoked` to the same `AttentionReason`
  taxonomy.

## Phase order (rationale)

Same as v0; independent items, no inter-phase dep. Land in increasing
complexity so each phase can ship + be smoke-tested independently:

1. **Phase A** — TLS unify (Cargo feature swap + `web-push` pre-commit).
   Lowest LOC, highest fan-out risk; lands first so the rest of the cycle
   builds against the unified stack.
2. **Phase B** — `AttentionReason` enum + JSON sidecar + 4 call-site
   updates + tray-watcher prefer-JSON. Pure agent crate; ships independent
   of backend.
3. **Phase D** — Hub `unregister_agent` race investigation (read + targeted
   tests). If a real bug surfaces → fix as Phase D'. If not → "verification
   only" closure note.
4. **Phase C** — backend `POST /api/agent/enroll/probe` endpoint + sparse
   index (no SPA wiring yet; ships in agent commit so backend can deploy
   first).
5. **Phase C-SPA** — wizard SPA banner + `cmd_probe_enroll` +
   `cmd_derive_machine_id`. Ships independently — if SPA work runs over,
   agent crate cycle still tags.

The rc.54 tag fires after Phases A + B + D (+ D' if needed) land. Phase C
backend + Phase C-SPA can ship in a follow-up `agent-v0.3.0-rc.54.1` or
roll into rc.55 if blocked.

---

## Phase A — Unify TLS to rustls + native-roots (~0.2 d, low risk)

**Files**: `Cargo.toml`

```diff
-reqwest = { version = "0.12", features = ["json", "multipart", "cookies", "stream"] }
+reqwest = { version = "0.12", default-features = false, features = ["json", "multipart", "cookies", "stream", "rustls-tls-native-roots"] }
-web-push = "0.10"
+# [CR#1] Pre-commit web-push to isahc-client (rustls) so the Phase A
+# `cargo tree -e features -i native-tls` returns empty. If isahc-client
+# doesn't satisfy the push backend at integration time (Phase A
+# verification), fall back to the 2-stack reality and document in the
+# auto-fail section + backlog.
+web-push = { version = "0.10", default-features = false, features = ["isahc-client"] }
```

`default-features = false` on reqwest strips the implicit
`default-tls = native-tls` chain. `rustls-tls-native-roots` matches
tungstenite + vendored webrtc-ice behaviour: trust the OS store
(Schannel/CryptoKit/system-trust) without going through native-tls.

`web-push` pre-commit per `[CR#1]`: defaults pull `hyper-client → hyper-tls
→ native-tls`. `isahc-client` is the rustls path. If isahc breaks the push
consumer (e.g. async runtime mismatch with axum's tokio loop), document the
2-stack outcome in the Phase A auto-fail section rather than reverting the
reqwest swap — push has a single consumer (`crates/services/src/push/`)
and would be tractable to migrate to `a2` or a forked dep in rc.55.

### Verification (mars CI; this dev box can't link openssl-sys per `feedback_windows_no_local_backend`)

```bash
cargo check --workspace
cargo tree -e features -i native-tls       # MUST be empty after the swap
cargo tree -e features -i webpki-roots     # confirms no consumer pulled it back
cargo tree -e features -i rustls           # all 3 consumers point at the same root
cargo tree -e features -i openssl-sys      # MUST be empty (Linux risk)
cargo build -p roomler-agent --release --features full
cargo build -p roomler-installer --release
cargo build -p roomler-ai-api --release
cargo test -p roomler-ai-tests              # OAuth, GitHub-API, file upload, push
```

### Risks identified (grep verified)

- `grep -rn "native_tls\|TlsConnector" crates/ agents/` → ONLY hit is
  `crates/vendored/webrtc-ice/src/agent/tcp_turn_conn.rs:61, :158` —
  `tokio_rustls::TlsConnector` (NOT native-tls). Safe.
- No `schannel::*` direct imports anywhere in the workspace.
- `crates/api` reqwest consumers: OAuth (5 providers), GitHub releases
  proxy (`routes/agent_release.rs:260, :516`), tunnel release proxy,
  push (`web-push` handled by the pre-commit above).
- **[CR#10]** `lettre = "0.11", default-features = false, features =
  ["smtp-transport", "tokio1", "builder", "hostname"]` at `Cargo.toml:111`
  — lettre is already feature-tuned with **NO TLS feature**; SMTP transport
  ships without TLS today. Unaffected by the reqwest swap because lettre
  doesn't pull native-tls. If STARTTLS becomes required later that's a
  separate ticket; out of rc.54 scope.
- Linux build is the riskiest: native-tls on Linux uses OpenSSL via
  `openssl-sys`; some transitive may still need it. `cargo tree -e
  features -i openssl-sys` after the swap MUST be empty.

### Smoke matrix entries (Phase A)

- **SM-A1** — Plain-internet host: `cargo run -p roomler-agent --
  enroll` + `run`. Verify: reaches server, no TLS handshake regressions.
  Run `tcpdump -i any -w probe.pcap host roomler.ai`, confirm `Client
  Hello` cipher suite is the rustls one (`TLS_AES_128_GCM_SHA256` etc.)
  not Schannel's `TLS_ECDHE_*_WITH_AES_*` order.
- **SM-A2** — Corporate-MITM host (mkcert + stunnel per SM-3 from rc.53
  plan, but in front of `https://roomler.ai/api/agent/latest-release`).
  rc.53 binary: fails reqwest call with `UnknownIssuer` (Schannel path).
  rc.54 binary: succeeds. Confirms goal #2.
- **SM-A3 [CR#1]** — Push send round-trip with `web-push` on isahc-client:
  exercise via `cargo test -p roomler-ai-tests push::tests::send_round_trip`
  and verify no `native_tls`/`openssl` symbols appear in
  `nm target/release/roomler-ai-api | grep -E "native_tls|SSL_CTX"`.
  If push test fails: gate decision — accept 2-stack and revert the
  `web-push` feature change OR fork.

LOC budget: ~10 lines workspace Cargo.toml (reqwest + web-push) + lock
churn. **Tests**: 0 new unit; verification = `cargo tree` + e2e Playwright run + SM-A3.

---

## Phase B — `AttentionReason` enum + JSON sidecar (~0.5 d, low–medium risk)

**Files**:
- `agents/roomler-agent/src/notify.rs` (add enum + structured fn; keep
  legacy `raise_attention*` API)
- `agents/roomler-agent/src/signaling.rs:136-151, :159-188, :190-225`
  (4 call-sites)
- `agents/roomler-agent-tray/src/commands.rs:20-70` (additive
  `attention_info` field + JSON-prefer reader)

### B.1 — Enum definition

```rust
// agents/roomler-agent/src/notify.rs
/// rc.54: structured reason on the attention sentinel. JSON sidecar
/// written alongside the existing text file; tray + future admin UI
/// can render per-reason chips without scraping prose.
///
/// NOTE: when adding a variant, any consumer with a defensive
/// `_ =>` arm needs `#[allow(unreachable_patterns)]` per
/// CLAUDE.md "Defensive enum catch-alls" rule.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    /// Three consecutive 401s on agent token. Pre-rc.53 call-site.
    AuthRejected,
    /// `ServerMsg::Goodbye { reason: AgentDeleted }`. rc.53 call-site.
    AgentDeleted,
    /// `ServerMsg::Goodbye { reason: PolicyRejected }`. rc.53 call-site.
    PolicyRejected,
    /// 3+ displacements in 5 min rolling window. rc.53 call-site.
    ReplacedByNewerEscalated,
}
```

### B.2 — JSON sidecar schema (LOCKED — versioned for forward-compat)

```rust
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct AttentionSidecar {
    /// Schema version. Bump on breaking change; old readers fall
    /// back to text sentinel on unknown version.
    pub schema_version: u8,           // = 1 for rc.54
    pub reason: AttentionReason,
    pub message: String,
    pub generated_at_unix: u64,
    pub agent_version: String,        // env!("CARGO_PKG_VERSION")
}
```

Sidecar path: `<text_sentinel_path>.json` (e.g.
`%PROGRAMDATA%\roomler\roomler-agent\needs-attention.txt.json`). Two
files. **Write ordering INVARIANT** (text first, JSON second): if the agent
crashes between writes, the tray sees text without JSON → falls back to
text-only path (acceptable). If JSON were written first and text was
interrupted, `has_attention()` would return `false` (text is the sentinel)
and the tray would ignore the JSON — wrong. Test
`raise_attention_structured_writes_text_before_json` locks the ordering.

### B.3 — New API + back-compat

```rust
/// rc.54: structured variant. Writes BOTH the legacy text sentinel
/// (so pre-rc.54 tooling keeps working) AND the JSON sidecar.
/// Path resolution reuses `attention_path_for_worker` so the
/// LocalSystem `%PROGRAMDATA%` routing from rc.53 is preserved.
///
/// Ordering invariant: text is written FIRST (it's the sentinel that
/// `has_attention()` polls); JSON is the optional metadata sidecar.
pub fn raise_attention_structured(
    reason: AttentionReason,
    message: &str,
) -> Result<PathBuf> {
    let (text_path, machine_global) =
        attention_path_for_worker().context("no attention path")?;
    let parent = text_path.parent().context("attention path no parent")?;
    // Legacy text sentinel (unchanged content). Written FIRST per
    // the ordering invariant above.
    let text = raise_attention_at(parent, message)?;
    // JSON sidecar. [CR#6] `with_extension("txt.json")` produces
    // `needs-attention.txt.json` because Path::with_extension strips
    // after the LAST `.` and appends the param verbatim. Test
    // `sidecar_path_has_unambiguous_double_extension` ASSERTS the
    // exact filename. If that test ever flips (e.g. stdlib change),
    // swap to: `text.with_file_name(format!("{}.json",
    //   text.file_name().unwrap().to_string_lossy()))`.
    let json_path = text.with_extension("txt.json");
    let sidecar = AttentionSidecar {
        schema_version: 1,
        reason,
        message: message.to_string(),
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs()).unwrap_or(0),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let json = serde_json::to_vec_pretty(&sidecar)?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("writing sidecar {}", json_path.display()))?;
    tracing::warn!(
        text_path = %text.display(),
        json_path = %json_path.display(),
        ?reason,
        machine_global,
        "raised structured needs-attention sentinel"
    );
    Ok(text)
}

/// rc.54: best-effort cleanup that wipes BOTH files. [CR#12] order
/// matches the write order in reverse: JSON sidecar removed FIRST so
/// a concurrent `has_attention()` poll never sees text-without-sidecar
/// from a leftover prior cycle.
pub fn clear_attention_with_sidecar() {
    if let Some(p) = attention_path_for_worker().map(|(p, _)| p) {
        let _ = std::fs::remove_file(p.with_extension("txt.json"));
        let _ = std::fs::remove_file(&p);
    }
}
```

### B.4 — Call-site updates

| File:line | Old | New |
|---|---|---|
| `signaling.rs:144` | `raise_attention(msg)` | `raise_attention_structured(AttentionReason::AuthRejected, msg)` |
| `signaling.rs:172` | `raise_attention_machine_aware(&body)` | match-arm migration (see below) |
| `signaling.rs:214` | `raise_attention_machine_aware(&body)` | `raise_attention_structured(AttentionReason::ReplacedByNewerEscalated, &body)` |
| `signaling.rs:120` | `notify::clear_attention()` | `notify::clear_attention_with_sidecar()` |

**[CR#9]** `FatalGoodbye` match-arm at `signaling.rs:172`:

```rust
// [CR#9] AgentCloseReason exhaustive match. ReplacedByNewerConnection
// CANNOT reach this arm because it flows through
// ConnectError::ReplacedByNewer { message } dispatched at
// signaling.rs:190 (the OTHER call-site below). If a future refactor
// routes ReplacedByNewerConnection through FatalGoodbye, this
// unreachable!() will PANIC at runtime — convert to
// AttentionReason::ReplacedByNewerEscalated instead of panicking.
raise_attention_structured(match reason {
    AgentCloseReason::AgentDeleted => AttentionReason::AgentDeleted,
    AgentCloseReason::PolicyRejected => AttentionReason::PolicyRejected,
    AgentCloseReason::ReplacedByNewerConnection => unreachable!(
        "ReplacedByNewerConnection routes through ConnectError::ReplacedByNewer \
         at signaling.rs:190, not through FatalGoodbye"
    ),
}, &body)
```

### B.5 — Tray watcher: additive `attention_info` field [CR#2]

**Decision**: option (b) **additive**. Keeping `attention: Option<String>`
for back-compat avoids a coordinated migration of the tray HTML/JS
(`agents/roomler-agent-tray/dist/index.html` currently reads
`status.attention`). The SPA renders both fields; the legacy field stays
populated with the path string so pre-rc.54 SPA bundles keep working
across the tray companion upgrade boundary.

`agents/roomler-agent-tray/src/commands.rs:20-33`:

```rust
#[derive(serde::Serialize, ts_rs::TS, Debug, Clone)]
pub struct StatusReport {
    pub enrolled: bool,
    pub agent_id: Option<String>,
    pub tenant_id: Option<String>,
    pub server_url: Option<String>,
    pub device_name: Option<String>,
    pub agent_version: String,
    pub config_schema_version: Option<String>,
    pub service_running: bool,
    pub service_kind: String, // "scheduledTask" | "scmService" | "none"
    /// Legacy path-only field. Pre-rc.54 SPA reads this. KEEP.
    pub attention: Option<String>,
    /// rc.54: structured sidecar parse. NEW. Optional so SPA pre-rc.54
    /// ignores via serde's "unknown field" tolerance. SPA post-rc.54
    /// prefers this when present, falls back to `attention`.
    pub attention_info: Option<AttentionInfo>,
    pub log_dir: String,
    pub config_dir: String,
}

#[derive(serde::Serialize, ts_rs::TS, Debug, Clone)]
pub struct AttentionInfo {
    pub path: Option<String>,
    pub reason: Option<String>,        // "AuthRejected" | ...
    pub message: Option<String>,
}
```

`commands.rs:51-55` reader update:

```rust
let attention = if notify::has_attention() {
    notify::attention_path().map(|p| p.to_string_lossy().into_owned())
} else {
    None
};
// [CR#2] rc.54: structured sidecar reader. Always populated alongside
// the legacy `attention` field. Falls back gracefully on schema
// mismatch, absent sidecar (pre-rc.54 agent binary), or parse error.
let attention_info = if notify::has_attention() {
    let text_path = notify::attention_path();
    let sidecar = text_path.as_ref()
        .map(|p| p.with_extension("txt.json"))
        .and_then(|p| std::fs::read(&p).ok())
        .and_then(|b| serde_json::from_slice::<notify::AttentionSidecar>(&b).ok())
        .filter(|s| s.schema_version == 1);
    Some(AttentionInfo {
        path: text_path.map(|p| p.to_string_lossy().into_owned()),
        reason: sidecar.as_ref().map(|s| format!("{:?}", s.reason)),
        message: sidecar.map(|s| s.message),
    })
} else { None };
// ... StatusReport { ..., attention, attention_info, ... }
```

### Tests (in `agents/roomler-agent/src/notify.rs::tests`)

```rust
#[test] fn attention_sidecar_round_trips_for_every_reason() { /* 4 variants */ }
#[test] fn raise_attention_structured_writes_both_files() { /* tempdir */ }
#[test] fn sidecar_path_has_unambiguous_double_extension() {
    // [CR#6] LOCK the filename — `Path::with_extension("txt.json")`
    // applied to `needs-attention.txt` MUST produce
    // `needs-attention.txt.json`, NOT `needs-attention.json`. If this
    // ever fails (stdlib semantics change), switch to with_file_name.
    let p = std::path::PathBuf::from("/tmp/needs-attention.txt");
    let j = p.with_extension("txt.json");
    assert_eq!(j.file_name().unwrap(), "needs-attention.txt.json");
}
#[test] fn raise_attention_structured_writes_text_before_json() {
    /* tempdir; observe partial-state by mocking the second write to fail;
       confirm text exists and JSON does not — the documented invariant */
}
#[test] fn sidecar_schema_v1_is_locked() { /* golden JSON byte assert */ }
#[test] fn clear_attention_with_sidecar_removes_both() { /* tempdir */ }
#[test] fn sidecar_does_not_bleed_across_clear() {
    // [CR#12] sequential race: raise(AuthRejected) → clear →
    // raise(AgentDeleted). Assert the AgentDeleted sidecar reads back
    // as AgentDeleted (not stale AuthRejected from a leftover sidecar).
    let dir = tempdir().unwrap();
    raise_attention_structured_at(&dir, AttentionReason::AuthRejected, "first")?;
    clear_attention_with_sidecar_at(&dir);
    raise_attention_structured_at(&dir, AttentionReason::AgentDeleted, "second")?;
    let bytes = std::fs::read(dir.path().join("needs-attention.txt.json"))?;
    let s: AttentionSidecar = serde_json::from_slice(&bytes)?;
    assert_eq!(s.reason, AttentionReason::AgentDeleted);
    assert_eq!(s.message, "second");
}
```

### Tray tests (`agents/roomler-agent-tray/src/commands.rs::tests`)

```rust
#[test] fn status_report_includes_reason_when_sidecar_present() { /* tempdir */ }
#[test] fn status_report_falls_back_to_text_when_sidecar_absent() { /* tempdir */ }
#[test] fn status_report_falls_back_when_sidecar_schema_mismatch() { /* schema=2 */ }
#[test] fn status_report_keeps_legacy_attention_field_populated() {
    // [CR#2] assert BOTH `attention` (path string) AND
    // `attention_info` (structured) are populated, not one-or-the-other.
}
```

### Per `feedback_defensive_enum_catch_alls`

No exhaustive matches on `AttentionReason` exist outside this PR; future
rc.55 admin UI consumers will need `#[allow(unreachable_patterns)]` on
catch-alls until any new variants land. Doc-comment on the enum
(see B.1) captures the rule.

LOC budget: ~250 (enum ~15, structured fn + invariant comments ~60, call-
site swaps including [CR#9] comment ~45, tray watcher additive field ~40,
tests ~90).

---

## Phase D — Hub `unregister_agent` race investigation (~0.2 d, no LOC if no bug)

**Goal**: read `crates/remote_control/src/hub.rs:192-211` + the
DashMap source to settle whether the rc.53 fix is sufficient.

### Investigation steps

1. **Atomicity of read-then-remove**: DashMap's `get(&key)` returns a
   `Ref` (read-guard). The current code (`hub.rs:198-210`) reads
   `still_ours` *under* the guard but `remove`s OUTSIDE the guard.
   Between the drop of the read guard and the `remove`, a third
   connection could land and `insert` a new entry. The remove would
   then evict THAT entry.
   - **Test to add** (`hub::tests::concurrent_three_registers`): drive 3
     concurrent registers + 1 stale unregister, assert third entry
     survives.
   - **[CR#8]** **Path A/B decision threshold (CONCRETE)**: run

     ```bash
     for i in $(seq 1 200); do
       cargo test -p roomler-ai-remote-control --lib \
         hub::tests::concurrent_three_registers -- --test-threads=8 \
         || { echo "FAILED at iter $i"; exit 1; }
     done
     ```

     **0 failures across 200 iterations = Path A**. **1 or more failures
     = Path B**. No subjective "looks flaky" gate.
2. **`ClientTx::same_channel` semantics**: it's `tokio::sync::mpsc::
   Sender::same_channel(&self, other) -> bool` which IS pointer-equal
   on the underlying `Arc<Inner>`. Confirmed by reading
   `tokio-1.x/src/sync/mpsc/bounded.rs::same_channel`. So `ptr_eq`
   is safe — a "coincidentally same channel" scenario is impossible.
   **No action**.
3. **Admin kick (`routes/remote_control.rs:259`)**: calls
   `unregister_agent(aid, None)` → "always remove" branch. Race with
   a fresh connect = admin wins (correct per ACL semantics). Add a
   test that admin-kick + concurrent re-register evicts the
   re-register cleanly (admin intent takes precedence).
   - **Test**: `hub::tests::admin_kick_evicts_even_during_reconnect_race`.

### Outcomes

- **Path A (no-op rc.54)**: investigation confirms (1) is benign, (2)
  is safe, (3) is intentional. Plan closes with 2 new tests + a doc-
  comment update on `unregister_agent` explaining the
  read-then-remove window is acceptable. **~30 LOC.**
- **Path B (fix needed)**: replace `ClientTx::same_channel` with a
  `ConnectionId(uuid::Uuid)` stamped on `ConnectedAgent` at
  `register_agent` time + returned to the WS handler; unregister
  takes `ConnectionId` not `&ClientTx`; identity check is
  `id == prev.connection_id`. **~50 LOC.**

  **[CR#11]** Path B call-site retrofit. `grep -rn "unregister_agent"
  crates/` returns:

  - `crates/api/src/routes/remote_control.rs::kick_agent` — admin path,
    currently passes `None`. Path B: passes `None` still (admin kick is
    "always remove" regardless of ConnectionId — the `None` semantics
    are preserved). Comment locks this.
  - `crates/api/src/routes/ws.rs` — WS disconnect path. Currently passes
    `Some(&tx)`. Path B: must capture the `ConnectionId` returned by
    `register_agent` at WS-accept time, store it in the per-connection
    state, and pass `Some(connection_id)` on disconnect.

  Both call-sites land in the same Path B commit; the migration is
  straightforward because there are only two.

Decision deferred to investigation finding. Plan defaults to Path A;
flips to B only if the concurrent-3-registers test fails per the
threshold in step 1.

LOC budget: ~30 (Path A) or ~80 (Path B).
**Tests**: 2 new (concurrent-3-registers, admin-kick-race).

---

## Phase C — Backend `POST /api/agent/enroll/probe` (~0.4 d, low–med risk)

**Files**:
- `crates/api/src/routes/agent_enroll.rs` (NEW — prefer new file over
  bloating `agent_release.rs` which is already 530 lines, mixed concerns)
- `crates/api/src/routes/mod.rs` (export new module)
- `crates/api/src/lib.rs:264-285` (mount route)
- `crates/services/src/dao/agent.rs` (new DAO method, sibling to
  `find_by_tenant_and_machine` at `:58`)
- **[CR#3]** `crates/db/src/indexes.rs:192-201` (sparse index in the
  same commit)

### C.0 [CR#3] — Sparse index added in the same commit as the route

```rust
// crates/db/src/indexes.rs — extend the agents block at lines 192-201
create_indexes(
    db,
    "agents",
    vec![
        index_unique(bson::doc! { "tenant_id": 1, "machine_id": 1 }),
        index(bson::doc! { "tenant_id": 1, "status": 1 }),
        index(bson::doc! { "owner_user_id": 1 }),
        // [CR#3] rc.54: sparse compound index for the
        // POST /api/agent/enroll/probe endpoint. The probe filter is
        // `{ tenant_id, $or: [{ name }, { machine_id }], deleted_at: { $ne: null } }`
        // — without this index, the `name` arm tablescans the
        // tenant. Sparse on deleted_at keeps the index small (only
        // soft-deleted rows are indexed).
        IndexModel::builder()
            .keys(bson::doc! { "tenant_id": 1, "name": 1, "deleted_at": 1 })
            .options(IndexOptions::builder().sparse(true).build())
            .build(),
    ],
)
.await?;
```

If `indexes.rs` uses helper fns that don't expose `.sparse()`, add an
`index_sparse` helper alongside the existing `index`/`index_unique`/
`index_ttl`/`index_text` helpers.

### C.1 — Route definition

```rust
// crates/api/src/routes/agent_enroll.rs (NEW)
//! `POST /api/agent/enroll/probe` — pre-enrollment soft-delete probe.
//!
//! Called by the wizard BEFORE `cmd_install` runs `roomler_agent::
//! enrollment::enroll`. Returns the LIST of soft-deleted rows in the
//! probe's tenant that match either `hostname` OR `machine_id`. The
//! wizard renders a banner if non-empty so the operator knows they're
//! about to lose identity continuity with old sessions/audit.
//!
//! Auth: enrollment JWT (same audience as POST /api/agent/enroll).
//! The token's `tenant_id` claim scopes the probe; the wizard
//! doesn't need a user JWT.

use axum::{Json, extract::State};
use bson::oid::ObjectId;
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, state::AppState};

#[derive(Deserialize)]
pub struct ProbeRequest {
    pub enrollment_token: String,
    pub hostname: String,
    pub machine_id: String,
}

#[derive(Serialize)]
pub struct ProbeMatch {
    pub agent_id: String,
    pub hostname: String,         // agent.name — wizard renders this
    pub machine_id: String,
    pub deleted_at_unix: i64,
    pub last_seen_at_unix: i64,
    /// True when `machine_id` matched (continuity preserved on
    /// re-enroll; rehydrate will fire). False when only hostname
    /// matched (machine_id changed — fresh identity).
    pub machine_id_match: bool,
}

#[derive(Serialize)]
pub struct ProbeResponse {
    /// Sorted most-recent first.
    pub deleted_matches: Vec<ProbeMatch>,
}

pub async fn probe_enroll(
    State(state): State<AppState>,
    Json(body): Json<ProbeRequest>,
) -> Result<Json<ProbeResponse>, ApiError> {
    let claims = state.auth.verify_enrollment_token(&body.enrollment_token)?;
    let tid = ObjectId::parse_str(&claims.tenant_id)
        .map_err(|_| ApiError::BadRequest("invalid tenant_id claim".into()))?;
    let matches = state.agents
        .find_soft_deleted_by_hostname_or_machine(tid, &body.hostname, &body.machine_id)
        .await?;
    Ok(Json(ProbeResponse {
        deleted_matches: matches.into_iter().map(|a| {
            let aid = a.id.expect("agent has _id");
            ProbeMatch {
                agent_id: aid.to_hex(),
                hostname: a.name,
                machine_id: a.machine_id.clone(),
                deleted_at_unix: a.deleted_at.map(|d| d.timestamp_millis() / 1000).unwrap_or(0),
                last_seen_at_unix: a.last_seen_at.timestamp_millis() / 1000,
                machine_id_match: a.machine_id == body.machine_id,
            }
        }).collect(),
    }))
}
```

### C.2 — DAO method

```rust
// crates/services/src/dao/agent.rs (sibling to find_by_tenant_and_machine at :58)
/// rc.54: return ALL soft-deleted rows in this tenant whose hostname
/// (= `agent.name`) OR `machine_id` matches. Sorted most-recent-
/// deleted first. Empty when no matches. Used by
/// `POST /api/agent/enroll/probe`. Backed by the sparse compound index
/// `(tenant_id, name, deleted_at)` added in the same commit
/// (`crates/db/src/indexes.rs:192-201`).
pub async fn find_soft_deleted_by_hostname_or_machine(
    &self,
    tenant_id: ObjectId,
    hostname: &str,
    machine_id: &str,
) -> DaoResult<Vec<Agent>> {
    use mongodb::options::FindOptions;
    let filter = doc! {
        "tenant_id": tenant_id,
        "deleted_at": { "$ne": null },
        "$or": [
            { "name": hostname },
            { "machine_id": machine_id },
        ],
    };
    let opts = FindOptions::builder()
        .sort(doc! { "deleted_at": -1 })
        .limit(20)  // sanity cap; UI shows first 5 + "show N more"
        .build();
    self.base.find_many(filter, opts).await
}
```

### C.3 — Route mount

`crates/api/src/lib.rs:265` — extend `public_agent_routes`:

```rust
let public_agent_routes = Router::new()
    .route("/enroll", post(routes::remote_control::enroll_agent))
    .route("/enroll/probe", post(routes::agent_enroll::probe_enroll))   // NEW
    .route("/latest-release", get(routes::agent_release::latest_release))
    ...
```

### Tests (in `crates/tests/`)

```rust
#[tokio::test] async fn probe_returns_empty_when_no_matches() { /* TestApp */ }
#[tokio::test] async fn probe_returns_soft_deleted_matching_hostname_only() { /* */ }
#[tokio::test] async fn probe_returns_soft_deleted_matching_machine_id() { /* machine_id_match=true */ }
#[tokio::test] async fn probe_excludes_non_deleted_rows() { /* never returns live rows */ }
#[tokio::test] async fn probe_excludes_other_tenants() {
    // C-fail leak guard: claims.tenant_id must scope, even when
    // hostname+machine_id collide cross-tenant.
}
#[tokio::test] async fn probe_rejects_user_jwt() { /* audience-check */ }
#[tokio::test] async fn probe_rejects_agent_jwt() { /* audience-check */ }
#[tokio::test] async fn probe_sorted_most_recent_first() { /* 3 rows, asc deleted_at */ }
#[tokio::test] async fn probe_caps_at_20_rows() { /* 25 rows, len == 20 */ }
```

LOC budget: ~120 backend + 40 tests + 10 mount + 10 sparse-index =
**~180** (slightly over v0 due to the index).

---

## Phase C-SPA — Wizard banner + probe + machine_id cmd (~0.3 d, low risk)

**Files**:
- `agents/roomler-installer/src/commands.rs` (Tauri `cmd_probe_enroll` +
  `cmd_derive_machine_id`)
- `agents/roomler-installer/src/main.rs:93+` (register both handlers)
- `agents/roomler-installer/src/front/app.js` (probe AT TOKEN-SUBMIT, NOT
  bootstrap; render banner on the gate AFTER Token step)
- `agents/roomler-installer/src/front/index.html` (banner DOM + optional
  "Confirm" gate step between Token and Install)

### C-SPA.1 — `cmd_derive_machine_id` [CR#5]

```rust
// agents/roomler-installer/src/commands.rs
// [CR#5] rc.54: expose roomler_agent::config::derive_machine_id() to
// the SPA so the wizard can pass machine_id to cmd_probe_enroll
// BEFORE the install runs. Deterministic over (hostname + os + arch +
// config_path); see CLAUDE.md "M3 A1 profile-path lesson". Pure read,
// no side effects.
#[tauri::command]
pub fn cmd_derive_machine_id() -> Result<String, String> {
    roomler_agent::config::derive_machine_id()
        .map_err(|e| format!("derive_machine_id: {e}"))
}
```

Registered alongside the existing handlers in `main.rs`.

### C-SPA.2 — `cmd_probe_enroll` with ClientBuilder [CR#7]

```rust
// agents/roomler-installer/src/commands.rs
use std::time::Duration;

#[derive(serde::Serialize)]
pub struct ProbeMatch {
    pub agent_id: String,
    pub hostname: String,
    pub deleted_at_unix: i64,
    pub machine_id_match: bool,
}

#[tauri::command]
pub async fn cmd_probe_enroll(
    server: String,
    token: String,
    hostname: String,
    machine_id: String,
) -> Result<Vec<ProbeMatch>, String> {
    // [CR#7] Mirror the asset_resolver.rs:77+:117 pattern: explicit
    // user-agent + 30s timeout. Bare `Client::new()` would use
    // defaults with no UA and a 30s default that's not future-proof.
    let url = format!(
        "{}/api/agent/enroll/probe",
        server.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("roomler-installer/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("probe client build: {e}"))?;
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "enrollment_token": token,
            "hostname": hostname,
            "machine_id": machine_id,
        }))
        .send()
        .await
        .map_err(|e| format!("probe: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("probe HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("probe parse: {e}"))?;
    Ok(serde_json::from_value(body["deleted_matches"].clone())
        .unwrap_or_default())
}
```

### C-SPA.3 — Probe fires at TOKEN-SUBMIT, not bootstrap [CR#4]

**Wrong (v0 plan)**: call probe in `probeAndRender()` at `app.js:90` on
bootstrap. The token isn't pasted yet — probe has no JWT to send.

**Right**: probe fires in the Token-step submit handler. Result feeds a
new `state.probe.deleted_matches` slot consumed by the next gate step.

Two implementation options (pick one in impl):

- **Option 3a (preferred)**: Insert a new "Confirm" gate step between
  Token and Install. Token-step submit awaits probe, stores
  `state.probe.deleted_matches`, then advances to Confirm. Confirm
  renders the banner if `deleted_matches.length > 0`, with Continue +
  Cancel. If `deleted_matches` is empty, the Confirm step auto-skips
  forward to Install (no friction in the happy path).
- **Option 3b (smaller diff)**: Token-step submit awaits probe; if
  `deleted_matches.length > 0`, render an inline banner on the Token
  step itself with an "I understand" ack checkbox gating the
  step-forward button. Continue advances to Install.

Either way, **`probeAndRender()` at `app.js:90` is NOT modified**. The
probe lives in the Token-step submit path. State plumbing (~10 LOC):

```js
// agents/roomler-installer/src/front/app.js — Token step submit
async function onTokenSubmit() {
    state.token = ui.tokenInput.value.trim();
    if (!state.token) return showError("Token required");
    // [CR#4] rc.54: probe BEFORE advancing. machine_id sourced from
    // [CR#5] cmd_derive_machine_id, server from state.server, hostname
    // from state.deviceName (or derived).
    try {
        state.machineId = await invoke("cmd_derive_machine_id");
        state.probe = {
            deleted_matches: await invoke("cmd_probe_enroll", {
                server: state.server,
                token: state.token,
                hostname: state.deviceName,
                machineId: state.machineId,
            }),
        };
    } catch (e) {
        // Probe failure is non-fatal — backend may not yet have rc.54.
        // Log + advance without the banner.
        console.warn("probe failed (non-fatal):", e);
        state.probe = { deleted_matches: [] };
    }
    advanceToStep("confirm");  // Option 3a
}
```

### C-SPA.4 — Banner UX

When `state.probe.deleted_matches.length > 0`, the Confirm step renders:

```
┌─────────────────────────────────────────────────┐
│ ⚠ A previous enrolment for this host was       │
│   deleted on 2026-04-12.                       │
│                                                 │
│ Continuing creates a new agent identity; old   │
│ sessions and audit logs stay associated with   │
│ the deleted row.                                │
│                                                 │
│ [Show 2 more deleted enrolments ▾]             │
│                                                 │
│ [ Continue ]   [ Cancel ]                       │
└─────────────────────────────────────────────────┘
```

LOC budget: ~80 SPA + ~20 Tauri probe cmd + ~15 Tauri machine_id cmd =
**~115** (slightly over v0 due to derive-machine-id cmd + token-submit
plumbing).

**Tests** (`agents/roomler-installer/tests/`): 1 mock-backend integration
covering banner-renders-after-token-submit + Continue-progresses. New
unit test for `cmd_derive_machine_id` covers the
already-tested-elsewhere `derive_machine_id` re-export (sanity only,
~3 LOC).

---

## Phase totals + risk + tests (recalculated)

| Phase | LOC | Risk | New tests |
|---|---|---|---|
| A — TLS unify + web-push pre-commit | ~12 | L | 0 (verification via `cargo tree` + SM-A3) |
| B — AttentionReason + JSON sidecar + 4 call-sites + tray additive field | ~250 | L–M | 11 (notify=7, tray=4) |
| C — `/enroll/probe` backend + DAO + sparse index | ~180 | L | 9 |
| C-SPA — wizard banner + 2 probe cmds | ~115 | L | 1 + 1 sanity |
| D — Hub race investigation (Path A default) | ~30 | L | 2 |
| D' — only if investigation flags bug | ~50 | M | (+2) |
| **Total (A+B+C+D, no C-SPA, no D')** | **~472** | **L–M** | **~22** |
| **Total (everything)** | **~587** | **L–M** | **~24** |

Net delta vs v0: **+62 LOC** mostly from web-push diff (~2), additive
`attention_info` struct + plumbing (~10), sparse-index helper (~10),
`cmd_derive_machine_id` (~15), probe-on-token-submit state plumbing
(~10), critique inline comments (~10), additional locked tests (~5).

Engineer-day budget: **~1.5 d realistic, ~2 d defensive**. Phases A + D
ship in <½ day each. Phase B + C are afternoon-each. C-SPA can roll
over to next session without blocking the tag.

---

## Smoke matrix (manual, after CI green on mars)

- **SM-A1** — Plain-internet host: `cargo run -p roomler-agent --
  enroll` + `run`. Verify: reaches server, no TLS handshake regressions.
  `tcpdump` confirms `Client Hello` cipher suite is the rustls one
  (`TLS_AES_128_GCM_SHA256` etc.) not Schannel's `TLS_ECDHE_*_WITH_AES_*`
  order.
- **SM-A2** — Corporate-MITM host (mkcert + stunnel in front of
  `https://roomler.ai/api/agent/latest-release`). rc.53 binary: fails
  reqwest call with `UnknownIssuer` (Schannel path). rc.54 binary:
  succeeds.
- **SM-A3 [CR#1]** — Push send round-trip with `web-push` on
  isahc-client: `cargo test -p roomler-ai-tests push::tests::send_round_trip`
  + `nm target/release/roomler-ai-api | grep -E "native_tls|SSL_CTX"`
  returns empty. If the push test fails, decision-gate the 2-stack
  fallback per auto-fail A-fail-3 below.
- **SM-B1** — Trigger `AuthRejected` (revoke token in admin UI). Wait 3
  failures (15 min on default backoff). Assert:
  - `%APPDATA%\roomler\roomler-agent\config\needs-attention.txt`
    exists with the prose msg
  - `<same>.json` exists, parses, `reason == "auth_rejected"`,
    `agent_version == "0.3.0-rc.54"`, `schema_version == 1`
  - Tray status `StatusReport.attention_info.reason == "AuthRejected"`
    AND legacy `StatusReport.attention` field is the path string (both
    populated per `[CR#2]`).
- **SM-B2** — Trigger `AgentDeleted` via admin delete. Assert sidecar
  `reason == "agent_deleted"` + tray surfaces it.
- **SM-B3** — Pre-rc.54 tray binary reading rc.54 sidecar: graceful
  ignore + falls back to text (back-compat across the tray companion
  upgrade boundary).
- **SM-B4 [CR#12]** — Sequential write race: trigger AuthRejected,
  manually `clear_attention_with_sidecar()`, trigger AgentDeleted within
  100ms. Tray reads `reason == "AgentDeleted"`, not stale AuthRejected.
  Mirrors the unit test `sidecar_does_not_bleed_across_clear` at the
  integration boundary.
- **SM-C1** — Wizard probe round-trip. (a) Enrol host `pc-001`, delete
  it. (b) Launch wizard from same host. (c) Token step accepts the
  fresh enrollment JWT. (d) After Token submit, Confirm step shows
  banner with `pc-001`'s deleted_at. (e) Continue → enrolment proceeds,
  rehydrates the row (because machine_id matched).
- **SM-C2** — Wizard probe with machine_id MISMATCH (e.g. operator
  flips `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` so config_path changes).
  Banner shows `pc-001` with `machine_id_match: false`. Continue
  creates a NEW row, old stays soft-deleted.
- **SM-C3 [CR#4]** — Verify probe fires on token-submit, NOT bootstrap.
  Launch wizard offline → Welcome + Server + Token steps render fine
  (no probe call attempted). Paste token + click submit → probe fires.
  If offline at this point, probe error is non-fatal (per
  `console.warn(...)` in the snippet) and wizard advances to Confirm
  with an empty banner.
- **SM-D1** — 3 concurrent agent registers from 3 fake socket handles,
  1 stale unregister of the first tx. Verify the latest register
  survives. (Only if Phase D Path B selected; Path A test asserts the
  bug doesn't manifest under the 200-iteration threshold.)

---

## Auto-fail conditions (force v2-revision or revert)

- **A-fail**: `cargo tree -e features -i native-tls` non-empty after the
  swap → some transitive (likely `lettre` if STARTTLS gets added, or a
  new dep) still pulls native-tls. Either feature-tune that dep or
  document the 2-stack reality and ship rc.54 as a partial unify (still
  removes the reqwest stack — improvement over rc.53).
- **A-fail-2**: Linux production hits an `openssl-sys` link error in
  CI. Revert the reqwest swap; ship rc.54 without Phase A.
- **A-fail-3 [CR#1]**: `web-push` on `isahc-client` fails the push
  round-trip in SM-A3 (e.g. async runtime mismatch with axum's
  tokio). Decision-gate: (a) revert the `web-push` feature change
  ONLY (keep reqwest swap), accept the 2-stack reality, document in
  backlog as "rc.55 evaluate `a2` or fork web-push". (b) Or block rc.54
  Phase A entirely and ship Phases B+C+D. Default: option (a).
- **B-fail**: Sidecar JSON encoding sees a serde error (e.g. an
  AttentionReason variant added in a follow-up commit isn't
  serialisable). Wire-format-lock test in `notify::tests` catches
  this in CI.
- **B-fail-2**: Tray binary built against rc.53 panics on rc.54
  sidecar shape. Schema versioning + `.filter(|s| s.schema_version
  == 1)` defends; if the panic surfaces anyway, the fallback path
  catches the parse error gracefully.
- **B-fail-3 [CR#6]**: `sidecar_path_has_unambiguous_double_extension`
  test fails (Rust stdlib change). Swap implementation to
  `text.with_file_name(format!("{}.json", text.file_name().unwrap().to_string_lossy()))`
  per the inline TODO in B.3.
- **C-fail**: `/enroll/probe` leaks deleted-but-cross-tenant rows.
  Test `probe_excludes_other_tenants` MUST be in the test file.
- **C-fail-2**: Probe response over-shares (e.g. tenant ID, owner
  user ID). Response struct is explicit field-list (no `..a`); leak
  test asserts the JSON shape contains ONLY the documented fields.
- **C-fail-3 [CR#3]**: Mongo `explain()` on the probe filter shows a
  `COLLSCAN` instead of `IXSCAN` over the new sparse index. Means the
  `index_sparse` helper / `IndexModel.options.sparse(true)` call wasn't
  applied. Re-apply + restart mongod (index build is async).
- **D-fail**: Investigation surfaces a non-benign race AND Path B
  fix breaks an existing hub test. Investigation pivots to a
  targeted lock-around-the-pair fix (~10 LOC, `parking_lot::Mutex`
  around the agents map).

---

## Commit / tag split

**Default**: single tag `agent-v0.3.0-rc.54` covering Phases A + B + D.
Phases C + C-SPA ship in a follow-up minor-tag `agent-v0.3.0-rc.54.1`
ONLY IF the wizard SPA work runs over the cycle window — per the
constraint "rc.54 cycle should NOT block on wizard-side UX shipping."

Commit split (single tag):

1. `chore(agent): unify TLS to rustls + native-roots (reqwest + web-push)`
   (Phase A, ~12 LOC) — both Cargo.toml lines in one diff per `[CR#1]`.
2. `feat(agent): AttentionReason enum + JSON sidecar` (Phase B.1-B.3,
   ~90 LOC including invariant comments).
3. `refactor(agent): migrate 4 attention call-sites to structured API`
   (Phase B.4, ~45 LOC including `[CR#9]` cross-ref comment).
4. `feat(tray): additive attention_info field + per-reason chip` (Phase
   B.5, ~40 LOC; back-compat via additive field per `[CR#2]`).
5. `test(hub): displacement-vs-unregister concurrent + admin kick race`
   (Phase D Path A, ~30 LOC) — or commit 5+6 if Path B fires.
6. (optional) `feat(api,db,wizard): /enroll/probe + sparse index + Welcome banner`
   (Phase C + C-SPA, ~295 LOC including `[CR#3]` index + `[CR#5]`
   derive_machine_id cmd).
7. `chore: bump workspace 0.3.0-rc.53 → 0.3.0-rc.54`
8. Tag `agent-v0.3.0-rc.54` + push.

If Phase C-SPA misses the window: drop commit 6 → second tag
`agent-v0.3.0-rc.54.1` once SPA lands.

Pre-tag gate (memory rule `feedback_cargo_test_before_agent_commit`):

```bash
cargo test -p roomler-agent --lib
cargo clippy -p roomler-agent --lib -- -D warnings   # NOT --all-targets
cargo check -p roomler-agent --features system-context  # rc.53 CI miss guard
cargo fmt --all -- --check
# Backend gates (run on mars per feedback_windows_no_local_backend):
cargo test -p roomler-ai-tests
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

---

## Files most critical for implementation

- `C:\dev\gjovanov\roomler-ai\Cargo.toml:103` — Phase A reqwest swap.
- `C:\dev\gjovanov\roomler-ai\Cargo.toml:152` — Phase A `[CR#1]` web-push
  pre-commit.
- `C:\dev\gjovanov\roomler-ai\Cargo.toml:111` — Phase A `[CR#10]` lettre
  reference (NO change; documented in risk section).
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\notify.rs:1-217` —
  Phase B add enum, structured fn, schema-locked sidecar.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\signaling.rs:117-225`
  — Phase B 4 call-site swaps (auth-rejected at `:136-151`, fatal-Goodbye
  at `:159-188`, replaced-newer escalation at `:190-225`, clear at
  `:117-122`).
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent-tray\src\commands.rs:20-70`
  — Phase B `[CR#2]` additive `attention_info` field + JSON-prefer
  reader.
- `C:\dev\gjovanov\roomler-ai\crates\db\src\indexes.rs:192-201` — Phase
  C `[CR#3]` sparse compound index `(tenant_id, name, deleted_at)`.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\routes\agent_enroll.rs`
  (NEW) — Phase C probe endpoint.
- `C:\dev\gjovanov\roomler-ai\crates\services\src\dao\agent.rs:54-69` —
  Phase C DAO sibling method.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\lib.rs:264-285` — Phase C
  route mount.
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\hub.rs:192-228`
  — Phase D investigation target (and Path B edit site if needed).
- `C:\dev\gjovanov\roomler-ai\crates\api\src\routes\remote_control.rs`
  (kick_agent) + `crates\api\src\routes\ws.rs` (disconnect) — Phase D
  `[CR#11]` Path B `unregister_agent` call-site retrofit.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\commands.rs`
  — Phase C-SPA `cmd_probe_enroll` (`[CR#7]` ClientBuilder) +
  `cmd_derive_machine_id` (`[CR#5]`).
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\asset_resolver.rs:77, :117`
  — Phase C-SPA `[CR#7]` ClientBuilder reference pattern.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\front\app.js`
  — Phase C-SPA `[CR#4]` probe at token-submit (NOT in `probeAndRender`
  bootstrap).

---

## Backlog (post-rc.54, candidate rc.55 items)

- **Admin UI per-reason chip**: agent → server protocol addition to ship
  `last_attention_reason` on heartbeats / hello so the admin agents list
  can render a coloured chip per agent. Wire-format addition on
  `ClientMsg::AgentHello` + `ClientMsg::AgentHeartbeat`, schema
  migration on `agents` collection, Vue3 `AgentsSection` column.
  ~150 LOC.
- **Wizard UX for LIVE duplicate rows** (not soft-deleted): the
  `/enroll/probe` endpoint could extend to return same-hostname-but-
  not-deleted rows with a different banner ("there's already a live
  agent with this hostname — are you sure?"). Out of rc.54 scope
  because the operator could just be reinstalling on the same box.
- **Migrate tunnel-client `TunnelRevoked` to `AttentionReason`
  taxonomy**: the tunnel-client agent has its own attention path with
  its own ad-hoc reason strings. Unify when the tunnel feature ships
  its own structured-reason cycle.
- **`web-push` 2-stack fallback** (if A-fail-3 fires): evaluate `a2`
  crate or fork web-push to add a rustls hyper backend. Push has a
  single consumer in `crates/services/src/push/`, so a fork would be
  small and surgical.
- **Tray SPA migration to `attention_info`**: rc.54 keeps the legacy
  `attention: Option<String>` field per `[CR#2]`. rc.55 or later: the
  tray HTML/JS migrates to read `attention_info.reason` + render a
  coloured chip, then the legacy field can be dropped in rc.56.

---

(End of v2 plan content.)