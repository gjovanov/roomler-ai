# Remote configuration — enabling exec / SSH from the dashboard

**Status: PLAN, nothing implemented.** This documents a design and the reasoning
that constrains it. Every claim about current behaviour below was checked
against the code at `5b60dacc`; the file notes where a check would need
repeating.

## 1. The problem

Turning on `roomler exec` or roomler SSH for a device today means editing
`config.toml` on that host by hand and restarting the daemon. For one machine
that is fine. For a fleet it is the reason both features are still off nearly
everywhere: the last gate is the one nobody can reach.

The ask is to flip those keys — and other device-owned config — from the web
dashboard, by the device's owner or an org admin, with offline devices picking
up the change when they next connect.

## 2. The tension this design exists to resolve

`exec_enabled` and `ssh_enabled` are gate 4 of four, and gate 4 is documented as
**"the only refusal that survives a compromised server"**. It has that property
for exactly one reason: the server cannot write it.

A naive remote-config feature deletes that property. If the server can set
`exec_enabled`, then an attacker holding the server can set it too, and the
four-gate chain becomes a three-gate chain with a longer description. That is
not a hypothetical distinction — it is the whole reason the key is on disk.

So the design constraint is not "add a config-push message". It is:

> **Make the device remotely configurable without making it server-configurable.**

### The resolution: a device-local opt-in

A new device-owned key, `remote_config_enabled`, **default OFF and never
settable by the server**. The device opts in to accepting pushed config.

This preserves gate 4's meaning rather than eroding it. A device that has not
opted in cannot be opened by any server, compromised or not — the opt-in *is*
the refusal that survives. What changes is that opting in becomes a one-time
local decision instead of a per-key local decision, which is a real reduction in
safety and should be stated plainly rather than glossed: **a host with
`remote_config_enabled = true` has delegated gate 4 to its control plane.** The
default is OFF so that delegation is always a deliberate act.

## 3. Rejected: server-derived state (Design B)

The first plan had the agent derive `exec_enabled` / `ssh_enabled` from server
state at connect, with no local key at all. It was rejected on review for two
independent reasons, both worth recording so it is not re-proposed.

**It breaks break-glass.** Key-list SSH is the documented path for when the
control plane is the broken thing. It works during an outage *because*
`ssh_enabled` is on disk. Server-derived means "server unreachable at boot ⇒ no
SSH", which removes the capability precisely when it is needed.

**It would not have worked anyway.** `overlay.rs`'s `RuntimeFingerprint` — the
guard that decides whether a respawned overlay runtime re-attaches or rebuilds —
contains **no SSH field**. Flipping `ssh_enabled` and respawning re-attaches and
returns early, so `crate::ssh::maybe_intercept` never re-runs and the `SplitTun`
splice never happens. A live flip needed more than the plan accounted for. (This
is why the design below restarts the daemon rather than reconfiguring in place.)

## 4. Multi-org: primary-only

**Verified at `5b60dacc`:** `AgentConfig::for_org` scopes `server_url`, `ws_url`,
tokens, ids, overlay keys, routes and the netstack port — and **none** of
`exec_enabled`, `ssh_enabled`, `ssh_authorized_keys`, `ssh_account_mode`,
`ssh_port`, `ssh_host_key`. A derived org config inherits the primary's by
`clone()`.

So those keys are **host-global** while the server models exec/SSH policy
**per-org**. Left alone, org B's admin flipping a switch would change org A's
access to the same host.

The codebase already answers this shape for `rc:agent.update`:

```rust
// Multi-org P1: the self-updater is machine-wide, so only the PRIMARY
// enrollment may drive it — a secondary org's admin must not force-update
// a binary shared with every other org.
if !ctx.is_primary { /* counter + warn, then ignore */ }
```

Config push takes the same rule: **honored only on the primary org's WS**,
ignored-and-surfaced on secondaries via an `OrgStatus` counter, never silently
swallowed. A machine-wide key may only be driven by the machine's primary
enrollment.

⚠️ This means an org that is a *secondary* on a host cannot enable exec/SSH
there from its dashboard. That is a real limitation and the UI must say so
rather than showing a switch that does nothing.

## 5. Who may flip it

Not just `MANAGE_AGENTS`. Enabling exec on a device is granting a power, and the
role work in #600/#605 established the rule that governs this:

> **You cannot grant a permission you do not hold.**

Enabling `exec_enabled` on a device opens a door; a caller who cannot walk
through that door should not be able to open it for others. So:

| action | requires |
|---|---|
| enable exec on a device | `MANAGE_AGENTS` **and** `EXEC_DEVICE` |
| enable SSH on a device | `MANAGE_AGENTS` **and** `SSH_DEVICE` |
| other device config | `MANAGE_AGENTS` |

Device owners (`owner_user_id`) are subject to the same rule. Both bits are
deliberately absent from `DEFAULT_ADMIN`, which is what makes this meaningful.

## 6. Shape

**Desired state on the agent row.** A `desired_config` sub-document on `agents`:
the keys an operator has asked for, plus who asked and when. This is the
offline story — nothing is "pushed" so much as *reconciled*, and a device that
was offline for a week converges on connect by the same path as one that was
online.

**Reconcile on connect, not just on change.** The agent compares its live config
against `desired_config` after hello. A change while connected is a nudge to
re-run that comparison, not a separate code path. One path means the
offline case is exercised on every single connect rather than only in the case
nobody tests.

**Wire.** A new `ServerMsg` variant carrying the desired keys.

⚠️ It must be gated on a hello capability flag. An older agent does not break on
an unknown frame — the parse-error arm logs at `debug!` and continues, which I
verified — but the frame **vanishes silently**, and the dashboard would show a
change that never landed. The server must know whether the agent understands it,
and the UI must show "device too old" rather than a spinner.

**Apply → persist → restart.** `config::save` already does atomic + fsync +
`.prev` + 0600/ACL, and is already called at runtime by `localapi_state.rs`
(the desktop companion's settings path). ⚠️ It needs the daemon's privilege —
`main.rs` records that a non-elevated `config::save` fails ACCESS_DENIED — which
is satisfied because the daemon is SYSTEM/root.

**Restart, staggered 0–120 s.** `config_surface`'s own doc says every key is read
at daemon startup, so the whole surface is `restart_required = true`. A
fleet-wide push without jitter restarts every device at once. The jitter is
per-device and derived from the machine id, so it is stable across retries
rather than re-rolled.

**Audit.** Every change and every refusal, in the same shape as `exec_audit` /
`ssh_audit`: who asked, which device, which keys, what the outcome was. The
refusals are the load-bearing rows — as in `agent_ssh.rs::dispatch`, the
decision function should return `Result<Applied, Reason>` and one call site
should record both arms, so "a new refusal that forgets to audit itself" stays
unrepresentable.

## 7. What this does not do

- **No bootstrapping problem.** Enabling `exec_enabled` remotely does not
  require exec: the control WS is the channel. Stated because it is the first
  thing that looks circular.
- **No secondary-org control** (§4).
- **No in-place SSH flip** (§3) — enabling SSH restarts the daemon.
- **`remote_config_enabled` is never itself remotely settable.** If it were,
  the whole design would be one push away from meaningless.

## 8. Order

1. `remote_config_enabled` key + config-surface entry, default OFF. Inert alone.
2. `desired_config` on `agents` + the authz rule in §5 + audit collection.
3. Hello capability flag + the `ServerMsg` variant + reconcile-on-connect.
4. Apply/persist/staggered-restart on the agent, primary-only.
5. Dashboard UI, including the "secondary org" and "agent too old" states.
6. Desktop companion: already has a generic settings pane over
   `cmd_config_entries` / `cmd_config_set`, which already exposes
   `exec_enabled` / `ssh_enabled`. It needs the new key surfaced, not a new
   pane.

Steps 1–2 are safe to land before the design is fully settled; step 3 fixes the
wire and should not be rushed.
