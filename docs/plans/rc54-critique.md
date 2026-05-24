# rc.54 cleanup-cycle v0 — independent critique

Mirrors the rc.53 critique format. All file:line citations verified against current source on commit-head `39470d4` (rc.53 era + 2026-05-24 ops fixes).

Verdict: **READY-WITH-MINOR-EDITS** (12 issues, 3 HIGH, 4 MED, 5 LOW; 8 critical inline fixes before ExitPlanMode).

Date: 2026-05-24. Reviewer: Plan agent. Plan under review: `docs/plans/rc54-cleanup-cycle.md`.

---

## A. Item-by-item check

### Phase A — TLS unify

- ✅ `Cargo.toml:103` — reqwest line confirmed at exactly the cited line (`reqwest = { version = "0.12", features = ["json", "multipart", "cookies", "stream"] }`). Diff is accurate.
- ✅ `Cargo.toml:176` — `tokio-tungstenite = { version = "0.28", features = ["rustls-tls-native-roots"] }`. rc.53 swap matches.
- ⚠️ `web-push = "0.10"` at `Cargo.toml:152` — declared with **default features**. Plan flags this as an "open question for impl" but should verify NOW: web-push 0.10's default feature `hyper-client` pulls `hyper-tls` → `native-tls` transitively. The plan should pre-commit to either `default-features = false, features = ["isahc-client"]` (isahc has rustls support) OR explicitly document the 2-stack outcome.
- ✅ `lettre = "0.11", default-features = false, features = ["smtp-transport", "tokio1", "builder", "hostname"]` at line 111 — lettre is already feature-tuned with NO TLS feature; SMTP transport without TLS. Plan's claim "lettre uses `tokio1-rustls-tls`, unaffected" is **wrong**: lettre currently has zero TLS feature enabled. If your SMTP backend requires STARTTLS, this is already broken (orthogonal to rc.54). Plan should not claim "unaffected" without first running Mailpit smoke.
- 🆕 (LOW) `crates/vendored/webrtc-ice/.../tcp_turn_conn.rs` uses `tokio_rustls::TlsConnector` per plan's own grep — but `webpki-roots` vs OS-store: per `reference_webrtc_rs_turn_gap` and rc.32 ship notes, the vendored ICE fork is on `rustls-native-certs`, NOT webpki. Plan's `cargo tree -e features -i webpki-roots` check is therefore correct as written.

### Phase B — `AttentionReason` enum + JSON sidecar

- ✅ `notify.rs:104-121` — `attention_path_for_worker` returns `Option<(PathBuf, bool)>` exactly as the plan asserts.
- ✅ `notify.rs:47-58` — `raise_attention_at(dir: &Path, message: &str) -> Result<PathBuf>` signature matches plan B.3 usage.
- ✅ `signaling.rs:120` (`notify::clear_attention()`), `:144` (`raise_attention(msg)`), `:172` (`raise_attention_machine_aware(&body)` in FatalGoodbye), `:214` (`raise_attention_machine_aware(&body)` in ReplacedByNewer escalation) — all 4 call-sites are at the lines the plan claims (current source). The match-arm migration is feasible as written.
- ⚠️ `signaling.rs:172` match-arm — plan B.4 uses `AgentCloseReason::ReplacedByNewerConnection => unreachable!()`. Verify against `signaling.rs:119-148` enum decl: variants are `AgentDeleted`, `ReplacedByNewerConnection`, `PolicyRejected`. The deserializer at `:154` rounds **unknown strings to `PolicyRejected`**. The FatalGoodbye arm DOES bind `reason: AgentCloseReason` so the match must be exhaustive — `unreachable!()` is safe because `ReplacedByNewerConnection` flows through `ConnectError::ReplacedByNewer { message }`, never through `FatalGoodbye`. Good — but the plan should add a comment cross-referencing the dispatch site so a future refactor doesn't accidentally route `ReplacedByNewerConnection` through `FatalGoodbye` and trip an UNREACHABLE panic.
- ❌ **`Path::with_extension("txt.json")` filename ambiguity.** `Path::with_extension` REPLACES the existing extension after the LAST `.`. For `needs-attention.txt`, calling `.with_extension("txt.json")` empirically produces `needs-attention.txt.json` (because rust's `with_extension` treats the parameter as opaque, appending after stripping the existing extension), but the exact behaviour depends on `OsStr` split semantics. This needs an **explicit unit test in `notify::tests` that asserts the JSON path is exactly `<parent>/needs-attention.txt.json`** rather than `<parent>/needs-attention.json`. Plan B's "Tests" block lists `raise_attention_structured_writes_both_files` — that test MUST assert the absolute filename, not just file-existence.
- ❌ **B.5 breaks the existing `StatusReport.attention: Option<String>` field** (`commands.rs:30`). Plan says "Add `AttentionInfo` struct to `StatusReport` (back-compat: extra fields are additive)" — but the snippet at lines 268-273 REPLACES the field with `Some(AttentionInfo { path, reason, message })`. Either: (a) add `attention_info: Option<AttentionInfo>` as an ADDITIONAL field, keeping `attention: Option<String>` for back-compat with the existing tray HTML/JS; OR (b) the SPA actually consuming `attention` (the path string) needs the same-commit migration. Plan must pick one — the v0 text is ambiguous and would break the tray's status page on first deploy.
- 🆕 (MED) Plan B doesn't address the **atomicity gap**: if the agent crashes between `raise_attention_at` (text written) and `std::fs::write(&json_path, json)` (JSON sidecar), the tray sees text without JSON → "fall back to text-only" path (acceptable). Conversely, if the JSON is written FIRST and the text is interrupted, `has_attention()` returns `false` (text is the sentinel) → tray ignores the JSON. Current order writes text first, JSON second — correct ordering, but the plan should explicitly call out this invariant ("text MUST be written first; JSON is the optional metadata sidecar").
- ✅ Plan B's `clear_attention_with_sidecar` correctly uses `attention_path_for_worker().map(|(p, _)| p)` per the snippet at line 237 — preserves rc.53's `%PROGRAMDATA%` routing under SystemContext.

### Phase C — `POST /api/agent/enroll/probe`

- ✅ `crates/services/src/auth/mod.rs:50-62` — `EnrollmentClaims { sub, tenant_id, iat, exp, iss, token_type, jti }`. `tenant_id: String` matches plan assumption.
- ✅ `verify_enrollment_token(&self, token: &str) -> Result<EnrollmentClaims, AuthError>` confirmed at `auth/mod.rs:250`.
- ✅ `crates/api/src/lib.rs:264-285` — `public_agent_routes` exists there; mounting `/enroll/probe` between `/enroll` and `/latest-release` is straightforward.
- ✅ `find_by_tenant_and_machine` at `dao/agent.rs:58` — sibling location is feasible.
- ⚠️ Plan C.1 references `state.auth` and `state.agents`. Verify against `AppState` — `remote_control.rs:85` already does `state.auth.verify_enrollment_token` and `state.agents.find_by_tenant_and_machine` ✅.
- 🆕 (HIGH) **Hostname is not unique-indexed per tenant**. Plan's filter uses `"name": hostname`. But there's NO index on `agents.name`. With ~thousands of agents per tenant the soft-delete probe becomes a collection scan — add a sparse index on `(tenant_id, name, deleted_at)` to `crates/db/src/indexes.rs` in the same commit as Phase C, OR document that the probe is bounded by `.limit(20)` which lets MongoDB short-circuit with a `tenant_id`-only scan. Either way, the **plan does not mention this**.
- 🆕 (MED-LOW) **Probe leaks cross-tenant data risk**. Plan's auto-fail C-fail-2 says "Response struct is explicit field-list (no `..a`); leak test asserts the JSON shape contains ONLY the documented fields." But the response includes `agent_id`, `hostname`, `machine_id`, `deleted_at_unix`, `last_seen_at_unix`. `machine_id` is a derived `hostname + os + arch + config_path` hash — potentially identifying. If an attacker has a single valid enrollment token for ANY tenant, they could brute-force-probe hostnames to discover which tenants have which hosts. Fix: rate-limit `/enroll/probe` at the route layer (or scope by `claims.tenant_id` only, which the filter already does — but the cross-tenant CHECK depends on the auth claims being trusted, which they are). Acceptable as designed; mention in security review.
- ⚠️ Plan asserts `a.deleted_at.map(|d| d.timestamp_millis() / 1000)`. `deleted_at` is `Option<bson::DateTime>`. `bson::DateTime` has `timestamp_millis() -> i64`. ✅.

### Phase C-SPA — Wizard banner + `cmd_probe_enroll`

- ✅ `agents/roomler-installer/Cargo.toml:62` — `reqwest = { workspace = true }` already declared.
- ❌ **Plan C-SPA.1 uses ad-hoc `reqwest::Client::new()`** but `asset_resolver.rs:77, :117` already uses `reqwest::Client::builder()` with timeouts. Per the critique charter spot-check #5 — should reuse the same client pattern. Inline `Client::new()` will use default 30s + no TLS-version pin + no User-Agent. Fix: use `reqwest::ClientBuilder::new().timeout(...).user_agent("roomler-installer/0.3.0").build()` matching the existing pattern.
- ⚠️ Plan asserts wizard "pulls the already-on-disk machine_id from `derive_machine_id` BEFORE the install (so wizard already knows what it WILL become)." Plan's `cmd_probe_enroll(server, token, hostname, machine_id)` takes the machine_id as a parameter. But the SPA at `app.js` doesn't currently call `derive_machine_id`. Plan needs an additional Tauri command `cmd_derive_machine_id() -> String` (or extend `cmd_default_device_name`) — **NOT mentioned in the plan**. ~10 LOC missing.
- 🆕 (LOW) Wizard probe runs in `probeAndRender()` at `app.js:90` per plan — but `probeAndRender` runs on bootstrap, BEFORE the operator pastes the token. Plan needs the probe to fire AFTER token-entry, on transition to the Welcome/Continue step. The current `probeAndRender` flow doesn't have the token yet. **Reorder concern**: token entry is step "Token" (per CLAUDE.md rc.28 notes "Welcome / Server / Token / Install / Done"), so the probe wants to fire on Token→Install transition. Plan's "Welcome step renders banner" is **wrong** — Welcome step has no token yet.

### Phase D — Hub `unregister_agent` race

- ✅ `crates/remote_control/src/hub.rs:192-228` — `unregister_agent` at the cited line range. Code matches plan's description (DashMap `get` → `ptr_eq` → `remove`).
- ✅ `ClientTx::same_channel` at `hub.rs:578-579` — confirmed `tokio::sync::mpsc::Sender::same_channel`. Plan's tokio source-reading conclusion is correct: it IS pointer-equal on `Arc<Inner>`.
- ✅ Plan's investigation step (1) is correct: `get` returns a `Ref` (read guard), but `remove` is called AFTER the guard drops. A third register could insert between guard-drop and remove. Plan's analysis is sound.
- ⚠️ **Path A vs Path B decision criterion is fuzzy.** Plan says "Path A if test reliably passes" / "Path B if flaky." Need a concrete threshold: e.g., "1000 iterations of the concurrent-3-registers test under `loom` OR `cargo test -- --test-threads=8 --repeated 200` with 0 failures = Path A." Without this, the decision becomes subjective.
- 🆕 (LOW) Plan D Path B (`ConnectionId(uuid::Uuid)`) doesn't account for the call-sites. `unregister_agent` is called in `routes/ws.rs` (on socket disconnect) and `routes/remote_control.rs::kick_agent`. Adding a `ConnectionId` param means both call-sites must capture the ID at `register_agent` time. Plan says "WS handler" updates — but doesn't enumerate the call-sites. Should add a `grep -rn "unregister_agent" crates/` to the plan.

## B. New issues in v0 (not anticipated in source plan)

1. **(HIGH)** `Cargo.toml:152` — `web-push = "0.10"` defaults pull `native-tls`. Plan's Phase A `cargo tree -e features -i native-tls` will FAIL post-swap. Fix: **pre-commit** to `web-push = { version = "0.10", default-features = false, features = ["isahc-client"] }` in the same Phase A diff. If isahc doesn't satisfy the api's push consumer, evaluate `a2` or fork the dep.
2. **(HIGH)** `agents/roomler-agent-tray/src/commands.rs:30` — `StatusReport.attention: Option<String>` field collision in Phase B.5. Plan must explicitly EITHER (a) replace this field with `attention: Option<AttentionInfo>` and migrate the SPA reader, OR (b) add `attention_info: Option<AttentionInfo>` ADDITIVELY. Pick before implementation.
3. **(HIGH)** No index on `agents.name` per tenant. Phase C probe's `"name": hostname` filter would tablescan. Fix: add sparse index `(tenant_id, name)` to `crates/db/src/indexes.rs` agents block (currently only `(tenant_id, machine_id)` unique exists), OR document the `.limit(20)` short-circuit and accept the perf risk for <10K agents/tenant.
4. **(MED)** Phase C-SPA's probe wiring is mis-staged — runs at bootstrap before token entry. Probe must fire on the Token-step submit handler, not in `probeAndRender()`. ~10 extra LOC for state plumbing.
5. **(MED)** Phase C-SPA missing `cmd_derive_machine_id()` Tauri cmd. Wizard can't supply `machine_id` to the probe without it. ~10 LOC.
6. **(MED)** Phase B `Path::with_extension("txt.json")` filename ambiguity needs an explicit unit-test assertion on the resulting absolute path string (`needs-attention.txt.json` vs `needs-attention.json`).
7. **(MED)** Phase C-SPA uses `reqwest::Client::new()` ad-hoc; should reuse the `asset_resolver.rs` ClientBuilder pattern with timeout + user-agent.
8. **(LOW)** Phase D Path A/B decision needs a concrete pass-rate threshold (e.g., "0 failures in 200 iterations under `--test-threads=8`").
9. **(LOW)** Phase B FatalGoodbye match-arm `ReplacedByNewerConnection => unreachable!()` needs a comment cross-referencing the dispatch site at `signaling.rs:190` so a future refactor can't silently route the variant through this arm.
10. **(LOW)** Plan's claim "`lettre` uses `tokio1-rustls-tls`, unaffected" is incorrect; current `lettre` feature set has NO TLS feature. Either correct the claim ("lettre is already TLS-free, no impact") or audit if Mailpit smoke depends on STARTTLS.
11. **(LOW)** Plan D should add `grep -rn "unregister_agent" crates/` results showing all call-sites for Path B's `ConnectionId` retrofit.
12. **(LOW)** Phase B test list omits a **same-process sequential write race** test (rapid `raise → clear → raise`) — would catch a leftover JSON sidecar from a prior cycle bleeding into the next attention reason.

## C. Verdict

**READY-WITH-MINOR-EDITS** — the plan is structurally correct, all cited file:line refs verify against current source, the 4 phases are independent and well-scoped, and the test matrix is complete enough. The 12 issues above are inline fixes (no structural rework). Before ExitPlanMode, apply these <8 critical inline fixes:

1. **Pre-commit `web-push` feature swap in Phase A** — don't leave it as "open question for impl." Issue #1 (HIGH).
2. **Resolve `StatusReport.attention` field collision** — choose (a) replace + migrate SPA, OR (b) add additive `attention_info` field. Issue #2 (HIGH).
3. **Add sparse index `(tenant_id, name)` to `crates/db/src/indexes.rs`** in Phase C, or document `.limit(20)` rationale. Issue #3 (HIGH).
4. **Move Phase C-SPA probe call** from `probeAndRender()` bootstrap to Token-step submit handler. Issue #4 (MED).
5. **Add `cmd_derive_machine_id()` Tauri cmd** to Phase C-SPA. Issue #5 (MED).
6. **Fix `Path::with_extension` unit test** to assert exact filename. Issue #6 (MED).
7. **Reuse `reqwest::ClientBuilder` pattern** in `cmd_probe_enroll`. Issue #7 (MED).
8. **Correct the lettre TLS claim** in Phase A risk section. Issue #10 (LOW).

Items #8 (Path A threshold), #9 (unreachable! comment), #11 (grep for call-sites), #12 (sequential-write test) can land during impl as TODO comments.

---

Relevant files for implementation (absolute paths):
- `C:\dev\gjovanov\roomler-ai\Cargo.toml`
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\notify.rs`
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent\src\signaling.rs`
- `C:\dev\gjovanov\roomler-ai\agents\roomler-agent-tray\src\commands.rs`
- `C:\dev\gjovanov\roomler-ai\crates\api\src\lib.rs`
- `C:\dev\gjovanov\roomler-ai\crates\api\src\routes\remote_control.rs`
- `C:\dev\gjovanov\roomler-ai\crates\services\src\auth\mod.rs`
- `C:\dev\gjovanov\roomler-ai\crates\services\src\dao\agent.rs`
- `C:\dev\gjovanov\roomler-ai\crates\db\src\indexes.rs`
- `C:\dev\gjovanov\roomler-ai\crates\remote_control\src\hub.rs`
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\commands.rs`
- `C:\dev\gjovanov\roomler-ai\agents\roomler-installer\src\front\app.js`
