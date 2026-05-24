# rc.54 cleanup cycle — wizard probe, TLS unify, structured attention, hub race verify

> **Status**: v0 GO-CANDIDATE. Critique pass recommended before ExitPlanMode.
>
> **Single tag**: `agent-v0.3.0-rc.54`. ~380 LOC, ~22 new tests, ~2 ED realistic.
> Four backlog items deferred from rc.53 v2 (Phase 6 dropped from v0; TLS unify +
> structured `AttentionReason` + Hub race revisit listed as rc.54 backlog).

## Problem this cycle solves

Three small drifts + one verification carried over from rc.53:

1. **Soft-delete-revive operator surprise.** When the wizard enrols a host whose
   `derive_machine_id` matches a *soft-deleted* row server-side, the existing
   enroll handler `rehydrate()`s the row silently (`crates/api/src/routes/remote_control.rs:91-108`).
   Same-`machine_id` continuity is preserved — but when machine_id has
   *changed* (cross-flavour reinstall, ROOMLER_AGENT_ENABLE_SYSTEM_SWAP flip,
   re-imaged host that shares the hostname), the operator gets a NEW row,
   silently. No banner, no "you used to have N deleted enrolments here, OK?".
2. **Three TLS stacks in one agent binary.** rc.53 swapped tungstenite to
   `rustls-tls-native-roots` (`Cargo.toml:176`); vendored `webrtc-ice` is on
   rustls + native roots (rc.32); `reqwest` is still on `native-tls` (Schannel)
   via the implicit `default-tls` feature (`Cargo.toml:103`). One cert-chain
   bug surface per stack × per OS — and a corp-MITM CA that only landed in the
   Windows trust store is now trusted by 2/3 paths but not the 3rd (file
   upload + GitHub releases proxy + enrollment HTTP).
3. **`needs-attention.txt` is freeform text.** rc.53 added 3 fatal-Goodbye
   call-sites all writing multi-line operator prose
   (`agents/roomler-agent/src/signaling.rs:159-188, :190-225, plus :136-151`
   from the pre-rc.53 auth-rejection case). The tray companion's
   `notify::has_attention()` call (`agents/roomler-agent-tray/src/commands.rs:51-52`)
   only knows "yes" / "no" — it can't render a coloured chip per reason. Field
   demand: structured reason on disk so the tray + future admin UI can
   differentiate AgentDeleted from PolicyRejected from ReplacedByNewerEscalated.
4. **rc.53 Phase 2b's `unregister_agent` race fix may be incomplete.**
   `hub.rs:192-211` reads the registered tx under DashMap and removes if
   `ptr_eq` matches. We need an investigation pass — is the read-then-remove
   atomic under DashMap entry locks? Is `ClientTx::same_channel` actually
   pointer-equal? Either pass = no-op rc.54, or pivot to a `ConnectionId`
   UUID stamp (~50 LOC).

## Goals

- Wizard surfaces deleted-row history per hostname before the operator commits
  to a new agent identity.
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

Independent items, no inter-phase dep. Land in increasing complexity so each
phase can ship + be smoke-tested independently:

1. **Phase A** — TLS unify (one-line Cargo feature swap + workspace verify).
   Lowest LOC, highest fan-out risk; lands first so the rest of the cycle
   builds against the unified stack.
2. **Phase B** — `AttentionReason` enum + JSON sidecar + 4 call-site
   updates + tray-watcher prefer-JSON. Pure agent crate; ships independent
   of backend.
3. **Phase D** — Hub `unregister_agent` race investigation (read + targeted
   tests). If a real bug surfaces → fix as Phase D'. If not → "verification
   only" closure note.
4. **Phase C** — backend `POST /api/agent/enroll/probe` endpoint (no SPA
   wiring yet; ships in agent commit so backend can deploy first).
5. **Phase C-SPA** — wizard SPA banner + `useProbe` composable. Ships
   independently — if SPA work runs over, agent crate cycle still tags.

The rc.54 tag fires after Phases A + B + D (+ D' if needed) land. Phase C
backend + Phase C-SPA can ship in a follow-up `agent-v0.3.0-rc.54.1` or
roll into rc.55 if blocked.

---

## Phase A — Unify TLS to rustls + native-roots (~0.2 d, low risk)

**Files**: `Cargo.toml`

```diff
-reqwest = { version = "0.12", features = ["json", "multipart", "cookies", "stream"] }
+reqwest = { version = "0.12", default-features = false, features = ["json", "multipart", "cookies", "stream", "rustls-tls-native-roots"] }
```

`default-features = false` strips the implicit `default-tls = native-tls`
chain. `rustls-tls-native-roots` matches tungstenite + vendored webrtc-ice
behaviour: trust the OS store (Schannel/CryptoKit/system-trust) without going
through native-tls.

### Verification (mars CI; this dev box can't link openssl-sys per `feedback_windows_no_local_backend`)

```bash
cargo check --workspace
cargo tree -e features -i native-tls       # MUST be empty after the swap
cargo tree -e features -i webpki-roots     # confirms no consumer pulled it back
cargo tree -e features -i rustls           # all 3 consumers point at the same root
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
  push (`web-push` crate has its own TLS — verify it doesn't pull
  native-tls; `web-push = "0.10"` default features must be audited).
  → If `web-push` defaults to native-tls, add `web-push = { version =
  "0.10", default-features = false, features = ["isahc-client", ...] }`
  per its rustls path. Open question for impl.
- Linux build is the riskiest: native-tls on Linux uses OpenSSL via
  `openssl-sys`; some transitive may still need it. `cargo tree -e
  features -i openssl-sys` after the swap MUST be empty.

### Smoke matrix entry (SM-A)

Spawn TestApp with rc.54 binary, exercise:
(i) enrollment HTTP, (ii) OAuth (mock), (iii) GitHub-API release fetch via
proxy, (iv) file upload (multipart), (v) Mailpit SMTP (lettre uses its own
TLS feature path — `tokio1-rustls-tls`, unaffected by reqwest swap).

LOC budget: ~5 lines workspace Cargo.toml + lock churn + potentially ~5
lines of `web-push`/`lettre` feature adjustments if they pull native-tls.
**Tests**: 0 new unit; verification = `cargo tree` + e2e Playwright run.

---

## Phase B — `AttentionReason` enum + JSON sidecar (~0.5 d, low–medium risk)

**Files**:
- `agents/roomler-agent/src/notify.rs` (add enum + structured fn; keep
  legacy `raise_attention*` API)
- `agents/roomler-agent/src/signaling.rs:136-151, :159-188, :190-225`
  (4 call-sites)
- `agents/roomler-agent-tray/src/commands.rs:51-55` (prefer-JSON
  reader)

### B.1 — Enum definition

```rust
// agents/roomler-agent/src/notify.rs
/// rc.54: structured reason on the attention sentinel. JSON sidecar
/// written alongside the existing text file; tray + future admin UI
/// can render per-reason chips without scraping prose.
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
files, atomic-ish via temp-then-rename if cheap. Both written every
time `raise_attention_structured` is called.

### B.3 — New API + back-compat

```rust
/// rc.54: structured variant. Writes BOTH the legacy text sentinel
/// (so pre-rc.54 tooling keeps working) AND the JSON sidecar.
/// Path resolution reuses `attention_path_for_worker` so the
/// LocalSystem `%PROGRAMDATA%` routing from rc.53 is preserved.
pub fn raise_attention_structured(
    reason: AttentionReason,
    message: &str,
) -> Result<PathBuf> {
    let (text_path, machine_global) =
        attention_path_for_worker().context("no attention path")?;
    let parent = text_path.parent().context("attention path no parent")?;
    // Legacy text sentinel (unchanged content).
    let text = raise_attention_at(parent, message)?;
    // JSON sidecar.
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

/// rc.54: best-effort cleanup that wipes BOTH files.
pub fn clear_attention_with_sidecar() {
    if let Some(p) = attention_path_for_worker().map(|(p, _)| p) {
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("txt.json"));
    }
}
```

### B.4 — Call-site updates

| File:line | Old | New |
|---|---|---|
| `signaling.rs:144` | `raise_attention(msg)` | `raise_attention_structured(AttentionReason::AuthRejected, msg)` |
| `signaling.rs:172` | `raise_attention_machine_aware(&body)` | `raise_attention_structured(match reason { AgentCloseReason::AgentDeleted => AttentionReason::AgentDeleted, AgentCloseReason::PolicyRejected => AttentionReason::PolicyRejected, AgentCloseReason::ReplacedByNewerConnection => unreachable!() }, &body)` |
| `signaling.rs:214` | `raise_attention_machine_aware(&body)` | `raise_attention_structured(AttentionReason::ReplacedByNewerEscalated, &body)` |
| (also update `auth recovered; clearing attention sentinel` block at `signaling.rs:117-122` to call `clear_attention_with_sidecar`) | `notify::clear_attention()` | `notify::clear_attention_with_sidecar()` |

### B.5 — Tray watcher: prefer JSON sidecar

`agents/roomler-agent-tray/src/commands.rs:51-55`:

```rust
let attention = if notify::has_attention() {
    // rc.54: prefer JSON sidecar so the tray can surface a
    // per-reason chip. Fall back to text-only on schema-version
    // mismatch or absent sidecar (pre-rc.54 agent binary).
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
```

Add `AttentionInfo` struct to `StatusReport` (back-compat: extra fields
are additive on serde side — front-end ignores unknown).

### Tests (in `notify::tests`)

```rust
#[test] fn attention_sidecar_round_trips_for_every_reason() { /* 4 variants */ }
#[test] fn raise_attention_structured_writes_both_files() { /* tempdir */ }
#[test] fn sidecar_schema_v1_is_locked() { /* golden JSON byte assert */ }
#[test] fn clear_attention_with_sidecar_removes_both() { /* tempdir */ }
```

### Tray tests (`agents/roomler-agent-tray/src/commands.rs::tests`)

```rust
#[test] fn status_report_includes_reason_when_sidecar_present() { /* tempdir */ }
#[test] fn status_report_falls_back_to_text_when_sidecar_absent() { /* tempdir */ }
#[test] fn status_report_falls_back_when_sidecar_schema_mismatch() { /* schema=2 */ }
```

### Per `feedback_defensive_enum_catch_alls`

No exhaustive matches on `AttentionReason` exist outside this PR; future
rc.55 admin UI consumers will need `#[allow(unreachable_patterns)]` on
catch-alls until any new variants land. Add a doc-comment on the enum:

```rust
/// NOTE: when adding a variant, any consumer with a defensive
/// `_ =>` arm needs `#[allow(unreachable_patterns)]` per
/// docs/CLAUDE.md "Defensive enum catch-alls" rule.
```

LOC budget: ~200 (enum ~15, structured fn ~50, call-site swaps ~40,
tray watcher ~30, tests ~65).

---

## Phase C — Backend `POST /api/agent/enroll/probe` (~0.4 d, low–med risk)

**Files**:
- `crates/api/src/routes/agent_release.rs` OR new `routes/agent_enroll.rs`
  (prefer NEW file — `agent_release.rs` is already 530 lines, mixed
  concerns; a new module groups all enroll-related public endpoints)
- `crates/api/src/routes/mod.rs` (export new module)
- `crates/api/src/lib.rs:264-285` (mount route)
- `crates/services/src/dao/agent.rs` (new DAO method)

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
/// `POST /api/agent/enroll/probe`.
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
#[tokio::test] async fn probe_rejects_user_jwt() { /* audience-check */ }
#[tokio::test] async fn probe_rejects_agent_jwt() { /* audience-check */ }
#[tokio::test] async fn probe_sorted_most_recent_first() { /* 3 rows, asc deleted_at */ }
#[tokio::test] async fn probe_caps_at_20_rows() { /* 25 rows, len == 20 */ }
```

LOC budget: ~120 backend + 40 tests + 10 mount = ~170.

---

## Phase C-SPA — Wizard banner + `useProbe` (~0.3 d, low risk)

**Files**:
- `agents/roomler-installer/src/commands.rs` (Tauri `cmd_probe_enroll`)
- `agents/roomler-installer/src/main.rs:93+` (register handler)
- `agents/roomler-installer/src/front/app.js:90-109` (call probe in
  `probeAndRender`, render banner on Welcome step)
- `agents/roomler-installer/src/front/index.html` (banner DOM)

### C-SPA.1 — Tauri command

```rust
// agents/roomler-installer/src/commands.rs
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
    // Reuses the existing reqwest client; pulls the
    // already-on-disk machine_id from derive_machine_id BEFORE the
    // install (so wizard already knows what it WILL become).
    let url = format!("{}/api/agent/enroll/probe",
        server.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "enrollment_token": token,
            "hostname": hostname,
            "machine_id": machine_id,
        }))
        .send().await.map_err(|e| format!("probe: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("probe HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await
        .map_err(|e| format!("probe parse: {e}"))?;
    Ok(serde_json::from_value(body["deleted_matches"].clone())
        .unwrap_or_default())
}
```

### C-SPA.2 — Banner UX

When `probe` returns ≥1 row, Welcome step renders:

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

Banner state goes into `state.probe` in `app.js`; Continue advances
the wizard step state machine, Cancel exits.

LOC budget: ~80 SPA + ~20 Tauri cmd = ~100.

**Tests** (`agents/roomler-installer/tests/`): 1 mock-backend integration
covering banner-renders + Continue-progresses.

---

## Phase D — Hub `unregister_agent` race investigation (~0.2 d, no LOC if no bug)

**Goal**: read `crates/remote_control/src/hub.rs:192-211` + the
DashMap source to settle whether the rc.53 fix is sufficient.

### Investigation steps

1. **Atomicity of read-then-remove**: DashMap's `get(&key)` returns
   a `Ref` (read-guard). The current code (`hub.rs:198-210`) reads
   `still_ours` *under* the guard but `remove`s OUTSIDE the guard.
   Between the drop of the read guard and the `remove`, a third
   connection could land and `insert` a new entry. The remove would
   then evict THAT entry.
   - **Test to add** (`hub::tests`): drive 3 concurrent registers +
     1 stale unregister, assert third entry survives.
   - **Likely outcome**: bug exists but is benign (third register
     in <µs window is extraordinarily rare with `tokio::spawn`
     overhead). Decide if worth fixing.
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
  takes `ConnectionId` not `&ClientTx`; identity check is `id ==
  prev.connection_id`. **~50 LOC.**

Decision deferred to investigation finding. Plan defaults to Path A;
flips to B only if the concurrent-3-registers test fails.

LOC budget: ~30 (Path A) or ~80 (Path B).
**Tests**: 2 new (concurrent-3-registers, admin-kick-race).

---

## Phase totals + risk + tests

| Phase | LOC | Risk | New tests |
|---|---|---|---|
| A — TLS unify | ~10 | L | 0 (verification via `cargo tree` + e2e) |
| B — AttentionReason + JSON sidecar + 4 call-sites + tray | ~200 | L–M | 7 |
| C — `POST /enroll/probe` backend + DAO | ~170 | L | 8 |
| C-SPA — wizard banner + probe cmd | ~100 | L | 1 |
| D — Hub race investigation (Path A default) | ~30 | L | 2 |
| D' — only if investigation flags bug | ~50 | M | (+2) |
| **Total (A+B+C+D, no C-SPA, no D')** | **~410** | **L–M** | **~17** |
| **Total (everything)** | **~510** | **L–M** | **~20** |

Engineer-day budget: **~1.5 d realistic, ~2 d defensive**. Phases A + D ship
in <½ day each. Phase B + C are afternoon-each. C-SPA can roll over to next
session without blocking the tag.

---

## Smoke matrix (manual, after CI green on mars)

- **SM-A1** — Plain-internet host: `cargo run -p roomler-agent --
  enroll` + `run`. Verify: reaches server, no TLS handshake regressions.
  Run a `tcpdump -i any -w probe.pcap host roomler.ai`, confirm `Client
  Hello` cipher suite is the rustls one (`TLS_AES_128_GCM_SHA256` etc.)
  not Schannel's `TLS_ECDHE_*_WITH_AES_*` order.
- **SM-A2** — Corporate-MITM host (mkcert + stunnel per SM-3 from
  rc.53 plan, but in front of `https://roomler.ai/api/agent/latest-release`).
  rc.53 binary: fails reqwest call with `UnknownIssuer` (Schannel path).
  rc.54 binary: succeeds. Confirms #1 → rustls + native-roots-via-OS-store.
- **SM-B1** — Trigger AuthRejected (revoke token in admin UI). Wait 3
  failures (15 min on default backoff). Assert:
  - `%APPDATA%\roomler\roomler-agent\config\needs-attention.txt`
    exists with the prose msg
  - `<same>.json` exists, parses, `reason == "auth_rejected"`,
    `agent_version == "0.3.0-rc.54"`, `schema_version == 1`
  - Tray status `StatusReport.attention.reason == "AuthRejected"`
- **SM-B2** — Trigger AgentDeleted via admin delete. Assert sidecar
  `reason == "agent_deleted"` + tray surfaces it.
- **SM-B3** — Pre-rc.54 tray binary reading rc.54 sidecar: graceful
  ignore + falls back to text (back-compat across the tray companion
  upgrade boundary).
- **SM-C1** — Wizard probe round-trip. (a) Enrol host `pc-001`,
  delete it. (b) Launch wizard from same host. (c) Welcome step shows
  banner with `pc-001`'s deleted_at. (d) Continue → enrolment
  proceeds, rehydrates the row (because machine_id matched).
- **SM-C2** — Wizard probe with machine_id MISMATCH (e.g. operator
  flips ROOMLER_AGENT_ENABLE_SYSTEM_SWAP so config_path changes).
  Banner shows `pc-001` with `machine_id_match: false`. Continue
  creates a NEW row, old stays soft-deleted.
- **SM-D1** — 3 concurrent agent registers from 3 fake socket
  handles, 1 stale unregister of the first tx. Verify the latest
  register survives. (Only if Phase D Path B selected; Path A
  test asserts the bug doesn't manifest under realistic timing.)

---

## Auto-fail conditions (force v2 or revert)

- **A-fail**: `cargo tree -e features -i native-tls` non-empty after the
  swap → some transitive (likely `web-push` or `lettre`) still pulls
  native-tls. Either feature-tune that dep or document the 2-stack
  reality and move on (don't gate rc.54 on full unification).
- **A-fail-2**: Linux production hits an `openssl-sys` link error in
  CI. Revert the reqwest swap; ship rc.54 without Phase A.
- **B-fail**: Sidecar JSON encoding sees a serde error (e.g. an
  AttentionReason variant added in a follow-up commit isn't
  serialisable). Wire-format-lock test in `notify::tests` catches
  this in CI.
- **B-fail-2**: Tray binary built against rc.53 panics on rc.54
  sidecar shape. Schema versioning + `.filter(|s| s.schema_version
  == 1)` defends; if the panic surfaces anyway, the fallback path
  catches the parse error gracefully.
- **C-fail**: `/enroll/probe` leaks deleted-but-cross-tenant rows.
  Test `probe_excludes_other_tenants` MUST be in the test file.
- **C-fail-2**: Probe response over-shares (e.g. tenant ID, owner
  user ID). Response struct is explicit field-list (no `..a`); leak
  test asserts the JSON shape contains ONLY the documented fields.
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

1. `chore(agent): unify TLS to rustls + native-roots` (Phase A, ~10 LOC)
2. `feat(agent): AttentionReason enum + JSON sidecar` (Phase B.1-B.3,
   ~80 LOC)
3. `refactor(agent): migrate 4 attention call-sites to structured API`
   (Phase B.4, ~40 LOC)
4. `feat(tray): prefer JSON sidecar + per-reason chip` (Phase B.5, ~30 LOC)
5. `test(hub): displacement-vs-unregister concurrent + admin kick race`
   (Phase D Path A, ~30 LOC) — or commit 5+6 if Path B fires.
6. (optional) `feat(api,wizard): /enroll/probe + Welcome banner`
   (Phase C + C-SPA, ~270 LOC)
7. `chore: bump workspace 0.3.0-rc.53 → 0.3.0-rc.54`
8. Tag `agent-v0.3.0-rc.54` + push.

If Phase C-SPA misses the window: drop commits 6 → second tag
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
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\notify.rs:1-217` —
  Phase B add enum, structured fn, schema-locked sidecar.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\signaling.rs:117-225`
  — Phase B 4 call-site swaps (auth-rejected at :136-151, fatal-Goodbye at
  :159-188, replaced-newer escalation at :190-225, clear at :117-122).
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent-tray\src\commands.rs:40-70`
  — Phase B tray watcher prefer-JSON.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\routes\agent_enroll.rs` (NEW) —
  Phase C probe endpoint.
- `C:\dev\gjovanov\roomler-ai\crates\services\src\dao\agent.rs:54-69` —
  Phase C DAO sibling method.
- `C:\dev\gjovanov\roomler-ai\crates\api\src\lib.rs:264-285` — Phase C
  route mount.
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\hub.rs:192-211` —
  Phase D investigation target (and Path B edit site if needed).
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\commands.rs` —
  Phase C-SPA add `cmd_probe_enroll`.
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\front\app.js:90-109`
  — Phase C-SPA banner integration.

---

## Backlog (post-rc.54, candidate rc.55 items)

- **Admin UI per-reason chip**: agent → server protocol addition to ship
  `last_attention_reason` on heartbeats / hello so the admin agents
  list can render a coloured chip per agent. Wire-format addition on
  `ClientMsg::AgentHello` + `ClientMsg::AgentHeartbeat`, schema
  migration on `agents` collection, Vue3 `AgentsSection` column. ~150
  LOC.
- **Wizard UX for LIVE duplicate rows** (not soft-deleted): the
  `/enroll/probe` endpoint could extend to return same-hostname-but-
  not-deleted rows with a different banner ("there's already a live
  agent with this hostname — are you sure?"). Out of rc.54 scope
  because the operator could just be reinstalling on the same box.
- **Migrate tunnel-client `TunnelRevoked` to `AttentionReason`
  taxonomy**: the tunnel-client agent has its own attention path
  with its own ad-hoc reason strings. Unify when the tunnel feature
  ships its own structured-reason cycle.
- **`web-push` / `lettre` TLS audit**: if Phase A's `cargo tree`
  shows either pulling native-tls back transitively, follow-up with
  explicit feature pinning. Probably ~5 LOC each.

---

(End of v0 plan content.)
