# FR-49: A second org gets no mesh, and every surface reports normally

**Issue:** [#TBD](https://github.com/gjovanov/roomler-ai/issues) ·
Status: **P0 — spec** (2026-08-31) · Found while enrolling four demo devices for
[FR-41](FR-41-product-demo-recording.md) (#965).

## Goal

A device enrolled into a second organization should either **be on that org's
mesh**, or **say that it is not**. Today it is neither: `overlay_mode` is forced
to `Off` at enrolment with no way to change it short of hand-editing
`config.toml`, and five separate surfaces report the device as healthy.

## Root cause, with anchors

`crates/agent-core/src/enrollment.rs:328` — every appended org, unconditionally:

```rust
let mut entry = crate::config::OrgEntry {
    label: label.clone(),
    ...
    enabled: true,
    overlay_mode: crate::config::OrgOverlayMode::Off,
```

`agents/roomlerd/src/main.rs:1653-1661` — `--overlay` exists, and sets the
**primary's** flag regardless of which identity was just enrolled:

```rust
if overlay && !cfg.overlay_enabled {
    cfg.overlay_enabled = true;
    tracing::info!("overlay participation enabled by --overlay");
}
```

So `roomlerd enroll --server B --token … --overlay` on a device already enrolled
in A appends org B with `overlay_mode = Off`, and turns on (or re-confirms) the
overlay for **A**. The flag reads as granted and applies to the wrong org.

## Why nobody notices: five surfaces, all normal

This is the part that makes it worth an FR rather than a one-line fix. The
device is genuinely, silently absent from the second org's mesh, and:

| Surface | What it shows | What it omits |
|---|---|---|
| `roomlerd enroll` output (`main.rs:1704`) | "Enrolled into an ADDITIONAL org as …" + "manage them with `roomlerd org ls`" | any mention of the mesh |
| `roomlerd org ls` (`main.rs:1883`) | LABEL · PRIMARY · ENABLED · ORG · SERVER | **no overlay column** |
| `roomler status` (`localapi::OrgStatus`) | label · agent id · enabled · connected · stopped · update-ignored | **no overlay field** — the org's WS *is* connected, so it reads healthy |
| `roomler peers` (`localclient.rs:1398`) | one `── org: <label> ──` section per org **that has peers** | an overlay-off org produces **no section at all** — identical to an org whose peers are simply offline |
| the dashboard | the device row, online | it is the server's view; the server was never told |

The device is online in both orgs, answers exec and SSH in both, and has no mesh
in one. Every indicator the operator has says it is fine.

⚠️ This is the `Some([])` vs `None` distinction the overlay ACL work already
paid for once (`ingress_rules`: `None` = no ACL, `Some([])` = deny — *never*
collapse them), and the same lesson as `ssh_activity`'s **"an empty result is
not evidence of inactivity"**. An org with no mesh and an org with no peers yet
must not render identically.

## And there is no supported way to turn it on

`roomlerd org` offers `ls · rm · enable · disable · set-primary`
(`main.rs:542-563`). None of them touches `overlay_mode`. The only path is to
open `config.toml` — the file that also holds the agent token and the SSH host
private key, is written atomically with a `.prev` sibling, and on Windows lives
under a machine-global ACL-restricted directory. Hand-editing it is not a
supported operation, and a partial write there is the documented
ALL-NUL-corruption class.

## Key design

Three phases, smallest first. **P1 alone closes the reported harm** — the device
still has no mesh, but the operator is told, which is the difference between a
five-minute fix and the two days this cost in the field.

### P1 — say it

- `OrgStatus` gains `overlay_mode: String` (wire-additive; empty from an older
  daemon, exactly as `PeerInfo.org` already handles this).
- `roomler status` prints it per org.
- `roomlerd org ls` gains an `OVERLAY` column.
- `roomler peers` prints a section for an **enabled** org whose overlay is off,
  saying so, instead of omitting it. An org with an overlay and no peers keeps
  its existing empty section — the two states must be distinguishable, which is
  the whole point.

### P2 — make it settable

- `roomlerd org overlay <label> <off|netstack|tun>`, persisted through the same
  `config::save` every other `org` verb uses.
- `enroll --overlay` applies to **the identity actually being enrolled**: the
  primary flag on a primary outcome, that org's `overlay_mode` on an
  `AppendedOrg`/`RefreshedOrg`.
  - ⚠️ Keeps the existing one-way invariant: `--overlay` only ever turns an
    overlay **on**. A re-enrolment must never drop a host out of a mesh it had
    joined (`enrollment.rs`'s merge protects the same property).
  - ⚠️ What mode does `--overlay` mean for a secondary? `tun` requires the
    primary's shared-adapter path (`overlay_multi_org`, `overlay_tun_per_org`)
    and refuses on macOS (`SystemTun::add_address_sync`); `netstack` needs no OS
    privilege at all. Resolved in P2, not assumed here — see Open decisions.

### P3 — scope the output

- `roomler peers --org <label>`, and the same on `status`.
- Motivation is not only ergonomics: on a device enrolled in a customer org and
  a personal one, `peers` prints **every** org's node names together, and there
  is no way to show one. During FR-41 that made the command unusable on camera,
  and it is the same problem for anyone sharing a screen or filing a bug report.

## Phases

| # | Phase | Kill switch | Status |
|---|-------|-------------|--------|
| P1 | Report `overlay_mode` in `org ls`, `status`, `peers` | read-only; nothing to disable | planned |
| P2 | `org overlay <label> <mode>` + `--overlay` targets the enrolled identity | the verb is opt-in; `--overlay` stays one-way | planned |
| P3 | `peers --org` / `status --org` | flag absent = today's output byte-for-byte | planned |

## Acceptance criteria

- [ ] on a device in two orgs, `roomlerd org ls` shows each org's overlay mode
- [ ] `roomler status` shows it per org
- [ ] `roomler peers` renders an overlay-off org **differently** from an org with
      an overlay and no peers — verified by producing both states on one device
- [ ] `roomlerd org overlay <label> tun` puts the device on that org's mesh after
      a restart, verified by a ping across it
- [ ] `enroll --overlay` into a second org enables **that** org, and leaves the
      primary's flag untouched
- [ ] `enroll` without `--overlay` on an org that already had one does not turn
      it off (regression lock on the one-way invariant)
- [ ] `roomler peers --org <label>` prints only that org
- [ ] an older daemon reporting no `overlay_mode` renders as it does today, not
      as "off" (absent ≠ off — the same trap as an absent age reading 0 ms)

## Out of scope

- **Server-side visibility.** The dashboard cannot show this: `overlay_mode` is
  device-owned and deliberately not in `DesiredConfig`. Surfacing it would need
  the device to report it (the `rc:agent.config_status` shape), which is a real
  follow-up but a different mechanism and a different gate.
- **Changing the default.** A secondary org defaulting to `Off` is defensible —
  joining a second mesh has host-global consequences (routes, DNS, a shared
  adapter) and the device owner should opt in. This FR makes the default
  *legible*, it does not argue with it.
- **`overlay_multi_org` / `overlay_tun_per_org` mechanics.** Shipped and default
  ON since rc.339; unchanged here.

## Open decisions

- **What mode `--overlay` selects for a secondary.** Candidates: mirror the
  primary's mode; always `netstack` (no OS privilege, cannot wedge a host);
  require an explicit `--overlay-mode`. Leaning `netstack` for the flag and the
  explicit verb for anything else — a flag that silently installs routes on a
  host is the failure mode "never self-wedge" exists to prevent.
- **Whether `status --org` is worth it** or `peers --org` alone covers the need.

## Field-verification log

_(appended as it happens)_
