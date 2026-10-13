# FR-101: Remote-desktop session hygiene — sensitive features off by policy, the audit that exists emitted, sessions that end, the transport hardened

**Issue:** [#2093](https://github.com/gjovanov/roomler-ai/issues/2093) · **Status:** proposed 2026-10-13 —
spec and claim · **Owner:** the agent (permission masking, the transport), the hub (policy, audit, limits),
the viewer (defaults) · **Anchors:** master `c2e79221b` · **Program:**
[StarDesk teardown](../plans/stardesk-teardown-program.md), wave 1 · **Builds on:**
[security baseline](../security-baseline.md), [permissions](../permissions.md),
[FR-27](FR-27-consent-surfaces-and-desktop-companion.md)

## 1. Goal

Close the small, concrete gaps that comparing against StarDesk turned up. None of them is a feature on its
own, and each one is cheap:

1. **Sensitive features by policy.** StarDesk ships clipboard sync, file transfer and audio **off** by
   default. Roomler requests clipboard and files on every session and gives no device or org a switch to
   refuse them.
2. **Audit that is defined but never written.** The clipboard and file audit kinds exist and nothing
   produces them.
3. **Sessions that end.** There is no idle timeout and no maximum duration for remote desktop, and no
   per-caller rate limit on session requests.
4. **The transport.** CBC suites are still offered, the agent's SRTP replay protection is off, and a
   hand-edited `http://` server URL becomes plaintext signalling.

## 2. Evidence (master `c2e79221b`)

| What | Where | Consequence |
|---|---|---|
| The viewer asks for clipboard, files and recording on every session | `ui/src/composables/useRemoteControl.ts` L7677 (`'VIEW \| INPUT \| CLIPBOARD \| FILES \| RECORD'`) | Files on by default wherever the grant allows |
| Clipboard auto-sync defaults **on** | `useRemoteControl.ts` L3631–3641 ("Default ON — only an explicit '0' disables") | The clipboard flows unless the viewer opts out |
| The permission default, and the hub's FILES back-compat | `crates/remote_control/src/permissions.rs` L159–163; `crates/modules/fleet/src/hub.rs` L1016–1025 | Clipboard is in the default set |
| The device/org policy has no switch for clipboard or files | `AccessPolicy` (`crates/remote_control/src/models.rs` L915–948): `consent_mode`, `prompt_owner`, `input_mode`, `rc_overlay` only | An admin cannot say "no file transfer on these machines" |
| Clipboard and file audit kinds have no producer | `AuditKind` (`models.rs` L3736–3800, e.g. `ClipboardWriteToHost` at L3772); a non-test grep finds no writer | `docs/remote-control.md`'s promise to audit transfer direction is unmet |
| `EndReason::IdleTimeout` exists with no producer | `models.rs` L3673 | Sessions run until someone ends them |
| Exec has a per-(caller, device) rate limit; remote-desktop requests have none | `crates/modules/fleet/src/agent_exec.rs` L324–327 (`exec_rate_limiter`) | Only `tower_governor`'s 60/min/IP applies |
| DTLS still offers CBC suites | webrtc-dtls 0.11 default suites include `TLS_ECDHE_{ECDSA,RSA}_WITH_AES_256_CBC_SHA` (registry `cipher_suite/mod.rs` L65–68) | Legacy suites on a path that only ever needs GCM |
| **The agent's SRTP/SRTCP replay protection is off** | No window is set at `peer.rs` L369, so `dtls_transport/mod.rs` L189 passes no option, and webrtc-srtp 0.14 falls back to `srtp_no_replay_protection()` (registry `context/mod.rs` L131–141). The setter exists: `setting_engine/mod.rs` L287 | An on-path attacker can replay captured SRTCP (a PLI forces an IDR; a REMB caps the rate) without decrypting anything |
| Enrollment promotes `http://` to `https://`; the WS URL derivation does not | `crates/agent-core/src/enrollment.rs` L525–531 (loopback exempt); `crates/agent-core/src/config.rs` L3092 (`derive_ws_url` maps `http` → `ws`) | A config edited to `http://` signals in plaintext. The rule does not hold at every entry point |
| coturn sets ports but no listening IP | `turnserver.conf` L9–10 | Whether TURN answers on IPv6 is unverified |

## 3. Design

### 3a. Policy for clipboard, files and audio

- A device-owned **and** a tenant setting per feature: `rc_clipboard`, `rc_files`, `rc_audio`, each
  `off | ask | on`.
- **The agent enforces it** by masking `Permissions` before the session starts, the way the consent floor
  works: the stricter of device and tenant wins. The hub also strips the bits it knows will be refused, so
  the viewer can say why instead of failing later.
- The viewer greys the toggle with the reason.
- ⚠️ The masking is applied at every entry point: session start, a mid-session permission change, and a
  watcher joining.

Whether clipboard auto-sync should default to **off** is the operator's call (§6). It changes everyday UX,
and the policy switch is useful either way.

### 3b. Write the audit that is already defined

- `ClipboardWriteToHost` / `ClipboardReadFromHost` and `FileSentToHost` / `FileSentFromHost` carry
  **counts and byte totals, never content**. File events carry a size and a name hash, not the name.
- `PermissionsChanged`, `WatcherJoined` and `WatcherLeft`.
- The agent reports through the existing `rc:session.stats` channel; the hub writes the rows.

### 3c. Sessions that end

- **An idle timeout:** no input from the controller and no damage on screen for N minutes. It produces the
  existing `EndReason::IdleTimeout`, so no new reason is needed and older servers are unaffected.
- **A maximum duration.**
- Both are tenant settings, with a device floor. Default: off for existing tenants, a documented
  recommendation for new ones.
- **A per-(caller, device) rate limit** on session requests, checked **after** the identity gates (as exec
  does), so a refusal names who was refused.

### 3d. The transport

- **CBC out.** Configure the agent's DTLS cipher suites to the two ECDHE-GCM suites. Chrome has offered GCM
  for years, so nothing that connects today loses.
- **Replay windows on.** `set_srtp_replay_protection_window(64)` and the SRTCP equivalent on the agent's
  `SettingEngine`.
- **`https` at every entry point.** Apply enrollment's promotion (loopback exempt) in `derive_ws_url` and at
  config load, so `ws://` can only ever mean loopback.
- **TURN over IPv6** gets a field check. If coturn does not answer on v6, add `listening-ip` for it.

## 4. Phases

| P | Scope | Kill switch | Status |
|---|---|---|---|
| P0 | This spec and the claim. The red evidence: a DTLS ClientHello pcap with CBC suites; an SRTCP replay accepted in a unit test | docs only | this PR |
| P1a | The transport: CBC out, the SRTP/SRTCP replay windows (`agents/roomlerd/src/rc_transport.rs`, the vendored `set_dtls_cipher_suites`) | `ROOMLERD_DTLS_LEGACY_SUITES=1`, `ROOMLERD_SRTP_REPLAY_WINDOW=0` | **built**: unit tests red/green on an SRTCP replay; the field pcap is owed |
| P1b | `https` at every entry point, and the IPv6 TURN check | an escape for plaintext labs | — **split from P1a** (see the note below) |
| P2 | Policy switches and agent-side masking; viewer reasons | each setting defaults to today's behaviour (`on`) | — |
| P3 | The audit producers | — | — |
| P4 | Idle timeout, max duration, the request rate limit | each off by default | — |
| P5 | Docs and the field table | — | — |

⚠️ **Why P1b was split from P1a (2026-10-13).** Replay windows and GCM-only DTLS
cannot cost a device its connection. Forcing `wss://` can. A config that still holds a
plaintext, non-loopback `server_url` signals over `ws://` today. That could be an
enrollment from before the promotion existed in its current home, or a hand edit. After
an auto-update such a host would dial TLS, fail, and become unreachable, with no remote
path to fix it: a self-wedge. P1b first counts the devices that would be affected,
and gives labs an explicit escape before the default flips.

## 5. Acceptance criteria

- [ ] **AC1 (red first):** a pcap of the deployed agent's DTLS ClientHello lists the CBC suites. After P1 it
  lists only ECDHE-GCM, and every fleet browser still connects.
- [ ] **AC2:** a unit test replays an SRTCP packet into the agent's session. Accepted on today's
  configuration (red), rejected after P1.
- [ ] **AC3:** a config with `server_url = "http://roomler.ai"` signals over `wss://` after P1, while
  `http://127.0.0.1` stays `ws://`.
- [ ] **AC4:** a device with `rc_files = off` refuses file transfer, the viewer shows why, and the agent
  refuses it even when the hub was bypassed (an integration test sending the bit anyway).
- [ ] **AC5:** a clipboard write and a file transfer each produce an audit row with direction, count and
  bytes, and **no content**. A test asserts no payload bytes reach the row.
- [ ] **AC6:** an idle session ends after the configured time with `EndReason::IdleTimeout` and an audit
  row. A session past its maximum duration ends the same way.
- [ ] **AC7:** a caller hammering session requests at one device is refused after the limit with a
  rate-limit reason, and an unauthenticated caller is refused by identity first.
- [ ] **AC8:** TURN over IPv6 is answered, or `listening-ip` is fixed and then answered.
- [ ] **AC9:** **Docs:** `docs/security-baseline.md` and `docs/remote-control.md` describe the policy switches,
  the audit rows (with a mermaid diagram of where they are written), the timeouts and the transport
  settings. Linked from `docs/README.md`.

## 6. Open decisions

1. **Clipboard auto-sync's default.** Recommended: keep it **on** for existing users, add the policy switch,
   and show the state in the viewer. Turning it off changes a daily workflow, which is a product decision.
2. **Idle timeout defaults.** Recommended: off for existing tenants, a 30-minute recommendation shown on the
   tenant settings page.
3. **File-name privacy in audit.** Recommended: a hash, not the name. The audit records that a transfer
   happened, not what the file was called.

## 7. Out of scope

- 2FA / WebAuthn and user-session revocation (open Known Issues).
- DTLS 1.3 (not in webrtc-dtls 0.11).
- Recording, whose rules (FR-85) are unchanged.

## 8. Field-verification log

| # | Date | Cell | Result |
|---|---|---|---|
| — | — | — | — |

## 9. Related

- [FR-100](FR-100-controller-identity.md): the identity binding that ships beside this.
- [FR-85](FR-85-hq-screen-recording.md): recording's rules, which this does not change.
- The program: [`docs/plans/stardesk-teardown-program.md`](../plans/stardesk-teardown-program.md).
