# Roomler SSH

SSH into any enrolled node by its overlay address, without installing `sshd`,
without distributing `authorized_keys`, and without opening a port on the host.
The roomler answer to [Tailscale SSH](https://tailscale.com/kb/1193/tailscale-ssh)
— with one capability theirs does not have: **it works on Windows.**

Status: **P1, P2, P3, P5a-d and P4a shipped** — transport, server, the full
four-gate authorization path, the privilege model on both Windows and Unix,
operator consent, and interactive shells on Unix. **P4b (PTY on Windows)** is
next; P6 (client) and P7 (SFTP) are designed, not built.

**Field-proven on `clk00017265`** (a corp-managed laptop with no `sshd`, all
three firewall profiles enabled, and WSL holding loopback `:22`): a session
opened through the full gate chain returns `orfnet\extjovanov` — the signed-in
domain user — with **no listener bound on the SSH port** and no firewall rule
added. Replaying the same grant is refused.

The feature is off at every level
until an operator turns it on: the `ssh-server` cargo feature is not in the
release sets, `ssh_enabled` defaults false, `ssh_authorized_keys` defaults
empty, and every device's `SshPolicy` defaults to `Off`.

---

## 1. Why the packets are intercepted instead of a socket being bound

The obvious implementation is a listener on `<overlay ip>:22`. Measured against
the fleet on 2026-08-19, by actually binding those addresses:

| Host | `overlay:22` | Why |
|---|---|---|
| mars / zeus / jupiter | **EADDRINUSE**, on *both* orgs' addresses | `sshd` holds `0.0.0.0:22`, which covers every local address |
| neo16 | **EADDRINUSE** | `sshd` is bound to `100.65.4.2:22` — the overlay address itself |
| clk00017265 | free, but useless | no `sshd` at all (`OpenSSH.Server` capability is `NotPresent`, corp-managed), WSL's `wslrelay` holds loopback `:22`, and all three firewall profiles are enabled so a new listener needs a rule an ordinary user cannot add |
| pc50045 / pc55331 | free | `sshd` installed but `Stopped/Manual` |

On four of seven hosts a bound socket is impossible. So roomler takes the
packets one layer lower, the way Tailscale does:

```
                                 ┌── dst == self_ip && tcp && dport == ssh_port
  mesh ─▶ WgDevice ─decrypt─▶ SplitTun ──┤       → Netstack (smoltcp) → SSH server
                                 └── everything else → SystemTun → OS stack
```

`bridge::run_bridge` was already device-agnostic over the `TunIo` trait, and the
netstack was already a full userspace TCP/IP stack implementing it, so
[`split_tun.rs`](../crates/tunnel-core/src/overlay/split_tun.rs) is a thin shim
between them. Consequences:

- **No port conflict.** `sshd` keeps `0.0.0.0:22`. Both serve, on different
  paths, on the same host.
- **No firewall rule.** Nothing binds. clk's three enabled profiles are
  irrelevant.
- **Nothing for EDR to kill.** No new service and no new listening socket — the
  failure mode that parked `regal` outbound-only when Kaspersky terminated
  `sshd.exe` as a service.
- **Off-mesh unreachable by construction.** The only way in is a packet that
  already cleared WireGuard. That is a property of the topology, not a policy
  someone can misconfigure.
- **Identical on a locked-down laptop.** In netstack mode the inner device is
  itself a netstack; the split works the same, so a corp host with no winnable
  routing table behaves like a server.

The classifier is deliberately tiny and total over hostile input (an
authenticated peer is not a trusted one): IPv4 + TCP + exact destination +
first fragment only, every field bounds-checked, malformed input answered with
"not ours" rather than a panic.

## 2. Configuration

| Key | Default | Meaning |
|---|---|---|
| `ssh_enabled` | `false` | Serve SSH on this node's overlay address |
| `ssh_port` | `2222` | The intercepted TCP port |
| `ssh_authorized_keys` | *(empty)* | OpenSSH public keys allowed to authenticate |
| `ssh_account_mode` | *(unset)* | What those keys run as: `daemon` \| `console_user` \| `named:<account>`. **Unset = they authenticate but run nothing** |
| `ssh_host_key` | *(minted on first use)* | This node's host identity — **not** exposed on the config surface |

```bash
roomler config set ssh_enabled true
roomler config set ssh_authorized_keys "ssh-ed25519 AAAAC3Nz... goran@neo16"
# restart to apply, then from any peer in the org:
ssh -p 2222 roomler@100.65.4.30 -- whoami
```

**Why 2222 and not 22.** Turning interception on for a port an OS daemon already
serves changes *who answers* for mesh traffic to that address — on neo16 and the
Linux boxes that would silently shadow the `sshd` the fleet SSHes into daily.
2222 lets both coexist during migration; the daemon logs a warning when it
detects an OS listener on the address and port it is taking over. Move a device
to 22 deliberately, per device.

**Host key.** Ed25519, minted on the first SSH-enabled start and stored in
`config.toml`, which already holds `agent_token` — so it inherits the atomic
write with `sync_all`, the `.prev` rotation, `0600` on Unix and the hardened ACL
on a machine-global Windows install, and adds no new secret-at-rest surface. If
it cannot be persisted, SSH stays **off** for that run rather than serving an
identity that changes every restart: a host key that rotates on every boot
trains operators to accept unknown fingerprints, which is exactly the habit that
makes host verification worthless. The fingerprint is logged at start-up so it
can be pinned out of band.

## 3. What a session can do today

- `ssh <node> <command>` — routed through the daemon's existing
  [`exec`](fleet-rpc.md) engine, so it inherits the wall-clock timeout, the
  output ceiling, the per-device concurrency cap, secret redaction and
  process-tree kill. An SSH transport is not a reason to reimplement any of
  that. The cost: output arrives when the command finishes rather than
  streaming, because the engine buffers to enforce its ceiling.
- **`ssh <node>` — a real interactive shell on a real terminal (P4a, Unix).**
  See below.
- Still **refused with a stated reason on stderr**: PTY and shell on *Windows*
  (P4b), SFTP — and therefore `scp` — (P7). A bare channel failure reads as
  "administratively prohibited", which is indistinguishable from a policy
  denial; `scp` silently hanging is a much worse diagnostic than "not
  implemented yet". Port forwarding (`-L`, `-R`) is rejected by russh's own
  defaults, promptly and cleanly.

### Interactive sessions (P4a — Unix)

`ssh <node>` allocates a pty, runs a login shell on it, streams both ways, and
reflows when the client window changes. `agents/roomler-agent/src/pty.rs`.

**A pty session deliberately does not go through the exec engine.** The engine
buffers a command's whole output to enforce a ceiling and returns it at the
end — correct for one-shot commands, and the exact opposite of what an
interactive session needs, where the output *is* the interaction. So a terminal
session has **no output ceiling and no wall-clock timeout**. It keeps the parts
that still apply: the same `RunAs` identity model (via the same
`exec::apply_run_as`, so every refusal is identical and there is not a second
privilege story for shells), the same operator-consent gate, and process-group
teardown.

Mechanics worth knowing:

- **`openpty`, not `posix_openpt`.** The latter needs `ptsname_r`, which does
  not exist on macOS. `openpty` is one call, declared for both platforms, and on
  glibc ≥ 2.34 needs no `-lutil`. Fewer moving parts across a boundary CI never
  compiles — macOS builds only at release-tag time.
- **The parent must close the slave** after spawning, or reads on the master
  never see EOF and a session whose shell exited would hang forever.
- **Linux reports `EIO`, not EOF**, on a master whose slave has been closed.
  Treated as end-of-session; treating it as an error would surface every normal
  shell exit as a transport failure.
- **The child `setsid`s and claims the tty**, so Ctrl-C signals the session's
  process group and not the daemon's.
- **Teardown signals the group with SIGHUP**, not just the shell — killing only
  the shell orphans whatever it was running, which keeps the pty alive.
- The terminal is allocated when the **shell starts**, not when `pty_request`
  arrives: that is when the identity is known and consent has settled, so a
  session about to be refused never gets a pty or a process.

⚠️ **Windows is P4b.** ConPTY (`CreatePseudoConsole` +
`PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` on `CreateProcessAsUserW`) is a different
mechanism; until it lands, a PTY request there is refused with that reason.
This is the platform where roomler SSH matters most — clk cannot host `sshd` at
all — so P4b is the next slice, not an afterthought.

### Which account a session runs as (P5a)

The device's `SshPolicy.account_mode` decides, and the agent maps it through
`RunAs`:

| Mode | Unix | Windows |
|---|---|---|
| `daemon` (default) | root | SYSTEM |
| `named` | **drops to that account** | refused — becoming an arbitrary user needs that user's credentials |
| `console_user` | refused — no console session token exists | **runs as the signed-in user** (`WTSQueryUserToken` + `CreateProcessAsUserW`); refused if nobody is signed in, or if the daemon is not SYSTEM |

**Both paths obey this, and it took a fix to make that true.** A grant carries
the policy's `account_mode`. A key-list session has no policy behind it, so it
uses the device-owned `ssh_account_mode` — and while that was unset it fell
back to the daemon's own identity, meaning listing a key quietly handed out
SYSTEM/root and a policy of `console_user` was simply untrue for that path.
Unset now means the session authenticates and runs nothing, with the reason on
stderr. (Authenticate-then-refuse rather than refuse-the-auth, because it can
explain itself; a bare auth failure cannot.)

The rule the whole type exists to enforce: **never silently run as something
more privileged than was asked for.** A policy that says `console_user` on a
host that cannot obtain that token FAILS the command; it does not fall back to
SYSTEM. Falling back is how an operator ends up believing sessions are
unprivileged while they are root — the worst outcome, because it is invisible
until it isn't. An account mode this agent doesn't recognise (a newer server)
is likewise an error, never a downgrade.

The Unix drop is `setgroups` → `setgid` → `setuid`, **in that order** — after
`setuid` the process can no longer change either, so a reversed order silently
leaves the child in root's supplementary groups, which is the classic
privilege-retention bug and looks like a successful drop from outside. The
result is then verified (`getuid`/`geteuid`) rather than assumed. Account and
group lookup happen in the *parent*: everything between `fork` and `exec` must
be async-signal-safe and must not allocate, since a malloc lock held by another
thread at fork time is never released in the child.

Naming an account that resolves to uid 0 is refused. Not because running as
root is impossible — `daemon` already is root there — but because it has to be
the policy's explicit choice rather than a side effect of naming an account
that happens to be uid 0.

⚠️ `daemon` is still the default, so a device whose policy has not been set
runs sessions as **SYSTEM / root**, exactly like Fleet RPC and for the same
reason. Listing a key in `ssh_authorized_keys` grants root to its holder unless
the policy says otherwise.

## 4. Authorization, and the gap P3 closes

| Gate | Owner | State |
|---|---|---|
| 0 — carrier identity | topology | **live.** The connection cleared WireGuard against a netmap key, so the peer is a specific enrolled node in a specific org — a cryptographic fact, not a claim |
| 1 — org kill-switch (`remote_ssh_enabled`) | server | **live.** Default off, and a *separate* switch from `remote_exec_enabled`: allowing bounded diagnostic commands is not the same decision as allowing interactive sessions |
| 2 — caller permission (`SSH_DEVICE`, `1 << 29`) | server | **live.** A *separate* bit from `EXEC_DEVICE` and, like it, deliberately **not** in `DEFAULT_ADMIN` |
| 3 — `SshPolicy` | server | **live.** `SshMode::Off` default, `can_originate` on the *originating* device, user/role allowlists, `account_mode`, consent |
| 4 — `ssh_enabled` + `ssh_authorized_keys` | the device | **live.** The refusal that survives a compromised control plane |

All four are default-deny, and each is owned by a different party, so no single
compromise is sufficient.

### The API

| Route | Permission | What |
|---|---|---|
| `POST …/agent/{id}/ssh` | `SSH_DEVICE` (gate 2) | Ask for a session. 200 with where to dial, or with which gate refused |
| `PUT …/agent/{id}/ssh-policy` | `MANAGE_AGENTS` | Gate 3. Deciding a device *may* be SSHed into is a management act, distinct from being allowed to do it |
| `GET`/`PUT …/ssh-settings` | `MANAGE_AGENTS` / `MANAGE_TENANT` | Gate 1. Writing needs the higher bar — one switch governs the org |

The device-originated leg (`rc:ssh.request`, for `roomler ssh` from a laptop's
LocalAPI) goes through the **same** `dispatch`, so there is exactly one place
the gates are evaluated regardless of how a request arrived.

A refusal is the server's last word — the session runs over a path it is not
on, so there is no equivalent of exec's device-reported error. Every failure is
therefore enumerated and answered synchronously, each naming which gate said
no; "denied" without a reason turns a five-second config fix into a support
ticket.

### How a grant works (P3a — shipped)

The caller mints an **ephemeral keypair per session** and sends only the public
half in `rc:ssh.request`. If the server authorizes, it pushes
`rc:ssh.grant` — that key, the principal's name, the account mode, an expiry —
to the **target**, and answers the caller with where to dial.

The agent does not verify a signature on the grant, and does not need to: the
frame arrived over the control WebSocket it is already authenticated on, the
same trust path `rc:request` uses to open a remote-control session. No shared
secret with the server, no key distribution, nothing long-lived anywhere.

What the agent *does* enforce locally, because "the server said so" is not a
reason to accept an unbounded table or an eternal key:

- **Single use.** A redeemed grant is removed, so a captured public key cannot
  be replayed into a second session even inside its lifetime.
- **A local deadline**, derived from *arrival* (`Instant`), not from the
  server's wall clock. The server's timestamp can only ever shorten the window
  — a skewed clock or a compromised control plane cannot mint an immortal
  grant. Ceiling: 60 s.
- **A capped table** (16 pending), so the control plane cannot grow agent
  memory. Overflow drops the oldest, which is the one closest to expiry anyway.
- **A session lifetime** (`session_secs`, requested by the caller, clamped both
  sides; 0 = the 12 h ceiling). Enforced at the deadline by disconnect: the
  terminal's process group dies with the pty, and an exec still in flight is
  bounded by the engine's own wall clock. Only key-list sessions are unbounded
  — deliberately, because the break-glass session must not die under whoever
  is fixing the control plane.
- **Gate 4 again.** A device with `ssh_enabled` off refuses to record grants at
  all rather than accumulating credentials it would never honour.

Grants are tried before `ssh_authorized_keys` and take precedence; sessions
authenticated by a grant carry the roomler principal into the log and the audit
record, which a key-list session cannot.

### Operator consent (P5d — shipped)

A device's `SshPolicy.consent_mode` decides whether a human **at the device**
has to approve before anything runs. `auto` is the only value that skips the
prompt; `prompt`, `email`, `push`, and a server that said nothing at all all put
a person in the loop. Absent means ask — the fail-safe direction for a gate
whose entire purpose is human review, and the same rule Fleet RPC applies to
`rc:rpc.exec`.

**The prompt happens at redemption, not when the grant arrives.** By grant time
the server has already answered the caller with where to dial; refusing there
would surface as a connection that rejects them for no stated reason. At exec
time the session is live, so the refusal has somewhere to explain itself — and
the caller is told *before* the wait begins:

```
roomler-ssh: waiting up to 30s for approval at the device…
```

Without that line a policy of `prompt` is indistinguishable from a 30-second
hang. Denial and timeout both refuse with their own reason and exit 1.

⚠️ **The broker is threaded, not discovered.** The consent broker rides
`ssh::SessionServices` from the daemon through `overlay::maybe_start` into the
server's `Ctx`, and the field is non-optional — a server that can be
constructed at all provably has someone to ask. (It used to be a process
global, `consent::set_shared`, whose absence the server had to treat as a
denial; "no broker ⇒ deny" is now unrepresentable rather than documented.)

⚠️ **A denied session burns its grant.** Authentication consumes the grant
*before* the prompt runs, so a deny — or a timeout — means requesting a new
grant, not re-dialling with the same key. Correct by construction (single-use
is single-use, and a refused session must not leave a redeemable credential
behind), but surprising if you expect a simple retry.

⚠️ **Key-list sessions are deliberately exempt.** They carry no server policy,
and they exist as the break-glass route for when the control plane is the broken
thing. Gating them on a prompt would remove the emergency path exactly when it
is needed. The device owner already consented by listing the key and setting
`ssh_account_mode`.

## 5. Roadmap

| Slice | Scope | State |
|---|---|---|
| P1 | Transport seam (`SplitTun`, netstack termination) | **shipped** |
| P2 | russh server, publickey auth, `exec` via the exec engine | **shipped** |
| P3a | Wire protocol (`rc:ssh.request` / `.grant` / `.response`), `SshPolicy` + `SshMode` + `SshAccountMode` models, `SSH_DEVICE` bit, agent-side grant table + redemption, `ssh` capability | **shipped** |
| P3b | The server half: `agent_ssh.rs` (gates 1-3 + grant minting), hub push, the `ssh_policy` + org-settings API, and the `rc:ssh.request` device leg | **shipped** |
| P3c | Admin UI for the two policies, and `exec_audit`-style auditing of SSH grants (today a denial is logged, not persisted) | next |
| P5a | `RunAs` + the never-silently-escalate rule; Unix named-account privilege drop (setgroups→setgid→setuid, verified); every unsupported mode refused | **shipped** |
| P5b | Windows console-user sessions (`WTSQueryUserToken` + `CreateProcessAsUserW` with captured output) | **shipped** rc.418, field-proven |
| P5c | `ssh_account_mode` — key-list sessions obey an explicit identity instead of silently taking the daemon's | **shipped** rc.419, field-proven |
| P5d | `SshPolicy.consent_mode` honoured — a policy of `prompt` now prompts, and refuses when nobody can be asked | **shipped** |
| P4a | PTY / interactive shell on **Unix** (`openpty`, login shell, resize, group teardown) | **shipped** |
| P4b | The same on **Windows** — ConPTY + `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` on `CreateProcessAsUserW`. The platform that needs this most | next |
| P6 | `roomler ssh <name>`, netmap-verified host keys (no TOFU), stdio `ProxyCommand` | designed |
| P7 | SFTP subsystem, `-L`/`-R`/`-J` | designed |
| P8 | Audit + session recording + admin UI | designed |

## 6. Build

`ssh-server` is a cargo feature (it implies `overlay-netstack`), **not** in the
default release set.

Measured by building `roomlerd` both ways on x86_64-pc-windows-msvc with
`codegen-units=1`: **29.939 MiB → 31.795 MiB, +1.856 MiB (+6.2%)**. At the
MSI's measured 2.67× compression that is roughly **+0.7 MiB** on the installer.
(An isolated harness had predicted +2.57 MiB; the real binary absorbs more,
because it already links a much larger std/tokio/webrtc surface for the linker
to share against.)

Little of that sharing is crypto, though: russh 0.62 sits on the next
RustCrypto generation (`aes-gcm` 0.11 vs our 0.10, `curve25519-dalek` 5 vs 4,
`p256` 0.14 vs 0.13, `sha2` 0.11 vs 0.10), so the binary genuinely carries a
second, parallel primitive stack and the dependency graph grows by ~99 crates —
worth weighing against the P3e trend that cut the tray from 470 to 321.

Two build constraints that must not be lost:

1. **`default-features = false, features = ["ring"]`.** russh's defaults are
   `["flate2", "aws-lc-rs", "rsa"]`, and aws-lc-rs is a C/NASM build that would
   break the deliberate ring-only / no-openssl invariant `tunnel-core` maintains
   for QUIC.
2. **`rsa` stays off.** It costs ~0.32 MiB and drags a `0.10.0-rc` pre-release
   into the graph. Roomler SSH authenticates ed25519.

russh has a real CVE history — CVE-2023-28113 (insufficient DH validation) and
RUSTSEC-2026-0154 / CVE-2026-46673 (unbounded allocation, CVSS 7.5, fixed in
0.60.3) — and releases fast. Pin it, watch it, and get `cargo audit` into CI
before this ships enabled.
