# Roomler Tunnel — install, setup, and test (corporate environment)

A `roomler-tunnel` connection has three sides:

| Side | What it does | Where it runs |
|---|---|---|
| **Tunnel-client** (`roomler-tunnel`) | Listens on a local TCP port on the operator's machine. Each incoming connection rides a **QUIC** stream to the agent by default (transparently falling back to a WebRTC data channel if QUIC setup fails). | Operator's laptop (Win11 / Linux / macOS) |
| **Agent** (`roomler-agent`) | Receives the forward request, dials the destination from inside the corp network, and pumps bytes back. | A host inside the corp network with route to the destination |
| **Server** (`roomler.ai`) | Issues JWTs, enforces the tenant ACL policy, relays SDP / ICE between the two peers (it never sees the payload). | Roomler-managed |

This guide walks through the Win11-on-both-sides flavour. Linux and macOS commands are inline where they diverge.

---

## Transport: QUIC by default, WebRTC fallback

Since `roomler-tunnel` / `roomler-agent` **0.3.0-rc.118**, the data plane defaults to **QUIC** (`quic-v1`, via [quinn](https://github.com/quinn-rs/quinn)) and falls back to the original **WebRTC data channel** (`webrtc-dc-v1`) only if QUIC can't be set up. Tuned QUIC is faster than WebRTC on a relayed path and reaches the same hard networks. Choose explicitly with `--transport`:

| `--transport` | Behaviour |
|---|---|
| `auto` *(default)* | Try QUIC; transparently re-open over WebRTC if QUIC setup fails. |
| `quic` | Force QUIC; error out if it can't be established (no fallback). |
| `webrtc` | Force the proven WebRTC data-channel path. |

**Reaching hard networks** — QUIC has no ICE, so it walks its own connectivity tiers in priority order, reusing the same coturn cluster as WebRTC:

1. **Direct** — dial the agent's host / server-reflexive candidates (best latency, no relay).
2. **QUIC-over-TURN (UDP relay, "Tier 2")** — when direct fails but UDP egress to coturn works.
3. **QUIC-over-TURNS/TCP (TLS relay, "Tier 3")** — when UDP is fully blocked (corporate firewall). Same reach as WebRTC.

The forward logs which path it took (`tunnel established transport=quic-v1 path=relay|direct …`); the relay allocation logs the UDP-vs-TLS sub-tier.

**Mixed fleets** — the server negotiates `quic-v1` only when the target **agent's version supports it** (≥ rc.104); against an older agent it transparently uses `webrtc-dc-v1`, so `--transport auto` is always safe.

---

## 0. Prerequisites

- A Roomler tenant with an admin account
- Two Win11 machines:
  - **`AGENT-CORP`** — joined to the corp network, with TCP reachability to your DB / service
  - **`OPERATOR-LAPTOP`** — the operator's machine, where local clients (psql, ssh, RDP, …) will connect to `127.0.0.1`
- Outbound HTTPS to `roomler.ai` from both machines (port 443)
- Outbound UDP/3478, UDP/443 (or TCP/443 TURNS fallback) to the Roomler coturn cluster from both machines

The same machine can play both roles for smoke-testing — see §8.

---

## 1. Install the agent on `AGENT-CORP`

The agent is the existing `roomler-agent` MSI. Download from your Roomler server (proxied through `roomler.ai` so corp AV trusts it — no `github.com` fetch in the user's network path):

**Win11 (PowerShell as Administrator only if you choose perMachine):**

```powershell
# perUser flavour — installs to %LOCALAPPDATA%, no UAC prompt
Invoke-WebRequest `
  "https://roomler.ai/api/agent/installer/peruser?version=latest" `
  -OutFile "$env:USERPROFILE\Downloads\roomler-agent.msi"
msiexec /i "$env:USERPROFILE\Downloads\roomler-agent.msi"
```

For unattended / fleet installs (boot-time, runs as `LocalSystem`):

```powershell
Invoke-WebRequest `
  "https://roomler.ai/api/agent/installer/permachine?version=latest" `
  -OutFile "$env:USERPROFILE\Downloads\roomler-agent-permachine.msi"
# Run elevated:
msiexec /i "$env:USERPROFILE\Downloads\roomler-agent-permachine.msi"
```

**Linux (.deb):**

```bash
curl -fsSL -o /tmp/roomler-agent.deb \
  "https://roomler.ai/api/agent/installer/peruser?version=latest&os=linux"
sudo dpkg -i /tmp/roomler-agent.deb
```

**macOS (.pkg):**

```bash
curl -fsSL -o /tmp/roomler-agent.pkg \
  "https://roomler.ai/api/agent/installer/peruser?version=latest&os=macos"
sudo installer -pkg /tmp/roomler-agent.pkg -target /
```

---

## 2. Enroll the agent

In the Roomler admin UI:

1. **Admin → Tenant → Agents**.
2. Click **"Issue enrollment token"**. A 10-minute single-use JWT appears with a copy-pasteable `roomler-agent enroll …` command.
3. Run that command on `AGENT-CORP` (the enrollment token is single-use, embedded in the command):

```powershell
roomler-agent enroll `
  --server https://roomler.ai `
  --token <jwt-from-admin-ui> `
  --name "AGENT-CORP"
```

4. Start the agent. Either foreground for testing:

```powershell
roomler-agent run
```

…or register as an auto-start service for production:

```powershell
roomler-agent service install               # perUser scheduled task
# or:
roomler-agent service install --as-service  # SCM service, runs as LocalSystem (requires the perMachine MSI)
```

Verify in **Admin → Tenant → Agents** that the row turned green (`status: online`).

---

## 3. Install the tunnel-client on `OPERATOR-LAPTOP`

**Win11:**

```powershell
Invoke-WebRequest `
  "https://roomler.ai/api/tunnel/installer/windows-x86_64?version=latest" `
  -OutFile "$env:USERPROFILE\Downloads\roomler-tunnel.zip"
Expand-Archive "$env:USERPROFILE\Downloads\roomler-tunnel.zip" `
  -DestinationPath "$env:LOCALAPPDATA\roomler-tunnel"
# Optional — add to PATH:
[Environment]::SetEnvironmentVariable(
  "Path",
  "$env:Path;$env:LOCALAPPDATA\roomler-tunnel\roomler-tunnel-0.3.0-rc.46-x86_64-pc-windows-msvc",
  "User"
)
```

**Linux (.deb):**

```bash
curl -fsSL -o /tmp/roomler-tunnel.deb \
  "https://roomler.ai/api/tunnel/installer/linux-deb?version=latest"
sudo dpkg -i /tmp/roomler-tunnel.deb
# `roomler-tunnel` is now on $PATH at /usr/bin/roomler-tunnel
```

**Linux (plain tarball):**

```bash
curl -fsSL -o /tmp/roomler-tunnel.tar.gz \
  "https://roomler.ai/api/tunnel/installer/linux-x86_64?version=latest"
mkdir -p ~/.local/opt && tar -C ~/.local/opt -xzf /tmp/roomler-tunnel.tar.gz
ln -sf ~/.local/opt/roomler-tunnel-*-x86_64-unknown-linux-gnu/roomler-tunnel ~/.local/bin/
```

**macOS (universal tarball):**

```bash
curl -fsSL -o /tmp/roomler-tunnel.tar.gz \
  "https://roomler.ai/api/tunnel/installer/macos?version=latest"
mkdir -p ~/.local/opt && tar -C ~/.local/opt -xzf /tmp/roomler-tunnel.tar.gz
ln -sf ~/.local/opt/roomler-tunnel-*-universal-apple-darwin/roomler-tunnel ~/.local/bin/
```

Verify:

```powershell
roomler-tunnel --version   # → roomler-tunnel 0.3.0-rc.46
```

---

## 4. Enroll the tunnel-client

In the admin UI:

1. **Admin → Tenant → Tunnels**.
2. Click **"Issue enrollment token"**. A 10-minute single-use JWT appears with a copy-pasteable `roomler-tunnel enroll …` command.
3. Paste-and-run on `OPERATOR-LAPTOP`:

```powershell
roomler-tunnel enroll `
  --server https://roomler.ai `
  --token <jwt-from-admin-ui> `
  --name "Operator laptop"
```

This writes the long-lived `TunnelClient` JWT to:

- Windows: `%APPDATA%\roomler\roomler-tunnel\config.toml`
- Linux: `~/.config/roomler-tunnel/config.toml`
- macOS: `~/Library/Application Support/roomler-tunnel/config.toml`

Verify in **Admin → Tenant → Tunnels** that the row appears.

---

## 5. Create a tunnel ACL policy

Server-side ACL is **default-deny** — without a matching policy, every `TcpForwardRequest` is rejected with `acl_denied`. Define what each subject (user / role / specific tunnel-client) is allowed to reach via which agents.

**Admin → Tenant → Tunnel ACL → New policy.**

For an "ops team reaches the prod Postgres" policy:

| Field | Value |
|---|---|
| Name | `ops-prod-postgres` |
| Subjects | `Role ID` → `<your ops role hex>`, plus optionally `User ID` for specific users |
| Targets | `Agent ID` → `<AGENT-CORP hex from Admin → Agents>` |
| Allowlist | `Exact` `db.intranet.corp` ports `5432–5432`<br>(or `Wildcard` `*.intranet.corp` ports `5432–5432`<br>or `CIDR` `10.0.0.0/24` ports `1–65535`) |
| Max concurrent flows | leave blank, or `64` for tight JDBC-pool bounds |
| Max bytes / session | leave blank, or e.g. `10737418240` for 10 GiB cap |

Click **Create policy**. The row appears in the table and goes live immediately — there's no policy cache in front of MongoDB; every `TcpForwardRequest` reads the live set.

For a tenant-wide catch-all (everyone → every agent → anywhere) **for testing only**:

| Field | Value |
|---|---|
| Subjects | `All users` |
| Targets | `All agents` |
| Allowlist | `Wildcard` `*` ports `1–65535` |

Do not ship that to production — it disables the policy gate.

---

## 6. Open a forward + test

In a regular (non-elevated) PowerShell on `OPERATOR-LAPTOP`:

```powershell
roomler-tunnel forward `
  --agent <AGENT-CORP-hex from Admin → Agents> `
  --local 5432 `
  --remote db.intranet.corp:5432
```

Expected output:

```
INFO connecting websocket   ws_base=wss://roomler.ai/ws
INFO websocket connected
INFO rc:tunnel.opened   session_id=… transport=quic-v1 ice_servers=N quic=true
INFO QUIC: server provided TURN creds — establishing QUIC-over-TURN (relay)
INFO QUIC client: TURN relay allocated   relay_addr=…
INFO tunnel established   transport=quic-v1 path=relay remote=…
INFO listening for local TCP connections (quic-v1)   local=127.0.0.1:5432
```

(On a directly-reachable agent you'll see `path=direct` and no relay line. If the agent is older than rc.104 or QUIC setup fails, `--transport auto` falls back and you'll instead see `transport=webrtc-dc-v1`, `DC pool fully open (8 channels)`, and the WebRTC path.)

In a second shell, test with the real client:

```powershell
psql -h 127.0.0.1 -p 5432 -U <db-user> -d <db-name>
# Or any TCP service: curl http://127.0.0.1:8080/ , ssh -p 22 user@127.0.0.1 , etc.
```

The forward stays up until you press Ctrl-C. Each new `psql` connection opens a new flow to the agent — a fresh QUIC bidirectional stream (or, on the WebRTC fallback path, a `flow_id` over the existing DC pool) — with no per-connection ICE / relay setup.

### Declared routes (daemon-supervised — P6)

A foreground `forward` dies with its terminal, and even `forward --daemon`
(an ephemeral daemon flow) is gone when the device service restarts. A
**declared route** is persisted in the daemon's config
(`[[tunnel_routes]]`) and reconciled back into a live flow on every
daemon start — the set-and-forget form of the same forward:

```powershell
# Requires a local roomlerd (the device service) running on this host.
roomler route add --agent <AGENT-CORP-hex> --local 5432 --remote db.intranet.corp:5432
roomler route ls          # ID / KIND / NODE / LOCAL / REMOTE / TRANSPORT / STATE
roomler route disable pg  # keep it declared, stop the listener
roomler route enable pg   # also clears a terminal `failed` state
roomler route rm pg       # remove entirely
```

Omit `--remote` for a supervised SOCKS5 listener toward the node. The
`STATE` column shows the reconciler's view: `pending` → `active (fl-N)`,
`backoff Ns: <error>` while flow creation retries (e.g. the port is
taken), and `FAILED: <reason>` — terminal, reserved for failures that
can't heal by retrying (enrollment revoked, cross-tenant); fix the cause
and `route enable` to retry. The desktop app's **Tunnels** section shows
and manages the same list.

Notes:
- The daemon writes the routes into its own `config.toml`; on
  perMachine/SYSTEM installs that file lives under `%PROGRAMDATA%`, and
  any *interactive* user of the host can manage routes through the
  LocalAPI (same trust boundary as the existing ephemeral
  `forward --daemon` / consent verbs — the logged-in user of an enrolled
  host is its operator).
- On Linux fleet hosts where `roomlerd` runs as a root systemd unit, the
  LocalAPI socket is root-owned — prefix the CLI with `sudo`
  (`sudo roomler route ls`).
- Routes do not survive a crash-loop auto-rollback to a pre-P6 daemon
  build (the older binary rewrites the config without the field).

---

## 7. Audit + diagnostics

**Admin → Tenant → Tunnels → click a tunnel-client → Audit** lists every `PeerOpen` / `TcpAccept` / `TcpReject` / `TcpDialFailed` / `TcpClosed` with timestamps, bytes-in/out, destination, and reject reason.

**Per-flow debug logs on the tunnel-client:**

```powershell
$env:RUST_LOG = "roomler_tunnel=debug,tunnel_core=info"
roomler-tunnel forward --agent … --local 5432 --remote db.intranet:5432
```

**Per-flow debug logs on the agent**: tail the existing log file:

- Windows: `%LOCALAPPDATA%\roomler\roomler-agent\logs\roomler-agent.log`
- Linux: `~/.cache/roomler-agent/logs/roomler-agent.log`

| Symptom | Likely cause | Fix |
|---|---|---|
| `forward request timed out` after 10 s | agent offline, or no matching ACL policy | Check `Admin → Agents` shows agent online; check `Admin → Tunnel ACL` has a matching policy |
| `acl_denied` from the server | policy gate rejected the (subject × target × destination) tuple | Edit the policy to include the destination, or add subject/target |
| `dial_failed` from the agent | destination unreachable from inside corp | Check from the agent host directly: `Test-NetConnection db.intranet -Port 5432` |
| `cross_tenant` | tunnel-client and agent belong to different tenants | Re-enroll one of them in the correct tenant |
| Never connects (no `tunnel established` / `DC pool fully open` line) | UDP blocked end-to-end **and** the TURNS/TCP (`:443`) relay also failing | Ensure outbound TCP/443 to the coturn cluster from both hosts; check `RUST_LOG=tunnel_core=debug,webrtc_ice=debug` |

---

## 8. Same-Win11-box smoke test (no corp network)

If you want to validate the wire end-to-end before involving the corp network:

```powershell
# Shell 1 — start a TCP listener on the local machine
python -m http.server 9999
# Shell 2 — agent (already installed + enrolled per §2)
roomler-agent run
# Shell 3 — tunnel-client (already installed + enrolled per §4)
roomler-tunnel forward --agent <same-machine agent hex> --local 8000 --remote 127.0.0.1:9999
# Shell 4 — test
curl.exe http://127.0.0.1:8000/
```

The HTTP listing from python's server should appear via the tunnel. Bytes flow: curl → 127.0.0.1:8000 (tunnel-client) → wss://roomler.ai (signalling only) → WebRTC DC → agent dial → 127.0.0.1:9999 (python).

---

## 9. Tear down / revoke

**Stop the forward**: Ctrl-C on `roomler-tunnel forward`. The agent's side closes automatically; audit row recorded.

**Revoke a tunnel-client** (operator left the team, laptop lost, etc.): **Admin → Tenant → Tunnels → click client → Revoke**. The next WS read on that client gets a `rc:tunnel.revoked` message and the WS closes; further enrollment attempts with the same `machine_id` are rejected.

**Revoke an agent**: **Admin → Tenant → Agents → click agent → Revoke**. Same mechanism.

**Soft-delete a policy**: **Admin → Tenant → Tunnel ACL → click delete on the row**. Live tunnel sessions keep working until they close naturally; new `TcpForwardRequest`s that relied on that policy alone start being rejected with `acl_denied` from the next request onward.

---

## 10. Keeping the CLI current

**Tunnel-only hosts** (installed just the `roomler` CLI — the tunnel-client role,
no daemon MSI) self-update in place:

```
roomler self-update            # download + verify + swap the running exe
roomler self-update --check    # report the latest version without installing
```

It polls `roomler.ai/api/tunnel/latest-release`, downloads the matching build
through the same corp-AV-friendly proxy, verifies the SHA-256, and swaps the
binary (Windows today; other OSes are pointed at the manual re-download). This is
the single updater for tunnel-only hosts.

**Daemon hosts** (installed a "be-accessed" role — the agent MSI) do NOT need
this: since the node-stack unification the daemon MSI *carries* `roomler.exe`, so
the daemon's own auto-update refreshes the CLI alongside `roomlerd` on every MSI
upgrade. Running `roomler self-update` on such a host still works but is
redundant — the MSI is the source of truth there.
