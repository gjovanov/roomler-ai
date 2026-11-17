# roomler-desktop — the companion app

**Status:** FR-84 ([#1633](https://github.com/gjovanov/roomler-ai/issues/1633)) — every
phase merged; field verification rides `agent-v0.4.103` (the FR's field-verification log
records each result).

`roomler-desktop` is the tray + window app a person uses on an enrolled machine. It is a
**thin client of the local daemon**: every number it shows and every change it makes goes
through the LocalAPI to `roomlerd`, which runs as SYSTEM / root (or as the user, for a
per-user install). The companion never holds a user credential and never talks to the
server on its own — when it needs server data (the devices this machine can see), the
daemon fetches it with the device's agent token and hands it over.

It is also one of the consent **prompt surfaces** (FR-27): when the daemon cannot draw a
native panel, the companion shows the "someone wants to view your screen" prompt.

| | |
|---|---|
| Crate | `agents/roomler-desktop` (Tauri 2; Rust in `src/`, a plain-JS front in `src/front/`, no bundler) |
| Window | one 1100×740 main window (close = hide to tray) + two always-on-top panels (consent, "being viewed") |
| Talks to | the LocalAPI only — `\\.\pipe\roomler` on Windows, a Unix socket elsewhere |
| Ships in | the agent release (standalone EXE on Windows; on Linux a `.deb` and an `.rpm`, each for x86_64 and aarch64 (FR-93 P6d, P6e; §10.1); the `.pkg` on macOS) |

## 1. Where it sits

```mermaid
flowchart LR
    subgraph host["the enrolled machine"]
        D["roomler-desktop<br/>(the person's session)"]
        CLI["roomler (CLI)"]
        API["LocalAPI<br/>pipe / socket"]
        R["roomlerd<br/>(SYSTEM / root / user)"]
        D -- "JSON request per connection" --> API
        CLI --> API
        API --> R
    end
    R -- "agent token<br/>/api/agent/self/*" --> S[("roomler server")]
    R -- "control WS (rc:*)" --> S
```

⚠️ The LocalAPI is the trust boundary. On Windows its SDDL grants **SYSTEM, Administrators
and the daemon's own account** in full and **Interactive Users** reading and writing data
only, at medium integrity (`crates/localapi/src/lib.rs:4429-4430`): any person logged in at
the machine can call it, but only the first three add an instance of the pipe, and a
sandboxed low-integrity process cannot call it. Everything
below that lets the companion *change* something is therefore something any interactive user
can reach, and the daemon decides who may — which is why the device-side rules (the
`files_dir` SYSTEM rule, the visibility rule on the Devices page) are enforced in the daemon
and on the server, never in the page. A consent answer and the banner's Disconnect are taken
only from the person at the screen the daemon shows, SYSTEM or an elevated administrator
(§12.4). The console person's own settings, recording, keep busy and external-access password
follow the same screen on Windows (§12.4).

## 2. The pages

Hash routes in `src/front/app.js`; one `<section id="view-*">` per page. The install matrix
(vmtest, FR-61) asserts that every `#view-<name>` renders — keep those ids.

| Page | Route | What it shows | Data (LocalAPI) | Refresh |
|---|---|---|---|---|
| Overview | `#/overview` | this device (name, overlay IPs, server, service, versions), exit node, updates | `Status` (via the shared device view) | on the 2 s device-view tick |
| Devices | `#/devices` | the devices this machine's mesh can see: a server-side grid (search, sort, paging, column chooser) and the mesh graph — see §6 | `Devices` / `Mesh` (daemon-proxied), live connection cells from `Peers` | list 30 s and mesh 60 s while visible; peers 2 s |
| Recordings | `#/recordings` | local and remote screen recordings (FR-85) | FR-85's verbs | on entry |
| Routes | `#/tunnels` | declared routes (`[[tunnel_routes]]`) and live forwards, with add / edit / enable / disable / remove | `RouteList` + `Flows` on **one** connection; `RouteAdd` / `RouteUpdate` / `RouteSetEnabled` / `RouteRemove` | 2 s while visible |
| Settings | `#/settings` | the config surface, grouped (§5); device name; service; the log | `ConfigGet` / `ConfigSet`, `SetDeviceName`, `TailLog` | on entry |
| Onboarding | `#/onboarding` | enrol (server, name, token) or re-enrol | in-process enrolment | — |
| Welcome | `#/welcome` | the first-run tour — see §10 | `Status`, `ConfigSet`, `RestartDaemon`, the files commands | while a step waits |

## 3. The LocalAPI, and why the Routes page used to blink

Every companion request opens the pipe **fresh**, sends one JSON request (or a short batch
on one connection) and closes. Five things poll it at once in a running companion, and a sixth
while the prompt window asks a question:

| Poller | Where | Interval | Connections per tick |
|---|---|---|---|
| service status | `src/front/app.js:190` | 10 s | 0 (reads config, runs `roomlerd service status`) |
| device view (`Status` + `Peers`) | `src/front/app.js:192` → `cmd_device_view`, `src/commands.rs:335` | 2 s | 1 |
| Routes (`RouteList` + `Flows`) | `src/front/tunnels.js:550` → `cmd_tunnels_view`, `src/commands.rs:2838` | 2 s, visible only | 1 |
| consent (in-window) | `src/front/consent.js:200` | 1.5 s | 1 |
| consent watcher (Rust) | `src/main.rs` | 750 ms | 1 |
| consent (the prompt window, §10.9) | `src/front/panel-consent.js:210` | 750 ms | 1 |

**What went wrong (field, 2026-09-25):** the Routes page's tables appeared and disappeared
every few seconds while the daemon's data was steady. Three raw back-to-back opens of the
pipe — exactly what the Rust client does — failed the 2nd open with `ERROR_PIPE_BUSY` (231)
**60 of 60 times**: the server kept **one** listening instance and created the next only
after `connect()` returned. The companion opened three connections in the same millisecond
every 2 s; the client retried once after 50 ms; and the two Routes commands turned the
loser's error into an empty list, which the page painted as "no routes".

⚠️ A .NET `NamedPipeClientStream.Connect(0)` probe of the same pipe reported 90/90 OK — it
calls `WaitNamedPipe` first, and `0` there means the server's *default* wait (50 ms). A
probe that waits cannot see a gap a non-waiting client falls into.

**The fix, in three layers:**

```mermaid
sequenceDiagram
    participant P as Routes page
    participant C as cmd_tunnels_view
    participant Pipe as \\.\pipe\roomler (pool of 4)
    participant D as roomlerd
    P->>C: refresh (seq = n)
    C->>Pipe: open (PIPE_BUSY → back off 5…160 ms, ≤ 315 ms)
    Pipe->>D: RouteList
    D-->>C: routes
    C->>D: Flows (same connection)
    D-->>C: flows
    C-->>P: {available, reason, routes, flows}
    Note over P: n older than the newest applied? drop.<br/>available = false? keep the last good data,<br/>show "unavailable since … · N failures · reason"
```

1. **The page never paints an error as data.** `cmd_tunnels_view` answers
   `{available, reason, routes, flows}` from one connection; `tunnels.js` keeps the last good
   data on a failure and says why (`noteUnavailable`, `src/front/tunnels.js:98`), drops a
   response older than the newest it applied, and patches rows in place keyed by route id /
   flow id (`routeRows`, `:48`) so a button never moves under the cursor. Refresh failures
   are also written to the companion's own log (`src/desktop_log.rs`, capped at 2 MiB,
   one rolled generation).
2. **The pipe always has a spare.** `serve_windows_at` (`crates/localapi/src/lib.rs:4529`)
   binds the first instance (retrying while another process holds the name), then runs a
   pool of accept tasks (`accept_pipe`, `:4484`), each owning one listening instance and
   re-creating it before handing a connected one off. Default 4
   (`DEFAULT_PIPE_POOL`, `:4223`); config `localapi_pipe_pool` (1–16) — **`1` is the kill
   switch** and reproduces the old single-instance behaviour.
3. **The client backs off instead of giving up.** `pipe_busy_backoff` (`:5467`) retries a
   busy pipe at 5, 10, 20, 40, 80, 160 ms (≤ 315 ms in total); any other error (the daemon
   is not running) returns at once.

A route can now be **edited** in place: `RouteUpdate` replaces the descriptor in one save
under the config write lock and restarts its flow; an invalid replacement is refused and the
old route keeps running (`agents/roomlerd/src/tunnel/route_reconciler.rs:437`). A daemon
that predates the verb answers "bad request", which the companion shows as *"The device
service predates route editing"* (`src/commands.rs:2232`), never as a blank page.

⚠️ A route whose edit keeps the **same local port** restarts its listener on that port. If
the old listener is still held (the #1035 class — a listener outliving the flow that owned
it), the new flow retries on its backoff until the port frees.

**A route's state is the port's truth (#1685).** `active` is shown only once the route's
tunnel session is up and its local listener is **bound** — the daemon flips it from the bind
itself (`tunnel_core::driver::ListeningHook`), not from `rc:tunnel.opened`, which the server
also answers for a node whose data plane then fails to come up. Until then the page shows
`connecting` (first attempt in flight) or `retrying (N, next in Ss) — <last error>` (the
session failed N times in a row), and appends `· device offline` when the Devices data
already knows the target is offline. Field, 2026-09-25: three routes to an offline laptop
read `active` with nothing listening while the daemon log looped `tunnel session failed;
retrying`. The wire keeps its state words and adds fields (`flow_id`, `attempts`), so a
companion older than the fix shows those routes as `pending` / `retrying in Ss: <error>`
rather than losing the page to a tag it cannot parse; a companion from the fix on shows a
state word it does not know (a newer daemon) as that word, never as an error
(`stateLabel`, `src/front/tunnels.js`).

## 4. Polling rules every page follows

- **Poll only while visible.** A page's own timer runs only while its view is the current
  one; re-entering a view never stacks a second timer. The one exception is the shared
  device view (2 s), which the sidebar status dot and the Overview both read.
- **One connection per refresh.** A page that needs two answers asks for both on one
  connection (`cmd_device_view`, `cmd_tunnels_view`).
- **Stale-while-error.** A failed refresh never replaces data with an empty state; it keeps
  the last good data and says how long it has been stale and why.
- **A sequence guard.** A response older than the newest applied is dropped.
- **Keyed rows.** Rows are patched in place by a stable key; `replaceChildren` on every tick
  destroys focus, hover and in-flight clicks.

## 5. Settings, grouped

The config surface (`crates/agent-core/src/config_surface.rs`) is one table of
`KeyMeta { key, kind, group, tier, live, description }` (`:268`) — adding a key without
saying which group and tier it belongs to, and whether the daemon applies it live, does not
compile.

| Group | Example keys | Default state |
|---|---|---|
| **Essentials** (every `tier = essential` key, shown first) | `overlay_enabled`, `auto_grant_session`, `exec_enabled`, `remote_config_enabled`, `ssh_enabled`, `encoder_preference`, `power_policy`, `auto_update` | **open** |
| Access & consent · Private network · Network carriers & relays · Routing & adapter · Tunnels & SOCKS · Roomler SSH · Remote desktop · Capture & input · Video encoding · Video rate & latency · Files · Device & service | the other ~140 keys | collapsed |

- **`restart_required` is per key.** `live = true` only where the daemon really applies a
  change at once — `exec_enabled` and `remote_config_enabled` (the LocalAPI `ConfigSet`
  re-seeds them through `adopt_local`). Every other key is read once at startup.
- **"Modified"** compares a value with the key's built-in default, computed from a config
  that contains nothing but the identity fields (`builtin_default`, `:1481`).
- **Which daemon it edits (#2871).** `cmd_config_entries` / `cmd_config_set` go to the
  daemon `localapi::connect()` resolves — on a Mac running both halves (FR-43), the
  per-user GUI worker, whose `config.toml` is its own: FR-43 keeps the worker enrolled
  through P2. Meanwhile the Overview's network lines are the root half's (§6), so the two
  pages described different daemons and nothing said so: a person switching `ssh_enabled`
  on here changed a file the mesh node never reads (measured on 0.4.169). **Design A
  (#2871, the operator, 2026-10-25):** the root half's person socket now serves the
  configuration — the listing, with every key it will not set marked
  `read_only = "root_only"`, and `ConfigSet` of exactly the keys that are the console
  person's own call (`localapi::CONSOLE_PERSON_KEYS`, `crates/localapi/src/lib.rs:3463`;
  the device-owned gates are not among them) — and the page shows the Mac as one device:
  each row comes from, and a change goes to, the half that READS the key. The split is the
  key's group (`config_surface::Owner`, `crates/agent-core/src/config_surface.rs:212`):

  | Owner | Groups | On a two-half Mac | Examples |
  |---|---|---|---|
  | **session** — the per-user half, which serves the remote-desktop session | Remote desktop · Capture & input · Video encoding · Video rate · Files | the worker's row, edited on the worker as before | `rc_clipboard`, `privacy_mode_allowed`, `lock_on_end`, `display_match`, `peer_auth`, `keep_busy_enabled`, `record_*`, `files_dir`, `encoder_preference` |
  | **machine** — the root half, the device `roomler ssh` / `roomler exec` reach | Access & consent · Private network · Carriers · Routing · Tunnels · Roomler SSH · Device & service | the root half's row, over its person socket: editable where that socket sets the key (`external_*`), otherwise read-only with the administrator's command | `exec_enabled`, `ssh_enabled`, `remote_config_enabled`, `overlay_enabled`, `auto_grant_session`, `auto_update`, `external_access_enabled` |

  `merge_two_halves` (`agents/roomler-desktop/src/commands.rs:636`) builds the rows —
  machine-side keys from the root half's listing, in the worker's order — and
  `cmd_config_set` (`:828`) sends a machine-side key to the root half
  (`root_half_config_set_beside`, `:753`), whose answer is final: the worker's copy of
  such a key is a file the Mac's device never reads, so it is not written. The worker's
  own copies of machine keys stay reachable with `roomler config …` in a terminal, and
  the CLI's note says so. ⚠️ Two caveats the split cannot express, both P3's to dissolve:
  the root half forwards its own value of a few session knobs to the worker as a floor
  (FR-43's `WORKER_ENV_FORWARD`), and a delegated session's policies are judged by both
  halves' configs, so the worker's copy — the one this edits — can only tighten the
  root's. The banner (`cmd_config_target`, `:1187`; `two_halves_note`, `:1007`) names both
  halves' files and says which rows change here. It is shown only when the reached daemon
  says it runs unprivileged AND a root daemon of another identity answers `Status` over the
  person socket (`two_halves`, `:990`) — one daemon behind both sockets, a root daemon's
  legacy per-user bind, is not two halves. A root half older than design A still refuses
  the listing; the rows are then the worker's alone and the banner carries the #2879 words
  (`two_halves_legacy_note`, `:1028`): the root half's switches are
  `sudo roomler config set <key> <value>`.
- **A Linux system install, and a Mac with a root half alone (#2891, #2871).** There the
  daemon the page reaches runs as root and this app does not: `connect()`'s first candidate
  on Linux, `$XDG_RUNTIME_DIR/roomler.sock`, IS the root daemon's person socket (§12). Until
  #2891 the page took that socket's refusal of `ConfigGet` for a daemon that was down, fell
  back to the per-user config file — a path a system install does not have — and showed
  "Could not load the configuration" with a Retry that never helped; with a stale per-user
  file left by an earlier per-user install it would have read, and `cmd_config_set`
  written, a file no daemon reads (#2871's failure, on Linux). #2891 made a root daemon's
  refusal final for `cmd_config_entries`, `cmd_config_set` and `cmd_set_device_name`: none
  falls back to the file (`refused_by_root_daemon`,
  `agents/roomler-desktop/src/commands.rs:1143`). Since design A the same socket serves the
  listing, marked: `cmd_config_target` (`:1187`) reports `root_only` (`root_daemon_only`,
  `:1053`, pure: the reached daemon says it runs as root, this process does not, and the OS
  is not Windows, whose pipe admits interactive users to those verbs), the person's own keys
  are editable, and the rest — the device-owned gates, the network, SSH, the updater, the
  recordings folder — show the root daemon's value with "administrator's" and
  `sudo roomler config set <key> <value>`
  (`settings.js`, the `read_only` row; `root_only_note`, `:1080`). From a daemon older than
  design A the card still shows the daemon, its file (`Status.config_path`:
  `/etc/roomler/config.toml`) and the administrator's commands in place of rows
  (`root_only_legacy_note`, `:1102`).
- **The Service line on Linux and macOS (#2871).** `probe_service_state` reads the root
  daemon's own `--as-service` answer — `com.roomler.daemon: running, enabled`,
  `roomlerd.service: active, enabled` (`system_answer`, `:3447`) — as
  `service_kind = "systemDaemon"`, rendered "System daemon (root) · running". It only ever
  read Windows' `Roomler:` line before, so a supervised Mac, whose LaunchAgent FR-43 P2d
  boots out, said "not installed" while its root daemon served the mesh.
- A daemon that predates the grouping sends entries without `group`; the page falls back to
  the flat list.
- Every new agent knob is registered here (the standing rule): the `config.rs` field, the
  `KeyMeta` entry, the `current_value` / `apply` arms, and a set/echo test.

## 6. What the companion may know about other devices

The Devices page shows **the devices this machine's overlay netmap already carries, plus
itself** — never the organisation's inventory. The companion cannot sign in as a user, so the
daemon asks the server with its **agent token**:

| Route | Auth | Returns |
|---|---|---|
| `GET /api/agent/self/devices?page&per_page&q&sort&dir&kind` | agent JWT (status-checked on every use) | `VisibleDevicesPage` — rows with display name, name, OS, version, presence, overlay IP, MagicDNS, tags, reachability, `is_self`; plus `overlay` (`ok` / `no_node` / `no_network` / `unavailable`) and `acl_mode` |
| `GET /api/agent/self/mesh` | agent JWT | the web dashboard's mesh payload, restricted to the same set, with a server-computed `label` per node and `self_node_id`; `{"enabled": false}` when stats are off |

How the set is computed (`crates/api/src/routes/agent_self.rs:107`):

```mermaid
flowchart TD
    A["agent token → tenant + agent"] --> B{"network module mounted?"}
    B -- no --> Z["self only · overlay = unavailable"]
    B -- yes --> C{"overlay network exists?<br/>(read-only lookup)"}
    C -- no --> Z2["self only · no_network"]
    C -- yes --> E{"this device has a live node?"}
    E -- no --> Z3["self only · no_node"]
    E -- yes --> F["shape_full_netmap(self node)<br/>= the exact join-time ACL shaping"]
    F --> G["visible = self ∪ netmap peers"]
```

- **The same code decides both.** `shape_full_netmap`
  (`crates/modules/network/src/overlay.rs:2810`) was extracted from the overlay join and is
  what the join itself now calls, so the list can never show a peer the netmap withholds:
  ACL `off` / `warn` list every live node, `enforce` only the shaped set.
- ⚠️ **The device's node is found before any ACL read.** Loading the ACL can create the
  tenant's overlay network if it is missing; a GET must never allocate one.
- **What never leaves the server:** machine ids, owner ids, WireGuard keys and key epochs,
  consent settings, codecs. Those fields are blanked *before* search runs, so `q` cannot be
  used to probe them. Search, sort and paging are the web grid's own
  (`parse_query` / `apply_query`, `crates/api/src/routes/device.rs:295`, `:337`), so the two
  grids cannot drift.
- Proven by a negative control: with visibility taken from every live node instead of the
  shaped netmap, the enforce test leaks a withheld peer and fails
  (`crates/tests/src/agent_self_tests.rs`).

### The companion's side

```mermaid
sequenceDiagram
    participant G as Devices page (grid.js, mesh.js)
    participant L as LocalAPI
    participant V as roomlerd self_view
    participant S as /api/agent/self/*
    G->>L: Devices {org, page, per_page, q, sort, dir}
    L->>V: cache hit (≤ 10 s)? answer it
    V->>S: GET with THAT org's agent token (10 s timeout)
    S-->>V: VisibleDevicesPage
    V-->>G: DevicesPage — or Upstream {code, message}
    Note over G: live connection / RTT cells come from the local<br/>Peers poll (2 s), joined on the overlay node id
```

- **The daemon proxies, per org.** `agents/roomlerd/src/self_view.rs` keeps each org's server
  URL and agent token (the token is redacted from `Debug` and never logged) and asks with
  the right one. Answers are cached — 10 s for a page, 20 s for the mesh, 32 entries, one
  request in flight per key — so the companion, the CLI and re-renders share a fetch.
- **A failure has a name, never an empty list.** `Response::Upstream { code }`:
  `server_unreachable` (connect, TLS, timeout), `unauthorized` (401), `unsupported_server`
  (404 — a server older than the route), `module_unmounted` (the server's own 503),
  `rate_limited` (429), `server_error`, `unknown_org`, `org_disabled`.
- **The page.** `front/grid.js` is the web grid's model ported to plain JS: server sort keys,
  debounced search, pager, and a column chooser (show / hide, drag, ▲/▼) remembered per org
  in `localStorage`. `front/mesh.js` draws the web dashboard's ring graph in plain SVG; its
  pure helpers `front/mesh-util.js` are **generated** from `ui/src/utils/mesh.ts`, and a
  Vitest check fails when the copy is stale. The list refreshes every 30 s only while the
  view is visible (60 → 120 s back-off after an error, the last page kept on screen).
- **Older sides still work.** A daemon that predates the verbs, or a server without the
  routes, gets the pre-FR-84 peers table with a one-line note to update. `overlay:
  "no_node"` shows this device and the "private network is off" hint; ACL `enforce` with no
  visible peers says so.
- **From a terminal:** `roomler devices [--org L] [-q TEXT] [--sort KEY] [--desc]
  [--page N] [--per-page N] [--json]`.

### A two-half Mac asks its root half (FR-97 P2g)

A Mac with both halves (FR-43) is **two devices**, and only the root half is on the private
network: only root can hold a utun. The companion talks to the per-user half, whose own answer
is `no_node`, so the page listed the Mac and nothing else. The per-user half now asks the root
half for the machine's view, over a person socket the root half serves the console user:

```mermaid
sequenceDiagram
    participant G as Devices page
    participant U as per-user half (the console user)
    participant S as /api/agent/self/*
    participant R as root half (/var/run/roomler-person/<uid>.sock)
    G->>U: Devices / Mesh
    U->>S: with ITS token
    S-->>U: itself only · overlay = no_node
    U->>R: the same request (5 s)
    R->>S: with the root half's token
    S-->>R: the netmap's devices · overlay = ok
    R-->>U: DevicesPage
    U-->>G: the root half's answer, when it is `ok`, its own otherwise
```

- **Only a `no_node` answer off root asks** (`person_socket::machine_view_wanted`), and only an
  `ok` answer replaces it (`machine_view_usable`). A root half that is missing, older, switched
  off (`ROOMLERD_PERSON_SOCKET=0`) or itself off the network leaves the per-user half's own
  answer on screen, as before. So does a socket served by anyone but root
  (`localapi::connect_unix_served_by_root`).
- **"This device" is the root half's row.** The per-user half's own row has no node, so the
  machine's netmap does not carry it, and it is not in the list.
- **The live view comes from the root half too (FR-97 P2h).** The Connection column and the
  Overview page read `Peers` and `Status`, which the half answered from its own overlay. It
  runs none, so before P2h a Mac showed:
  - `—` in the Connection column, with the dot falling back to the server's presence;
  - no online count and "(no overlay IP)" on Overview, with no MagicDNS or exit line.

  Both requests now pass through one async hook (`LocalApiState::status_view` /
  `peers_view`, which default to the sync reads). When the half holds no overlay address of
  its own (`person_socket::network_view_wanted`), its hook answers:
  - `Peers` with the root half's peers, as the person socket serves them: the link (online,
    direct or relayed and over what, round trip) without any endpoint;
  - `Status` with its own name, version and server link, plus the root half's overlay
    addresses, exit status and MagicDNS (`person_socket::with_machine_network`) — and, since
    #2892, **`network_from`** (`crates/localapi/src/lib.rs:197`): the root half's node id and
    name, set only when those fields were lent. Nothing on the wire said so before, and `roomler status` as the console user
    printed the root half's address as this half's, right above this half's own "overlay OFF"
    enrollments. The CLI now marks `overlay ip` / `overlay ip6` "(the root daemon's)", adds a
    `network` line naming that daemon (`sudo roomler status` asks it), and its "not on the
    mesh" line stops prescribing netstack to a half that is off the mesh by design (FR-43);
    Overview's address reads "· the root daemon's (<name>)". The field is additive and absent
    unless lent, so every other host's JSON and output are byte-identical.

  A root half that is slow to answer (1.5 s) leaves the half's own answer on screen for that
  poll.
- **What the per-user half does NOT borrow: configuration (#2871).** The per-user half
  never forwards a config verb to the root half. Since design A the COMPANION asks the root
  half's person socket itself for the machine-side rows and sends their changes there
  (§5), so the person always knows which daemon answered; the per-user half's own
  `ConfigGet` / `ConfigSet` stay its own file's. The same socket tells the companion, and
  the CLI's `config ls` / `config set` note (`localapi::other_daemon_socket`,
  `crates/localapi/src/lib.rs:5513`), that a root half exists: `/var/run/roomler` is
  `0700`, so the console user's `stat` of the root socket fails and that note had been
  silent for them.
- The root half's person socket, its directory and the uid check: §12 and the
  [security baseline](security-baseline.md) §4.

## 7. Apply now — restarting the service from the companion

Most settings are read once at startup, so saving one is not the same as applying it. The
Settings page collects the saved restart-required keys in a bar with an **Apply now** button;
`roomler restart` is the same thing from a terminal. Both send `Request::RestartDaemon`
(`crates/localapi/src/lib.rs:1754`), and **the daemon decides**.

It restarts only when it can prove that something will start it again — exiting an
unsupervised `roomlerd run` would take the device offline for good:

| How the daemon runs | How it knows (`agents/roomlerd/src/supervision.rs:193`) | Leaves with | Who starts it again |
|---|---|---|---|
| Windows service (SCM host) | the host passes `run --supervisor scm` | `0` — the host respawns 0 at once | the service host |
| Windows Scheduled Task (per-user) | the task passes `run --supervisor task` | `9` | **the caller**: the task's `IgnoreNew` policy drops a start while the old process is still exiting, so the companion / CLI runs `roomlerd service start` every 3 s until a new process answers |
| systemd | `INVOCATION_ID` **and** `/proc/self/cgroup` puts the process inside one of our units, **and** `systemctl show` says this process is the unit's main PID and its restart policy restarts exit `9` | `9` | systemd |
| launchd | `XPC_SERVICE_NAME` starts with `com.roomler.` | `9` — `KeepAlive{SuccessfulExit:false}` relaunches only a non-zero exit | launchd |
| the macOS supervisor (FR-43) | the worker's `--supervised` flag | `9` | the supervisor |
| anything else | — | **refused** | — |

`9` is `RESTART_REQUESTED_EXIT_CODE` (`agents/roomlerd/src/watchdog.rs:210`); it is outside
systemd's `RestartPreventExitStatus=7 8`, and the SCM host maps it to *respawn* as well.

```mermaid
sequenceDiagram
    participant C as companion / roomler restart
    participant D as roomlerd (old process)
    participant S as supervisor
    participant N as roomlerd (new process)
    C->>D: RestartDaemon {reason}
    D->>D: decide: supervised? enabled? recording? < 30 s since the last?
    D-->>C: DaemonRestarting {supervisor, restart_by, exit_code, pid, started_at_ms, restart_within_s}
    Note over D: shutdown starts only after the answer is written
    D->>D: the auto-updater's graceful path (clean shutdown recorded, exit routes purged, virtual desktop torn down)
    D->>S: exit(code)
    S->>N: start
    C->>N: Status until a DIFFERENT pid + start time answers
```

- **It is not a crash.** The restart leaves through the same internal shutdown an auto-update
  uses, so the clean shutdown is recorded and the crash / rollback accounting never sees it
  (`exit_for_requested_restart`, `agents/roomlerd/src/app.rs:2261`).
- ⚠️ **A virtual-desktop host reaps its own desktop before it exits (#1684).** On Linux the
  daemon may run a virtual desktop (Xvfb + WM + apps, `ROOMLERD_VIRTUAL_DESKTOP=1`), and under
  systemd `KillMode=control-group` whatever it spawned that outlives it — the desktop's
  setsid'd `at-spi-bus-launcher` grandchild, which **ignores SIGTERM** — keeps the unit's
  cgroup non-empty until systemd SIGKILLs it after `TimeoutStopSec` (90 s), then pays
  `RestartSec`: a `roomler restart` took **96 s**. So every graceful exit now tears the whole
  desktop tree down (`virtual_desktop::teardown`, `agents/roomlerd/src/virtual_desktop.rs`) —
  the tree is spawned into its own process group, and teardown unions the group members with
  the parent-link closure of the daemon's direct children (which reaches a setsid'd
  grandchild, since `setsid` changes the session and group but never the parent), SIGTERMs,
  waits a 2 s grace, then SIGKILLs the remainder. A parent link reaches such a grandchild
  only while its parent lives, and an app that exits, or an X server whose death ends its
  clients, takes the link with it. So since #3665 the window manager and the apps are the
  children of **the desktop's keeper** (`roomlerd vd-keeper`), a subreaper in the desktop's
  group that adopts what they leave behind and ends the whole desktop when the daemon's end
  of its input closes, which a daemon that dies closes too
  ([`linux-capture.md` §2.3](linux-capture.md#23-the-desktops-keeper-3665)). systemd then
  finds an empty cgroup and relaunches after `RestartSec` (~5 s). ⚠️ It SIGKILLs **as root**, so it never signals a
  recycled pid: the daemon's own children are reaped only **after the last signal** (an
  unreaped zombie keeps its pid — and, for Xvfb, the desktop's pgid — reserved), and any
  other pid is signalled only after its `/proc` **starttime** is re-read and matches the
  snapshot that selected it (`same_process`). Both rules are unit-tested against a scripted
  process table (`kill_tree_signals_everything_before_it_reaps_anything`,
  `kill_tree_never_signals_a_recycled_pid`).
- **The wait follows the supervisor.** A caller waits for the relaunched daemon at least as
  long as the supervisor says a stop + relaunch can take — under systemd,
  `TimeoutStopSec + RestartSec`, read from `systemctl show` and carried as the additive
  `restart_within_s` in `DaemonRestarting` — plus a margin, never below the 60 s floor and
  never past 10 minutes (`localapi::restart_wait`, `RESTART_WAIT_CAP`: a unit with
  `TimeoutStopSec=1h` must not make `roomler restart` hang for an hour). Before this, a fixed
  60 s wait was shorter than the supervisor's
  worst case, so the CLI reported failure on a restart that had in fact worked. `restart_within_s`
  is absent for every non-systemd supervisor and from an older daemon, where the wait stays 60 s.
- **Refusals are named and shown verbatim:** no supervisor it can prove,
  `local_restart_enabled = false` (read on every request, so turning it off applies to the
  very next one), a screen recording in progress (FR-85), a previous restart less than 30 s
  ago (`RESTART_MIN_INTERVAL`, persisted next to the config, so it also bounds a loop across
  processes), and on the macOS supervisor a worker younger than 30 s.
- **"Back" means a new process.** The companion waits for a different pid **and** start time
  (`cmd_restart_wait`, `src/commands.rs:1522`) — the leaving process keeps answering for a
  moment, and Windows can hand the relaunched daemon the old pid.
- ⚠️ **This is local only.** Remote configuration never restarts a daemon: a server push is
  persisted and reported as *applied-pending-restart*, and there is no server message that
  restarts anything (`docs/remote-config.md` §7b).

### 7.1 The macOS permission card's Restart (#3164)

macOS applies a Screen Recording grant to a freshly started process only, so a Mac's card offers
**Restart roomlerd** beside its Screen Recording row: the same flow as *Apply now*
(`window.Roomler.restartDaemon`, the LocalAPI `RestartDaemon` verb, the agent's own supervisor
relaunches it). Accessibility needs none: the agent reads it live and opens its input backend
again once it is granted.

The card shows the **agent's** grants, as the agent reports them in its status
(`NodeStatus.macos_grants`, read from its own process). Before #3164 it asked TCC about the
menu-bar app's own process, `ai.roomler.agent-tray`, which never holds either grant: the card
kept asking for both after the person had granted them to `roomlerd`, and its button registered
the menu-bar app itself in the pane, as the "Roomler" entry that changes nothing. The buttons now
only open the pane. An agent that does not report its grants (older than #3164, not running, or a
root half with no GUI session) shows no card.

macOS's own **Quit & Reopen** restarts the agent too (`agents/roomlerd/src/macos_quit.rs`): a
Quit AppleEvent from anyone but `loginwindow` ends it with the supervisor's restart code instead
of AppKit's exit 0. Up to 0.4.189 that button left the agent offline (`docs/installation.md`,
the macOS grants).

## 8. What this device can encode

The Overview's **Hardware video encoding** card is a codec × backend matrix — H.264, HEVC,
AV1 and VP9 against NVENC, QSV, AMF, VideoToolbox, VAAPI, D3D12, Vulkan, Media Foundation and
the software encoders — with the chroma each cell produced (4:2:0 / 4:4:4), denied cells
struck through, and how long the probe took.

- It asks `Request::EncoderCaps`, which answers from the probe's **cached** result only
  (`caps::cached()`, `agents/roomlerd/src/encode/caps.rs:194`). The capability probe runs in
  child processes (vendor driver code) at the first server hello; a control-surface poll
  must never be what launches it — locked by `not_probed_never_triggers_a_probe`.
- Three states: `ready` (the matrix), `not_probed` (the daemon has not connected yet — the
  card polls every 5 s while the Overview is visible, then stops), `unsupported` (a build
  with no video encoder). An older service gets a one-line "update the service" instead.
- **A Linux system install** (#3831). The companion runs as the person and reaches the root
  daemon over its person socket (§12), which serves `EncoderCaps` as well (`person_may`,
  `crates/localapi/src/lib.rs:3709`): the answer is the cached summary
  (`caps::summarize_cached`, `agents/roomlerd/src/encode/caps.rs:298`), so the verb starts no
  probe and changes nothing on the host. Up to 0.4.222 the socket refused it, and the card read
  **Error**. On such a daemon a newer companion reads **Unavailable**: "The encoders are shown
  to this device's administrator." (`root_only`). Only an *answer* that refuses reads that way,
  and only from a root daemon that this unprivileged app reached, which one `Status` on the
  same connection confirms (`refused_by_root_daemon`,
  `agents/roomler-desktop/src/commands.rs:1143`, the test the config verbs use, #2891; the
  decision is `encoder_caps_view`, `:1349`). No answer at all (a failed write or read, a closed
  connection) stays **Error**, with its stage. Windows serves the verb to every interactive
  user (the pipe's `IU` ace), and a Mac's companion asks its per-user half, which answers from
  its own probe.
- `denied` is the effective `encoder_cells_deny` list (env, config or built-in), shown
  because a denied cell is never opened by the probe or a session, so it could not appear in
  the matrix otherwise. The resolved `encoder_preference` is shown under it.
- It is the same data `roomlerd caps` prints and the server's `AgentCaps` carries; see
  [encoders.md](encoders.md) for what a cell means and why a probe proves an open, not a
  session.

## 9. Where files dropped from a remote viewer land

The Overview's **Incoming files** card shows where a file a controller drops onto this
screen will land **right now**, with **Open folder**, **Change…** (the native folder picker,
which can create a folder) and **Use default**. The setting is `files_dir`; unset, the ladder
is the active user's Downloads, then the service's own fallback (a Windows background service
with nobody signed in refuses the file instead, §9.2; on Linux the service's own folder is never
one another account can write, §9.5; a Mac's system daemon transfers no file itself, and a
session no GUI worker serves is told why, §9.6). It is **live**: every
transfer reads the current value, so a change needs no restart.

⚠️ `files_dir` is a security control, because the daemon writing the file can be SYSTEM or
root and the LocalAPI admits any interactive user. The rules
(`crates/agent-core/src/files_dir.rs`):

| Rule | Why |
|---|---|
| Absolute, or `~\…` — expanded against the **active** user's profile when a file arrives | one machine-wide setting stays right for whoever is signed in |
| No device / kernel-namespace paths (`\\?\GLOBALROOT`, `\\.\…`), no admin or device shares, no `..` | none of those is a folder a person meant |
| No name Windows silently rewrites (a trailing dot or space, `:stream`) | `C:\Windows.\Temp` would otherwise pass the system-folder check |
| Not under the system folders (`%WINDIR%`, `%ProgramFiles%`, `/etc`, `/usr`, `/bin`, `/System`, …) | remote-supplied files must never land where the OS executes from |
| **When the writer is SYSTEM or root, inside the active user's profile — and refused when no profile is known** | otherwise a non-admin user could make SYSTEM write remote-supplied files anywhere |
| Links resolved and re-checked; an unresolvable link is refused for a privileged writer | a junction must not carry the path outside the rules |
| Creatable and writable | a folder that cannot take the file is a failed drop later |

- Checked **when set** (the daemon refuses the `ConfigSet` with the reason, which the card
  shows verbatim) **and when used** (`download_dir()` re-validates per transfer and falls back
  to the default ladder with a warning — the check that holds even if `config.toml` was
  edited by hand).
- Only the **console user** may change it through the LocalAPI, on Windows the person at the
  screen the daemon shows (§12.4): another signed-in account on the same machine must not
  choose where that person's incoming files land. A SYSTEM shell
  (`roomler exec`, SSH) can still edit the config file; the use-time check applies.
- The Status the companion shows is refreshed off the request path, so a Downloads folder
  redirected to an offline share cannot stall the 2 s poll. On a Linux system install the refresh
  also waits 1.5 s at most for who is at the screen, and shows the folder as pending until a
  lookup answers (#3796, §9.4).

### 9.1 A Linux system install writes the file as the person at the screen (FR-97 P3d)

A root daemon serves the session of whoever is at seat0, and "the active user's Downloads"
used to mean root's own ladder: with no XDG Downloads for root, the file landed in **`/tmp`,
owned `root:root`** (measured on 0.4.143, FR-97 §8). It now lands in the person's Downloads
and is **born theirs**:

```mermaid
flowchart TD
    B["files:begin"] --> Q{"root on Linux, and<br/>ROOMLERD_FILES_AS_SEAT_USER on?"}
    Q -- "no" --> D["FsAs::Daemon<br/>files_dir, or the ladder"]
    Q -- "yes" --> W{"who is at seat0?<br/>asked off the async worker, 1.5 s at most"}
    W -- "root" --> D
    W -- "nobody, or the lookup failed" --> A["FsAs::DaemonAlone<br/>root's own Downloads, 0600 (§9.4)"]
    W -- "no answer in time" --> C["FsAs::CannotTell: refused,<br/>saying it could not tell (§9.4)"]
    W -- "a person" --> P["FsAs::Person<br/>(companion::seat0_account, uid 0 refused)"]
    P --> F["their files_dir (inside THEIR home),<br/>else XDG_DOWNLOAD_DIR, else ~/Downloads, else ~"]
    F --> S["every path step on a blocking thread<br/>under their fsuid, fsgid and groups"]
    S --> R["staging, data file, final rename,<br/>resume, cancel, orphan sweep"]
```

| What | Where (`agents/roomlerd/src/files.rs`) | Why |
|---|---|---|
| Who writes | `FsAs` (`:282-809`), chosen once in `begin` (`FsAs::for_write`, `:499`, off the async worker since #3711, §9.4) and carried by the transfer and by `PARTIAL_REGISTRY`'s `PartialEntry` (`:1496-1499`) | the end-time rename, a resume and a cancel act as whoever staged the partial, even after a user switch |
| How | `FsAs::run` → `crate::fs_identity::FsIdentity` (FR-85's guard, moved out of `recording`) | Linux checks every open, create and unlink against the thread's fsuid, fsgid and groups; root's bypass capabilities leave with the fsuid |
| Their folder | `person_downloads` / `xdg_download_dir` (`:1088-1121`) | `user-dirs.dirs` is the person's to write, so only a folder inside their home is taken |
| The staging | `stage_held` (`files.rs:5080`) → `staging::stage` (`agents/roomlerd/src/files/staging.rs:746`): `mkdirat` for its folders, `openat` with `O_CREAT`, `O_EXCL` and `O_NOFOLLOW` for its files | an upload's own folder (`<id>`) is never reused, and a link or a folder planted where it goes refuses the begin; the stager's own `.roomler-partial` is reused once it is made 0700, and a folder that keeps no modes stages privately (§9.5) |

⚠️ **Never as root followed by a `chown`.** In a folder the person owns, a link they planted
would be followed with root's rights, and the staging folder's `meta.json`, which a resume reads
and whose `dest_dir` the final rename trusts, is theirs to edit. Under their identity, a tampered
`meta.json` reaches nothing they could not write themselves. A root-only test
(`seat_user_tests::as_root`, run under `sudo` in CI) plants a link into a `0700 root` folder and
shows the begin refused, with a control proving root's own rights go straight through it.

⚠️ "The person" is seat0's (`companion::seat0_account`, FR-97 P5a), never whoever asked: an xrdp
session sits on no seat. With nobody at seat0, the daemon's own ladder applies, but never a folder
a controller chose, nor the configured one, and that ladder ends in root's own Downloads (#3688,
§9.5). Since #3688 downloads and browsing (the host → viewer
direction) take the same person (§9.4).

### 9.2 A Windows background-service install writes the file as the person at the screen (#3048)

The background service's worker (SystemContext) is LocalSystem in the session it serves. It used to
write a received file as LocalSystem: into a Downloads built from the person's user name, or, when
that Downloads was out of its reach (redirected to a share), into the machine's own `uploads` folder.
Either way the file was not the person's. It is now written the way §9.1 writes it on Linux: every
path step on the person's side runs as the person signed in to that session, so the file is born
theirs.

```mermaid
flowchart TD
    B["files:begin"] --> L{"LocalSystem worker?"}
    L -- "no, or ROOMLERD_FILES_AS_SEAT_USER=0" --> D["FsAs::Daemon<br/>files_dir, or the ladder"]
    L -- "yes" --> S{"someone signed in<br/>to this session?"}
    S -- "no" --> N["FsAs::Nobody: refused,<br/>'Nobody is signed in to this computer<br/>to receive the file'"]
    S -- "yes" --> P["FsAs::Person<br/>SID, profile, Downloads from their token"]
    P --> F["their files_dir (inside THEIR profile),<br/>else their Downloads known folder, else their profile"]
    F --> T["staging in the service's own folder,<br/>as the service"]
    T --> E["files:end: the data opened as the service;<br/>as the person, a new file made and filled;<br/>the staging removed as the service"]
```

| What | Where | Why |
|---|---|---|
| Who writes | `FsAs::Person` and `FsAs::Nobody` (`agents/roomlerd/src/files.rs:282-332`), chosen by `fs_as_for` (`:418`) when the process is LocalSystem | the person is read from their token (`win_identity::session_person`, `agents/roomlerd/src/win_identity.rs:308`), never from a name, which can name another account's folder |
| How | `win_identity::as_person` (`agents/roomlerd/src/win_identity.rs:381`), on a blocking thread | it impersonates the session user's token after checking its SID is the person's; no token, another SID or a refused impersonation is an error, never a step as LocalSystem; a failed revert aborts |
| Their folder | `person_downloads_windows` (`agents/roomlerd/src/files.rs:817`) | the Downloads known folder for their token, read with `KF_FLAG_DONT_VERIFY`, local or redirected to a share: the write is theirs, made with their logon (the share is a lab read, #3048). None, or not a folder: their profile. The status card takes it as it is and probes nothing. The choice is a unit test on every OS (`person_downloads_windows_is_the_known_folder_else_the_profile`, `:8337`), and the worker's whole decision one in the Windows lane (`a_system_context_worker_takes_the_person_downloads_never_the_machine_folder`, `:10894`): never the machine-global folder |
| The staging | `stager_for` (`:2377`) | the service's own `%PROGRAMDATA%` staging, as before (rc.22); the person needs no right to it |
| The end | `place_across` (`:2931`) | no rename crosses from the service's folder into theirs, which would carry the staging file's permissions along: the data is copied into a new file the person creates, with their folder's permissions. A failed copy removes its half file and keeps the staging for a resume |
| A resume | `resume_incoming_as` (`:3182`), `PartialMeta::person_sid` (`:1541`) | the file is placed only for the person the transfer began for; for anyone else it is refused. A partial from a daemon before #3048 goes to whoever is signed in |

- **A locked screen keeps its user**, so a file sent then lands in their Downloads.
- **Nobody signed in** (before sign-in, after sign-out): the upload is refused with the reason
  above, which the viewer shows. File transfer never writes as LocalSystem.
- **The viewer says where the file landed**, by path, local or on a share: "Uploaded report.pdf
  to `\\server\share\…\Downloads`" ([`remote-control.md`](remote-control.md) §18.2).
- `files_dir` and a folder the viewer chose keep FR-84 D4's rule (inside the person's profile), and
  are checked and written as the person.
- `ROOMLERD_FILES_AS_SEAT_USER=0` keeps the service's own identity, as on Linux.
- Per-user and attended installs are unchanged: their daemon already is the person.

### 9.3 A Windows background-service install reads as the person at the screen too (#3669)

The other direction, host → viewer, is three verbs: a download (`files:get`), a folder download
(`files:get-folder`, one zip) and the drawer's listing (`files:dir`). On a background-service
install they used to run as the worker, LocalSystem. They now run as the person a write takes
(§9.2), chosen by the same function, through the same impersonation:

```mermaid
flowchart TD
    R["files:get · files:get-folder · files:dir"] --> L{"LocalSystem worker?"}
    L -- "no, or ROOMLERD_FILES_AS_SEAT_USER=0" --> D["FsAs::Daemon, as before"]
    L -- "yes" --> S{"someone signed in<br/>to this session?"}
    S -- "no" --> N["FsAs::Nobody: refused<br/>before anything is touched"]
    S -- "yes" --> P["FsAs::Person: each step on a<br/>blocking thread, as them"]
    P --> F["a file: resolved, checked<br/>and opened as them; the<br/>pump reads that handle"]
    P --> Z["a folder: each folder listed<br/>and each file opened as them"]
    P --> G["a listing: resolved, opened<br/>and read as them"]
```

| What | Where (`agents/roomlerd/src/files.rs`) | Why |
|---|---|---|
| Who reads | `FsAs::for_read` (`:478`), one function with `FsAs::now` (`session_or_daemon`, `:538`) | a read and a write cannot take different people. The person is fixed when the request arrives, and every step asks for the session user's token again and checks its SID (`win_identity::as_person`) |
| A download | `begin_outgoing_as` (`:3542`), `open_to_send` (`:3660`) | one step resolves the path, refuses the agent's own secrets, states, caps and opens the file. Windows checks access when a handle is opened, and a handle on a share keeps the logon it was opened with, so the pump reads through it and needs no identity |
| A folder | `begin_outgoing_zip_as` (`:4252`), `walk_and_zip` (`:4333`), `zip_folder_entries` (`:4527`) | the root is resolved and opened for listing as them, so a folder they may not list is refused at the start, never sent as an empty zip. Each folder is listed, and each file opened, in its own step as them; what they may not open is left out, as an unreadable file always was. A step whose identity fails (they signed out) ends the walk with an error, never a short zip that looks complete |
| A listing | `list_dir_as` (`:4026`), `list_canonical` (`:4044`) | resolved and opened as them inside the 3 s timeout, then the entries read through that handle |

- **Nobody signed in**: a download or a folder download is refused with "Nobody is signed in to
  this computer to send the file"; the drawer, its drive list included, with "Nobody is signed in
  to this computer to browse its files". The viewer shows either.
- **A file or folder Windows refuses to open for the person** (their own rights, or a filter
  such as an EDR or DLP product, which answers with the same code) is refused, as a download or a
  listing, with "`<path>`: Windows refused to open it for the person signed in to this computer".
  Inside a folder download, an entry they may not open is left out of the zip, not refused, as an
  unreadable file always was; only a folder they may not list at all is refused.
- **A locked screen keeps its user**, so reads run as them, as writes do.
- **Another person signs in** (switch user): the console moves to their session and the service
  replaces the worker with one there, so a read takes them from then on, as a write does. A
  transfer under way ends with the old worker, and every step checks the SID it began with.
- **A Downloads folder redirected to a share** is listed and downloaded with the person's own
  logon, which the folder grants, where the computer's own account is refused. Like the write in
  §9.2, the share is a lab read, not a unit test. Its path is the one the upload message names.
- **The service's own folders** (the drawer's **Upload staging folder** button, `<staging>`,
  and the service's logs and crash dumps) are SYSTEM's and the Administrators' alone once
  sealed ([installation.md](installation.md)). A standard user may not list them, nor may an
  administrator while UAC is on (the session holds an administrator's filtered token), so the
  drawer shows the refusal. An administrator fetches those files with `roomler exec`.
- **The denylists stay**, as a second line: the registry-hive and kernel-namespace refusals of
  `validate_outgoing_path` and `is_agent_secret` run on the path the person resolved.
- `ROOMLERD_FILES_AS_SEAT_USER=0` keeps the service's own identity for reads too: one switch, both
  directions.
- **A worker that cannot read its own token** is taken for LocalSystem (`fs_as_for`,
  `agents/roomlerd/src/files.rs:418`; the token is read once, `win_identity::own_token_is_local_system`,
  `agents/roomlerd/src/win_identity.rs:62`): reads and writes then take the person, or nobody, never
  the worker itself.
- **The entry points are tested with the person**, not only their steps: a test stands the
  low-integrity person in for what `FsAs::for_read` answers (`files::stand_in`), and drives
  `files:get` (`begin_outgoing`), `files:dir` (`list_dir`) and `files:get-folder` through the task
  the viewer's request runs (`peer::outgoing_zip`). A person who signs out partway through a folder
  ends it with `files:error`, never `files:eof`.
- Per-user and attended installs and macOS are unchanged; a Linux system install takes the person at
  seat0 since #3688 (§9.4). Three small changes reach every
  install, each a unit test in `files::tests`: a folder that cannot be listed is refused when its
  download begins (`an_unlistable_folder_is_refused_when_its_download_begins`; it used to arrive
  as an empty zip), a download opens its file before its offer and checks the handle it sends, so
  one that will not open is refused at once
  (`a_file_that_will_not_open_is_refused_before_its_offer`), and a listing resolves its path off
  the async worker, inside its 3 s timeout (`a_listing_resolves_its_path_off_the_async_worker`).

### 9.4 A Linux system install reads as the person at the screen too (#3688)

On a Linux system install the daemon is root. A download (`files:get`), a folder download
(`files:get-folder`) and the drawer's listing (`files:dir`) used to run as root, while a received
file was already written as the person at seat0 (§9.1). They now take that same person, by the
same rule, and each read step also gives up the thread's capabilities:

```mermaid
flowchart TD
    R["files:get · files:get-folder · files:dir"] --> Q{"root on Linux, and<br/>ROOMLERD_FILES_AS_SEAT_USER on?"}
    Q -- "no" --> D["FsAs::Daemon, as before"]
    Q -- "yes" --> S{"who is at seat0?<br/>asked off the async worker, one lookup<br/>in flight, 1.5 s at most"}
    S -- "root" --> D
    S -- "nobody: an empty seat, the login<br/>screen, a host with no logind" --> N["FsAs::Nobody: refused<br/>before anything is touched"]
    S -- "no answer in time,<br/>or the lookup failed" --> C["FsAs::CannotTell: refused,<br/>saying it could not tell"]
    S -- "a person" --> P["FsAs::Person: each step on a blocking thread<br/>with their fsuid, fsgid and groups,<br/>and no capability"]
    P --> H["every handle, a file's or a folder's,<br/>held to what was asked,<br/>off the kernel's own file systems"]
    H --> F["a file: the pump reads that handle"]
    H --> Z["a folder download: before each folder and<br/>each file, still their uid at seat0? Each folder<br/>listed through its handle, each file opened as them"]
    H --> G["a listing: resolved as them,<br/>listed through its handle"]
```

| What | Where (`agents/roomlerd/src/files.rs`) | Why |
|---|---|---|
| Who reads | `FsAs::for_read` (`:478`) → `linux_fs_as` (`:949`), the function `FsAs::now` (`:447`) asks for a write | a read and a write cannot take different people. The person is seat0's (`seat_now`, `:856`, through `companion::seat0_account`, `agents/roomlerd/src/companion.rs:4523`, FR-97 P5a's rule), fixed when the request arrives. A name that resolves to another uid than the one logind recorded for the session is nobody (`seat_of`, `files.rs:872`): that account's rights are not the person's |
| The lookup | `seat_within` (`:1059`), `SEAT_LOOKUP_WAIT` (`:1028`), `FsAs::CannotTell` (`:331`), `linux_fs_as_within` (`:1001`), one lookup in flight `SEAT0` (`:1037`, `OneInFlight`, `agents/roomlerd/src/one_in_flight.rs:72`) | logind and the account database are other processes: the lookup runs on a thread of its own, never a stalled async worker, and each caller waits for it 1.5 s. #3796 — one lookup at a time, on a thread named `seat0-lookup`: a caller who asks while one runs waits on it, within its own 1.5 s, and starts none of its own, so retries during a hang hold one thread, not one each. A caller takes only the answer of a lookup that began at or after it asked (the list below says why). A listing then waits for its own 3 s timeout, and the viewer's drawer gives up on `files:dir` after 5 s, so the two leave room inside it; `files:get` and `files:get-folder` have no timer of their own in the viewer. Only seat0's own answer is nobody (`companion::is_nobody_at_the_screen`, `agents/roomlerd/src/companion.rs:4644`), and so is a host with no logind at all, which has no seat: `loginctl` is not installed (`loginctl_did_not_start`, `companion.rs:4559`), or systemd did not boot the host (`loginctl_failed`, `:4575`). Any other lookup that fails cannot tell who is there (`Seat::CannotTell`), and one that runs out of time does not know yet (`Seat::OutOfTime`, `files.rs:850`): the read is refused with its own text either way, logged |
| A received file's lookup (#3711) | `FsAs::for_write` (`:499`), from `FilesHandler::begin` (`:2421`), `FilesHandler::resume_incoming` (`:3188`) and `sweep_orphans` (`:1760`) | the same path as a read's, with the same wait. These three asked inline on the async worker, with no time limit, so a slow account database (SSSD or LDAP refreshing a large group list) stalled that worker for as long as the lookup took, and the startup sweep, which signalling waits for, held the daemon's start as long. What a received file takes with each answer is in the list below |
| A files folder's set (#3769) | `FilesDirSet` (`files.rs:1371`): `check` (`:1385`), `saved` (`:1415`), `refresh_status` (`:1425`), from `config_set` (`agents/roomlerd/src/localapi_state.rs:1435`) | a `ConfigSet` of `files_dir` asks who a drop would be written as once, as a received file asks it, and runs §9's set-time check for and as that person before it takes the config write lock (`:1455`, the lock `:1462`). Under the lock are only the load, the change, the save and the live folder. The status answer is recomputed for the same person after the lock (`:1536`), and published only while what the set saved is still the configured value (`publish_effective`, `files.rs:1270`), so an earlier set's late answer never shows after a later set. Both checks used to ask under the lock with no time limit, so a slow logind or account database held every other config read and write behind it. What a set answers is in the list below |
| The person socket's check (#3796) | `person_socket::at_the_seat_now` (`agents/roomlerd/src/person_socket.rs:450`), its own lookup in flight `PERSON_AT_SEAT0` (`:463`), from the dispatch (`crates/localapi/src/lib.rs:4131`) | before it applies a `ConfigSet`, the person-at-the-device socket asks again whether its caller is still the person at seat0 (#2871's review). That is logind's answer, so it is asked as a received file's is: one lookup in flight (`seat0-person`), 1.5 s at most. It used to wait as long as logind took, a blocking thread for each request. With no answer in time the set is refused with "Could not tell who is signed in at this computer's screen. Try again." (`AtTheSeat::NotKnownYet`, `crates/localapi/src/lib.rs:3839`), the text a read or a received file gets then, never "the seat changed". #3845 — the socket's 5 s poll, which moves it to whoever is at the seat, now asks through the same lookup in flight, 5 s at most. A look that cannot tell keeps the socket where it is, and a consent answer is checked as a set is (§12.3) |
| The status answer (#3796) | `spawn_effective_refresh` (`files.rs:1311`), `refresh_effective_files_dir_as` (`:1252`), `FsAs::now` (`:447`), the read `effective_files_dir` (`:1221`) | the card's folder comes from one background refresh at a time, which asks who a drop would be written as. It asked seat0 inline, with no time limit: while a lookup hung the refresh never finished, and the card kept the folder it showed before, perhaps for a person who had left. It now asks through `SEAT0`, 1.5 s at most. With no answer in time it publishes the folder as pending (no folder, not fresh: #3769's shape), and the next status read starts another refresh, which waits on the same lookup. The status read itself answers from its cache and never waits for a lookup |
| How | `FsIdentity::begin_reading` (`agents/roomlerd/src/fs_identity.rs:233`), from `run_here` (`files.rs:739`) | §9.1's fsuid, fsgid and groups, and the thread's effective capabilities emptied for the step. An fsuid that is not 0 takes away only the file-system capabilities, and the rest would still answer for root wherever a check asks for one. The drop gives the capabilities back first (the groups' restore needs `CAP_SETGID`), then the identity, then the capabilities again, and checks all of it: a thread it cannot restore aborts the daemon |
| Exec and SSH as `console_user` (#3822) | `exec::console_account` (`agents/roomlerd/src/exec.rs:919`), `exec::console_person` (`agents/roomlerd/src/exec.rs:972`), through `ask_seat0` (`agents/roomlerd/src/files.rs:1046`) | Fleet RPC's exec, SSH's exec and shell, and SFTP find the person at the screen through the same lookup in flight, `SEAT0`, 1.5 s at most, awaited from their own task. They used to run `loginctl` inline on the async worker, with no time limit. Only `Seat::Person` is an account. No answer in time, or a lookup that failed, refuses with the try-again text; nobody and root at the screen refuse. Nothing else stands in for the person. Details: `docs/roomler-ssh.md` § `console_user` on Linux and macOS |
| The handles | `held_to_the_person` (`:4611`), `folder_held_to_the_person` (`:4623`), `folder_listing` (`:4503`), `KERNEL_FILE_SYSTEMS` (`:4585`) | inside the step, before a handle leaves it or anything is listed through it: never on one of the kernel's own file systems (procfs, sysfs, debugfs, tracefs, securityfs, efivarfs, bpf, pstore, configfs, cgroup, …: `fstatfs` on the handle, a file's or a folder's), and the kernel's own path for the handle must be the path asked for. A folder is opened first, checked so, and listed through that handle (`/proc/self/fd/N`), never through its path again. Some kernel files answer by who reads at `read()`, which the pump does as root, so the person's rights to them say nothing; and a link swapped in after the path was resolved is followed by the open, through a `/proc/<pid>/fd` link even onto a file or folder the daemon holds open |
| A folder download, the person leaves | `FsAs::still_at_the_screen` (`:688`), `seat0_active_uid` (`:904`), `walk_and_zip` (`:4347`) | before each folder is listed and before each file is opened, seat0's active uid (`ACTIVE_UID=` in `/run/systemd/seats/seat0`, logind's own record of the seat, as `sd-login` reads it) must still be the person's. Once it is not, or it does not read, the walk ends with `files:error`, never `files:eof` (§9.3's rule) |
| A received file, nobody at seat0 | `FsAs::DaemonAlone` (`:296`), `resolve_dest_path_as` (`:3796`), `FsAs::writer` (`:770`), `new_file` (`:1687`) | written as the daemon, as before, but neither a folder the controller chose nor the configured one (`files_dir`) is taken: the first is refused before it is touched, and the second is checked with no active home, so its `~` never resolves in root's. The upload lands in the default folder: FR-84 D4's rule for a privileged writer with nobody signed in (§9). Its partial and the partial's notes are created 0600 from the first byte, so the file is root's only (`root:root`, 0600), in root's own Downloads (§9.5). Nothing changes the mode after |

- **Nobody at seat0** (an empty seat, the login screen, a server or a cluster node with no session
  at all): a download or a folder download is refused with "Nobody is signed in at this computer's
  screen to send the file"; the drawer, its roots view included, with "Nobody is signed in at this
  computer's screen to browse its files". The viewer shows either. Uploads still work, into
  root's own `/root/Downloads` (§9.5), as files only root may read. The rule is the screen, not a
  sign-in: someone signed in over SSH is not at it.
- **A host with no logind at all** (no `loginctl`, or a host systemd did not boot: a container, WSL
  without systemd, a minimal VM) has no seat0, so nobody is at its screen: its reads get the texts
  above, never "Try again", and the switch below gives them back.
- **Who is at seat0 could not be told** (logind did not answer, on a host that has it; the account
  database did not answer; or the lookup took longer than 1.5 s): every read is refused
  with "Could not tell who is signed in at this computer's screen. Try again." A retry asks again.
  A lookup that ran out of time keeps going on its own thread, so the account database's own
  caches (SSSD's, nscd's) usually hold its answer by then. #3796 — that thread is the only one: a
  retry, a read or the status refresh that asks while it runs waits on it, within its own 1.5 s, and
  starts no lookup of its own. When it ends, its answer, late, is no one's, and the next caller asks
  afresh. A caller takes only the answer of a lookup that began at or after it asked: one that
  began earlier may answer for a moment that has passed. A stopped logind's D-Bus call fails after
  about 25 s, and a failed lookup leaves a received file to the daemon alone, so a caller who took
  that failure would get a write as root where its own lookup gives "Try again". An account lookup
  that stalls after logind has answered could name a person who has since left. So a caller waits
  such a lookup out, then asks a fresh one. The cost: with a slow lookup that still works, taking
  `d`, a caller who asks `e` after one began waits `(d - e) + d`. Up to `d` = 0.75 s every caller is
  answered. At `d` = 1 s a caller who asks within 0.5 s of a lookup's start gets "Try again" where
  its own lookup would have answered (`one_in_flight::tests::the_cost_a_caller_just_after_a_slow_lookup_began_waits_for_two`).
  A normal lookup takes tens of milliseconds. A received file takes one of two answers
  (#3711):
  - **No answer in 1.5 s**: the upload is refused with the same text before anything is touched,
    and the viewer shows it on the upload's row. The lookup is still running and a retry usually
    finds the person, so the file is not written for anyone else in the meantime. A resume whose
    partial the daemon still holds needs no answer and goes on; one it does not hold is refused, and
    the viewer starts that upload again. The startup sweep sweeps nothing that start.
  - **The lookup failed outright** (logind or the account database answered with an error): the
    file is the daemon's alone, as with nobody there, in root's own Downloads, 0600. Reads and
    writes differ here on purpose. A write has a safe place to go: no controller chose it, and only
    root reads what lands there, so a host whose lookup always fails (its logind masked, say) keeps
    its uploads. A read has no such place: read as root, it would send whatever root may read, so a
    read whose lookup failed is refused, as above.
- **Setting the files folder while who is at seat0 is not known** (#3769):
  - **No answer in 1.5 s**: a folder (the card's **Change…**, or `roomler config set files_dir`) is
    refused with "Could not tell who is signed in at this computer's screen. Try again." before
    anything is saved or made, and the card shows that text. **Use default** needs no one and goes
    through. Until a lookup answers, the status names no folder, so the card shows the default with
    its folder pending (`…`), never the folder just cleared. The next status read asks again, and
    the next file asks for itself.
  - **The lookup failed outright**: today's answers. The daemon writes alone with no one's profile,
    so the placement rule refuses a folder ("… no active user profile is known right now, so
    files_dir cannot be honoured"), and **Use default** shows root's own default folder (§9.5).
  - Either way nothing else waits for the lookup: a `ConfigGet`, a set of another key and the status
    answer at once.
  - Off Linux the answers are today's: a Windows service asks for the person signed in to its
    session (its token, with no time limit) on a blocking thread, now before the lock; a Mac's
    daemon asks for no one.
- **The status card while who is at seat0 is not known** (#3796): a status read (the companion's
  2 s poll, `roomler status`) answers at once, from its cache. Within 1.5 s of the refresh asking,
  the card shows the default with its folder pending (`…`), never the folder it showed before; once
  a lookup answers, the folder is back.
- **The person's own settings while who is at seat0 is not known** (#3796): a `ConfigSet` over the
  person-at-the-device socket (the companion's settings, or `roomler config set` as the person) is
  refused within 1.5 s with "Could not tell who is signed in at this computer's screen. Try
  again.", and nothing is saved. On a Mac the check is a look at `/dev/console`, which has no wait.
- **A person who leaves seat0 during a folder download** (signs out, or the login screen or another
  person takes seat0) ends it before its next file or folder with `files:error`, "the person this
  began for is no longer signed in", never `files:eof`, as on Windows (§9.3). The file being zipped
  when they left still goes.
- **A file or folder Linux refuses to open for the person** (their own rights, or an LSM such as
  AppArmor or SELinux, which answers with the same code) is refused, as a download or a listing,
  with "`<path>`: Linux refused to open it for the person signed in at this computer's screen".
  The switch covers file permissions only: SELinux and AppArmor still judge the daemon's own
  domain, so a refusal of theirs is the daemon's, which the text cannot tell from the person's.
  Inside a folder download, an entry they may not open is left out of the zip, as an unreadable
  file always was; only a folder they may not list at all is refused.
- **A kernel file system** is refused with "`<path>`: procfs is a kernel file system, which file
  transfer does not read for the person signed in at this computer's screen" (sysfs and the others
  alike), a handle that is not the file asked with "`<path>`: what opened is not the file asked
  for, so file transfer does not send it", and one that is not the folder asked with "`<path>`:
  what opened is not the folder asked for, so file transfer does not list it". Inside a folder
  download each is left out without a word: the zip ends `files:eof` without that entry.
- **A locked screen keeps its person**: GNOME and KDE lock inside the session, and seat0 still shows
  it (`companion::console_tests::a_locked_screen_keeps_its_person`). LightDM's light-locker switches
  seat0 to a greeter, which is nobody, for reads as for writes.
- **seat0's active session only**: someone signed in over SSH or xrdp, or on another seat, is never
  the one a read takes. After a user switch, a read takes whoever seat0 shows then, and nobody while
  the greeter is between them.
- **root itself at seat0** reads as the daemon: that person is root.
- **The denylists stay**, as a second line, on the path the person resolved: `validate_outgoing_path`
  and `is_agent_secret`. The agent's config is `0600 root`, so the person is refused it anyway.
- **A cost**: on a case-insensitive mount, a path asked in another case than the one it is listed in
  is refused, since the kernel names the handle as listed. Inside a folder download such an entry
  is left out without a word.
- **Writes with a person at seat0 are §9.1's, unchanged**: their steps keep the thread's
  capabilities, which the placement rule already holds to the person's own folders.
- Per-user installs (the daemon already is the person), macOS and Windows are unchanged.
- **Tested as root in CI**: `files::seat_user_tests::as_root` runs under `sudo` in the
  `rust-recorder` job with `ROOMLERD_TEST_REQUIRE_ROOT=1`, so a skip is a failure. It drives a
  download, a folder download and a listing through their entry points with the person stood in at
  seat0 (`files::stand_in::seat`), so `FsAs::for_read` itself decides who reads. It shows a read
  step holding no capability and leaving the thread as it found it (after a panic too), and
  refuses the kernel file systems to the person while the daemon reads them (the control). It
  refuses a folder swapped for a link after it resolved, whether the link leads to the daemon's own
  handle or to procfs, at the listing, a folder download's root and a folder of its walk
  (`a_folder_swapped_for_a_link_to_the_daemons_own_handle_is_not_listed`,
  `a_folder_swapped_for_a_link_to_a_kernel_file_system_is_not_listed`). It ends a folder download
  whose person leaves seat0 with `files:error`, at the next folder
  (`a_person_who_leaves_the_screen_mid_folder_ends_it_with_an_error_not_eof`) and at the next file
  (`a_person_who_leaves_the_screen_between_two_files_ends_it_at_the_next_file`), and receives a file
  with nobody at seat0 as root's, 0600, its partial included
  (`nobody_at_the_screen_receives_a_file_only_root_may_read`). It asks who a received file is written
  as at `files:begin`, `files:resume` and the startup sweep with a stood-in lookup that gives no
  answer in time, on a current-thread runtime, and sees each refused inside the wait, nothing
  touched, and the lookup on another thread than the runtime's own
  (`a_lookup_past_its_time_refuses_a_received_file_at_every_entry_point`); a lookup that answers in
  time still gives the file to the person (`a_lookup_in_time_still_receives_the_file_as_the_person`).
  It sets the files folder through a `ConfigSet` with a stood-in lookup that gives no answer in
  time, and sees the set refused inside the wait, nothing saved or made, and a `ConfigGet` asked
  meanwhile answered at once (`a_files_folder_set_whose_lookup_is_late_holds_no_config_read`); with
  a lookup in time, the folder saved and made as the person
  (`a_files_folder_set_whose_lookup_answers_in_time_is_checked_as_the_person`); with one that failed
  outright, the placement rule's refusal (`a_files_folder_set_whose_lookup_failed_keeps_todays_refusal`).
  #3796 — with a stood-in lookup held the way a stopped logind holds it, it asks three times in a
  row and four at once, and sees one lookup between them and each caller refused inside its wait
  (`retries_while_a_seat0_lookup_hangs_share_one_lookup`). It sends `ConfigSet`s over the person
  socket and sees each refused with the try-again text inside the wait, nothing saved and one
  lookup; after the release, the person's set is saved and someone else's refused in today's words
  (`the_person_sockets_seat_check_answers_within_its_wait_while_seat0_hangs`). It reads the status
  while the refresh's lookup hangs, and sees each read answered at once and the folder pending
  within the wait, never the folder from before (`a_status_read_shows_the_folder_pending_while_seat0_hangs`).
  It asks `FsAs::now()` and the set-time check from a thread of their own, and sees both told not
  known yet, never the daemon alone (`every_blocking_asker_is_told_not_known_yet_while_seat0_hangs`).
  How one lookup in flight is shared is a unit test on every OS (`one_in_flight::tests`: one lookup
  however many ask, an answer from before the question not taken, the cost, a panic that holds
  nothing), and the person socket's refusal one in `crates/localapi`
  (`a_seat_not_known_in_time_refuses_a_person_socket_set_with_try_again`).
  The decision itself (`reads_take_whom_writes_take`, `a_seat0_name_that_resolves_to_another_uid_is_nobody`),
  a lookup that cannot tell (`a_seat_that_cannot_be_told_is_not_nobody`, a received file's two
  answers included), a write's lookup past its time
  (`a_seat0_lookup_past_its_time_refuses_a_write_as_it_refuses_a_read`), a host with no logind
  (`a_host_with_no_logind_has_nobody_at_the_screen`, in `companion` too) and the lookup's wait
  (`a_listing_waits_for_the_seat_inside_the_drawers_timer`), nobody's texts, seat0's own record
  (`seat0s_active_uid_is_logind_s_own_record`), the handle checks, and the default folder with nobody
  at seat0 (`nobody_at_the_screen_takes_no_folder_a_controller_chose`,
  `nobody_at_the_screen_takes_no_configured_folder_either`), and a files folder's set with no one
  known (`a_files_folder_set_with_no_one_known_is_refused_and_a_clear_shows_pending`) are unit tests
  on every Linux run. An earlier set's late status answer
  (`an_earlier_files_folder_sets_late_answer_is_never_published`) is one on every OS.

**A server where nobody signs in at the screen** (most servers, cluster nodes, containers, WSL)
loses the drawer and downloads to this rule. Its administrator gives them back on that one machine
with a drop-in for the service:

```bash
sudo mkdir -p /etc/systemd/system/roomlerd.service.d
printf '[Service]\nEnvironment=ROOMLERD_FILES_AS_SEAT_USER=0\n' \
  | sudo tee /etc/systemd/system/roomlerd.service.d/files-as-the-service.conf
sudo systemctl daemon-reload
sudo systemctl restart roomlerd.service   # ends the sessions under way
```

- **What it sets**: `ROOMLERD_FILES_AS_SEAT_USER=0` in the daemon's environment.
- **Who may write it**: root alone. The drop-in folder is root's, and remote config cannot set the
  switch: the daemon reads it from its environment and nowhere else.
- **What it gives back**: the daemon's own identity for file transfer on that machine, both
  directions. Every controller allowed file transfer there then browses, downloads and receives
  files as root.
- **To undo it**: remove the file, `sudo systemctl daemon-reload`, and restart the service.
- **A host systemd did not boot** (a container, WSL without systemd) has no service to drop it
  into: the same variable goes into the daemon's environment wherever root starts it.

### 9.5 A received file stays off folders another account can write (#3688)

On Linux, a received file is staged beside the folder it lands in, as
`<folder>/.roomler-partial/<id>/{meta.json,data}`, and renamed into place at its end. Where the
daemon's own files land (§9.4: nobody at seat0, root at it, the switch off, a per-user daemon) and
everything in that staging is now its writer's own, made so and checked so, and a transfer holds
its folders from its begin to its end. A folder on a file system that keeps no Unix owners or modes
(a FAT or exFAT disk, NTFS without permissions, WSL's drive mounts without metadata, a share mounted
with fixed modes) cannot hold a staging folder of its writer's own. An upload into it, the
person's included, stages in its writer's own private folder instead, and its end copies the file
across:

```mermaid
flowchart TD
    B["files:begin, as the daemon itself"] --> X{"its XDG Downloads<br/>exists and passes?"}
    X -- "yes" --> T["that folder"]
    X -- "no" --> I{"it opened and failed,<br/>and it is ~/Downloads itself?"}
    I -- "no" --> D{"~/Downloads"}
    D -- "missing" --> M["made 0700"] --> T
    D -- "passes" --> T
    D -- "fails, or no home" --> R["refused, saying why:<br/>&lt;folder&gt; cannot receive files: …"]
    I -- "yes: refused once,<br/>no fallback (#3734)" --> R
    T --> H["the folder opened (links followed)<br/>and checked on that handle, then held"]
    H --> K{"does its file system<br/>keep Unix modes?"}
    K -- "yes" --> S[".roomler-partial: made 0700, or the writer's own<br/>(made 0700 again when others could write it);<br/>a link or another account's: refused"]
    K -- "no" --> Q{"root writing, and another account<br/>can write the folder?"}
    Q -- "yes" --> R
    Q -- "no" --> V["the writer's own private folder, made 0700:<br/>~/.local/state/roomler/staging,<br/>root's /var/lib/roomler/staging (in a<br/>/var/lib/roomler only root may write)"]
    S --> P["&lt;id&gt; 0700, meta.json 0600, data 0600 or 0644,<br/>each new, none through a link"]
    V --> P
    P --> E["files:end: renamed from the held &lt;id&gt;<br/>into the held folder; from a private folder,<br/>copied into a new file there"]
```

| What | Where (`agents/roomlerd/src/files.rs`, `agents/roomlerd/src/files/staging.rs`) | Why |
|---|---|---|
| The daemon's own default folder | `default_download_dir` (`files.rs:4986`), `own_home_and_downloads` (`:5299`), `account_home` (`:5155`), `may_receive` (`staging.rs:201`), `same_folder` (`:380`) | the XDG Downloads folder when it exists and passes, else `~/Downloads`, made 0700 when a file arrives (looking, for the status card, the sweep or a resume, makes nothing), else a refusal the viewer shows. An XDG folder that fails and is `~/Downloads` itself (the same device and inode, on the same mount, so a link to it counts) is that refusal at once, in the fallback's words, with no line saying `~/Downloads` is being tried: the fallback could only refuse it again for the same reason (#3734). Each is opened following its links, and the folder it leads to must be the writer's own and never the temp dir; for root, one no other account may write; for a per-user daemon, one not every account may (Ubuntu's private-group umask makes `~/Downloads` 0775), unless its file system keeps no owners or modes, where it need only be one the daemon can write into. Root's home is the one its account names (`getpwuid`), never `HOME`, and must pass root's rule itself; its `user-dirs.dirs` counts only when it is root's own plain file that nobody else may write, and only for a folder inside that home. It was the temp dir, which for root on a server is `/tmp`. The person at seat0 keeps their own folder (§9.1) |
| The folder held | `stage_held` (`files.rs:5080`), called from `begin_as` (`:2584`) with what `drop_folder_for` (`:4843`) resolved | begin opens the target folder once and holds it; whenever begin resolved the daemon's default folder, that very handle is checked again by the rule above |
| The staging, made | `open_staging` (`staging.rs:541`), `stage` (`:746`) | every folder and file with an explicit mode, whatever the umask: a per-user daemon on Ubuntu runs under 0002, and its staging still passes its own checks. A `.roomler-partial` that is a link, or another account's, is refused with a reason and left as it is; the writer's own, open to others, is made 0700 again through its handle. Another upload's end removes an empty `.roomler-partial`, and one gone between its open and the making of `<id>` in it is made and opened again. `data` is 0600 for the daemon alone (§9.4), 0644 otherwise, and is the received file itself after the rename |
| A folder that keeps no modes | `keeps_modes` (`staging.rs:296`), `writable` (`:244`), `stage_privately` (`:789`), `private_staging_root` (`files.rs:5121`), from `stage_held` (`:5110`) | whether a folder made in the target 0700, under a random name, reads back its writer's own (after one `fchmod`, made only on a folder that reads as the writer's), or exactly like the target (the same owner and permission bits: no modes kept). Anything else is no answer, and the target's owner and modes still count. A definite answer is kept per mount, file system type and writer, and nothing the probe makes stays: the startup sweep removes an empty probe folder a crash left (`sweep_probes`). Where it does not, the upload stages in its writer's own private folder, on a file system that keeps modes: `~/.local/state/roomler/staging` in the person's home, or a per-user daemon's (under `$XDG_STATE_HOME` when that is set), `/var/lib/roomler/staging` for root, in a `/var/lib/roomler` opened without following a link that is root's and no other account may write (made 0755 when absent), else that staging is refused; made 0700 and checked as `.roomler-partial` is. There a folder's owner and modes say nothing (a VirtualBox shared folder reads `root:vboxsf`, a share mounted with a fixed owner that owner), so a per-user daemon's default folder needs only to be one it can write into, as the kernel answers for it (`writable`), and receives by copy. A root writer still places only in a folder no other account may write |
| The staging, trusted | `reopen` (`staging.rs:819`), `read_notes` (`:875`), `reopen_data` (`:895`), `FilesHandler::resume_held` (`files.rs:3405`) | a resume walks the partial again from its notes' path: the folder it sits beside (links followed: that folder's own rule) or the writer's private folder, then each step below it without following a link (and without waiting on a FIFO), and trusts it only when its folder, `meta.json` and `data` are its stager's own and nobody else may write them. Anything else is "no partial state for id …", and the viewer starts that upload over. `data` is opened once and truncated on that handle |
| A root writer's resume and end | `root_may_place_in` (`files.rs:5200`), `FsAs::writes_as_root` (`:663`), from `resume_held` (`:3462`, `:3477`) and `end` (`:2826`) | the folder a partial's notes record must be one the rule gives it now, under a bare name: the default folder, its handle passing the rule; for root at the screen or the switch off, also the configured folder or a folder a controller may choose now. A partial beside a folder must record that very folder; one in root's private folder places only in a folder no other account may write. The person's partial, and a per-user daemon's, are placed where their notes say, as them (§9.1) |
| The end | `place` (`staging.rs:937`), `copy_across` (`:1009`), `discard` (`:1040`), from `end` (`files.rs:2839`) | renamed from the held `<id>` into the held folder, under a name nothing there has yet, a link counted; a folder upload's subfolders are made and opened one by one from it, and for a root writer a link among them refuses. From a private folder, or across file systems, copied through handles into a new file (`O_CREAT`, `O_EXCL`, `O_NOFOLLOW`) with the staged file's mode. A folder moved aside mid-upload, with another put in its place, changes nothing |
| Cancel and the sweep | `cancel_incoming` (`files.rs:3071`), `sweep_orphans` (`:1760`), `sweep_orphans_dir_here` (`:1905`), `sweep` (`staging.rs:1140`) | a cancel removes through the handles the transfer holds, or walks again with the same checks and removes only what passes. The startup sweep walks each drop folder's `.roomler-partial` and the writer's private folder. It registers only a partial that passes in full; removes one whose notes are missing or corrupt, and, once over a day old, the stager's own that fails the checks; and leaves a link, or anything another account owns, as it is |
| A resume's lookup | `drop_folder_without_creating` (`files.rs:5062`), from `resume_incoming_as` (`:3237`) | with nothing registered, a resume looks in the writer's private folder, then beside the folder a file would land in now, and makes nothing while it looks |

What a user notices:

- **With nobody at the screen, uploads land in `/root/Downloads`** (made 0700, root's), where they
  used to land in `/tmp`; each file is 0600, root's only (§9.4).
- **With the switch off (§9.4's drop-in) and a person at the screen**, received files sit in root's
  own 0700 folder too, out of that person's reach; before, they landed in `/tmp`, 0644.
- **A headless per-user install with no `user-dirs.dirs`** makes `~/Downloads` (0700) and receives
  there; before, it used `/tmp`.
- **A folder on a disk that keeps no Unix modes** (a FAT USB stick, a Windows drive under WSL, a
  share mounted with fixed modes) receives files: the upload stages in its writer's own private
  folder (`~/.local/state/roomler/staging`, or `/var/lib/roomler/staging` for root), and its end
  copies the file across, so the whole file must also fit where that private folder is. The sandboxed
  per-user unit may write only `~/.local/state/roomler`, so with `XDG_STATE_HOME` set elsewhere that
  staging is refused (read-only). A per-user daemon's Downloads there is not refused for reading
  world-writable; root still refuses a folder another account can write.
  Nor for reading another account's (a VirtualBox shared folder, a share mounted with a fixed owner), as
  long as the daemon can write into it ("… cannot receive files: file transfer cannot write into it").
- **A root writer's `files:end` can be refused** when its folder no longer passes the rule:
  "`<folder>` is not a folder this computer receives files into now (…), so the file is not placed
  there". The partial stays, and a resume completes the upload once the folder passes again.
- **A daemon running as root takes its home from its account**, not from `HOME`: one started by hand
  with another account's `HOME` (as `sudo -E` keeps it) receives into root's own Downloads.
  A configured folder, or one a controller chose, must then sit inside root's own home as well.
- **A default folder that cannot receive files says so**: "`<folder>` cannot receive files: another
  account can write to it" (or "it belongs to another account", "every account can write to it",
  "it is not a folder", "it could not be made (…)"). The sandboxed per-user unit
  (`ProtectHome=read-only`) can make no `~/Downloads` and says so; it could not write `/tmp` either.
- **A per-user daemon's partials from before this change** (made under umask 0002, so its group
  could write them) are refused once on resume, and that upload starts over; the sweep removes them
  after a day. Its `.roomler-partial` is made 0700 again.
- **A received file on a per-user daemon is 0644** where umask 0002 made it 0664.
- **A host whose XDG Downloads folder is configured but missing** receives into `~/Downloads`;
  before, the first upload made the XDG folder.
- **A host whose XDG Downloads folder is `~/Downloads` itself and cannot receive files** logs one
  line for a refused upload, the refusal (#3734). Before, a line first said the fallback was trying
  `~/Downloads`, the folder just refused. The viewer's message is the same as before. An XDG folder
  elsewhere still falls back to `~/Downloads` and says so.
- **A root writer's folder upload into a subfolder that is a link is refused**: "`<path>` is not a
  folder, so file transfer does not place the file there". The person's, and a per-user daemon's,
  still follow their own links with their own rights.
- macOS and Windows are unchanged: macOS's default is always `~/Downloads`, and a Windows service
  stages in the machine's own folder (§9.2).

**Tested**: `files::seat_user_tests::own_staging`, on every Linux run and again as root in the
`rust-recorder` job's sudo step (`ROOMLERD_TEST_REQUIRE_ROOT=1`). Each stands in the daemon's own
home (`stand_in::own_home`, carried by `FsAs::run` to the step's thread), so none touches a real
home, `/root/Downloads` or `/tmp`: the temp dir is never the folder
(`the_temp_dir_is_never_a_writers_folder`), the fallback is made 0700 and its writer's
(`the_fallback_folder_is_made_0700_and_owned_by_its_writer`), each writer's check on its folder
and through a link (`a_downloads_folder_is_checked_by_its_writer`), an XDG Downloads that is
`~/Downloads` itself refused once, a link to it included, while one elsewhere still falls back
(`an_xdg_downloads_that_is_downloads_itself_is_refused_once`,
`an_xdg_downloads_that_links_to_downloads_is_refused_once`,
`an_xdg_downloads_elsewhere_that_cannot_receive_still_falls_back`), the modes under any umask
(`a_partial_is_made_its_stagers_own_whatever_the_umask`), the writer's own open staging folder made
0700 (`the_writers_own_open_staging_folder_is_tightened_not_refused`), a staging folder that is a
link or another account's refused at begin and by the sweep
(`a_staging_folder_that_is_a_link_is_refused_at_begin_and_by_the_sweep`,
`a_staging_folder_another_account_owns_is_refused_at_begin_and_by_the_sweep`, root), a link at
`data` or `meta.json` and a partial others may write refused on resume
(`a_link_at_data_or_meta_json_is_refused_on_resume`,
`a_resume_of_a_partial_another_account_can_write_is_refused`), the sweep's choices
(`the_sweep_trusts_only_partials_that_are_their_stagers_own`), a root writer's resume held to the
rule's folder (`a_resume_whose_recorded_folder_is_not_the_rules_folder_is_refused`), a staging
folder moved aside mid-upload (`a_staging_folder_swapped_mid_upload_is_not_followed`) and a root
writer's folder upload through a link (`a_folder_upload_follows_no_link_for_root`). With file
systems standing in as keeping no modes (`stand_in::modes_kept`): a per-user daemon's upload
(`a_per_user_daemons_folder_that_keeps_no_modes_receives_by_copy`), the person's
(`a_persons_folder_that_keeps_no_modes_receives_by_copy`, root) and root's, which stays strict
(`root_stays_strict_where_modes_are_not_kept`, root), each staged privately, copied across, resumed
after a restart and swept; a per-user daemon's Downloads leading to a folder another account owns,
which it can write into, taken and filled by copy, and another account's it cannot write into passed
over (`a_per_user_daemons_folder_of_another_owner_keeping_no_modes_receives_by_copy`; as root, the
same folder refused). Then root's home taken from its account, never `HOME`
(`a_root_daemon_takes_its_home_from_its_account_not_its_environment`, root, with
`stand_in::env_home`), and FR-84's rule for a configured or a chosen folder judged against that home
(`a_root_daemons_configured_or_chosen_folder_is_judged_against_its_accounts_home`, root), a probe
that does not open as made answering nothing (`a_probe_that_does_not_open_as_it_was_made_answers_nothing`),
root's private staging only in root's own `/var/lib/roomler`
(`roots_private_staging_sits_only_in_roots_own_var_lib_roomler`, root), a probe folder left behind
swept (`the_sweep_removes_a_probe_folder_left_behind`), a failed copy across keeping its partial
(`a_copy_across_that_fails_keeps_its_partial`), a private partial resumed past 256 KiB
(`a_private_partial_resumes_past_256_kib`), a
`.roomler-partial` removed before the upload's own folder is made in it
(`a_begin_whose_staging_folder_goes_before_its_id_is_made_makes_it_again`), a default folder swapped
before its staging opens it (`a_default_folder_swapped_before_its_staging_is_held_to_its_rule`), a
daemon that is root placing in the configured or a chosen folder
(`a_root_daemon_places_in_the_configured_or_a_chosen_folder`, root), and an end the rule refuses,
whose partial survives it (`an_end_the_rule_refuses_keeps_its_partial_for_a_resume`). The umask
test runs again in a child process of its own under umask 0002.

### 9.6 A Mac's system install transfers files only in the person's session (#3757)

On a Mac with a system install, the root daemon holds the enrollment and hands each remote-control
session to the GUI worker it runs in the session of the person signed in at the screen (FR-43 P2b,
[remote-control.md §11.16.1](remote-control.md#11161-a-supervised-macs-delegated-sessions-fr-138-p2b)).
The session's peer, and with it every data channel, then lives in the worker, which runs as that
person: a file a viewer sends lands in their folder and is theirs, and a download or the drawer reads
with their rights, as on a per-user install. A session no worker serves has its peer in the root
daemon instead: nobody is signed in at the screen yet, or the Mac has two rows and this is its
`-daemon` row, which supervises no worker, or it is a secondary organization's session on a Mac that
is not one row (only the primary's sessions cross there). Until #3757 the root daemon served that
session's `files` channel itself, both ways. It now declines it:

```mermaid
flowchart TD
    O["rc:sdp.offer reaches the root daemon"] --> W{"a GUI worker takes it?<br/>DelegateHost::send_to_worker"}
    W -- "yes" --> P["the worker builds the peer,<br/>running as the person at the screen"]
    P --> S["files channel served as before,<br/>with that person's rights"]
    W -- "no worker serves it" --> D["the root daemon builds the peer"]
    D --> G{"the files channel opens:<br/>a Mac, effective uid 0?<br/>files_channel"}
    G -- "yes" --> X["declined: each request answered with the reason,<br/>files:dir-error in the drawer, files:error on a transfer;<br/>nothing read or written, the session goes on"]
    G -- "no" --> S
```

| What | Where (`agents/roomlerd/src/peer.rs`) | Why |
|---|---|---|
| The decision | `files_channel` (`agents/roomlerd/src/peer.rs:13025`), asked as the channel opens (`files_channel_here`, `:13034`, from the `"files"` arm, `:1331`) | a Mac process whose effective uid, the uid its file access runs under, is 0 declines; every other process serves. It needs no word from the delegation: a session's data channels live in its peer, an offer a worker takes builds that peer in the worker (`agents/roomlerd/src/signaling.rs:3944`), which runs as the person (the supervisor starts none for the login window, `agents/roomlerd/src/macos_supervisor.rs:1153`, and starts it with `sudo -u`), and an offer no worker takes builds it in the root daemon (`signaling.rs:4883`) |
| Mid-session | the peer stays where its offer was answered | a worker that attaches later moves no peer, so a declined channel stays declined until the session ends, and the next session is the worker's. A worker that leaves takes its own peer with it, never to the daemon. A new offer is decided again wherever it lands |
| The channel | `attach_files` (`agents/roomlerd/src/peer.rs:13060`), `attach_files_declined` (`:13083`), `declined_reply` (`:13114`) | a grant without FILES gets the FILES-denied channel first, as before. A declined one answers each request with the reason in the frame its waiter in the viewer settles on: `files:dir-error` for a listing, `files:error` for an upload, a download, a folder download, a resume, an end or a cancel; a chunk is dropped. Nothing on the host is read or written, and nothing ends the channel or the session |
| The reason | `FILES_IN_THE_PERSONS_SESSION` (`agents/roomlerd/src/peer.rs:12993`) | "File transfer on this Mac needs someone signed in at its screen and runs in their session, not in this one. Reconnect once someone is signed in, or connect to the Mac's user-session agent." |
| What stays | the device's caps (`agents/roomlerd/src/encode/caps.rs:4482`) | the row still offers file transfer: on a one-row Mac it is the row the worker serves whenever someone is signed in, and the hub puts FILES in a grant, which consent names, from these words when a session is asked. Consent, the session request and every other channel are unchanged |

What a viewer notices:

- **Before anyone signs in at the Mac's screen**, a session shows no screen, and Files shows the
  reason in the drawer and on every transfer. The session stays up. Reconnect once someone is signed
  in: the next session is the worker's.
- **On a two-row Mac's `-daemon` row** the same, whoever is signed in: the person's own row serves
  the screen and files.
- **A session that began before the person signed in** keeps its declined channel until it ends.
- **The no-screen sentence** that daemon sends (`no_display`, `agents/roomlerd/src/capture/mod.rs:774`)
  ends "Remote shell works here.", where it said file transfer worked there too.
- Windows, Linux, a per-user Mac and every session a GUI worker serves are unchanged.

**Tested**: `peer::files_channel_tests` (`agents/roomlerd/src/peer.rs:17116`), on every lane's lib
tests: the decision and its reason's words; each request's answer (`declined_reply`); and through
the router over an in-process peer pair, both peers closed after each test, a declined channel that
answers each request with the reason, leaves the folder asked about as it was and stays up
(`a_declined_channel_answers_each_request_with_the_reason_and_stays_up`), a served one that lists the
folder (`a_served_channel_keeps_the_file_transfer_handler`), and a grant without FILES that keeps its
own words (`a_session_without_files_keeps_the_not_granted_channel`). The row's offer:
`encode::caps::tests::a_macs_root_half_still_offers_file_transfer`. In the macOS job the module runs as
the runner's user and again under sudo with `ROOMLERD_TEST_REQUIRE_ROOT=1`, where the Mac's own
effective uid declines (`as_root::a_root_process_on_a_mac_declines_the_files_channel`); the step fails
unless that test ran.

## 10. Starting after install, and the Welcome flow

Every install path ends with the daemon starting, so the **daemon** opens the companion —
once per install, into the signed-in user's session, with `--first-run`
(`agents/roomlerd/src/companion/launch_once.rs`). Who does it on each platform, the login
start, the marker that makes it one-shot, and why an upgrade is *adopted* rather than
launched into are in [installation.md](installation.md) §"After install: the companion
opens". The rules that matter here:

- ⚠️ **No inherited handles.** A companion the daemon starts gets none of the daemon's
  handles: `CreateProcessW` with `bInheritHandles = FALSE`, detached, and outside the
  daemon's job object where that is allowed
  (`agents/roomlerd/src/win_service/companion_spawn.rs:98`). Before this, a companion
  spawned by a daemon worker could outlive it holding the worker's handles — the #1035
  signature, tunnel ports held by a process whose parent was already gone. A route that
  cannot bind its port for two minutes now logs who holds it.
- **Per-machine Windows launches from the service host, with the console user's own,
  non-elevated token** — never from a worker that may run elevated.
- **The companion decides what to show** (`agents/roomler-desktop/src/first_run.rs:75`):
  it reads its own arguments on the first launch too (`--first-run`, `--autostart`,
  `--for-prompt`, `--view=<name>`) and a per-user `desktop-state.json` (`first_run_done`, the
  start-at-login opt-out; `crates/agent-core/src/desktop_state.rs`). A missing or unreadable
  file is a first run. Until the tour is finished or skipped it shows at every start but the
  daemon's start for a consent question or a banner, which keeps it for the person's own
  start (#3887, §10.6); afterwards a login start stays in the tray.

**The Welcome** (`front/welcome.js`) is six steps — this device · the private network ·
who may view this screen · files you receive · start at login · done:

```mermaid
flowchart LR
    A["This device<br/>(enrolled as … on …)"] --> B["Private network<br/>what it is · what it changes · Enable"]
    B --> C["Who may view this screen<br/>ask me before someone connects"]
    C --> D["Files you receive<br/>§9"]
    D --> E["Start at login"]
    E --> F["Done → Overview"]
```

- **Enable** on the private network writes `overlay_enabled`, then uses the same restart
  flow as Settings' Apply now (`window.Roomler.restartDaemon`, `front/settings.js`) and
  waits up to a minute for the overlay address. A per-user install cannot create the
  WireGuard adapter, so it is offered **userspace mode** instead: `netstack_socks_port` on a
  free port (never a hard-coded one a declared route may already hold), explained as "apps
  reach the mesh through SOCKS5 on 127.0.0.1:<port>; remote desktop needs nothing".
- Every refusal from the daemon — Apply now's, `files_dir`'s — is shown verbatim, and a
  service too old for a step says so and points at the manual way instead.
- The tour can be reopened from the tray and from Settings.
- The whole device can opt out: `companion_autostart = false` stops the post-install launch
  and removes the Windows login start. On a multi-user server that is the switch to use —
  the machine-wide login start opens a companion in every interactive session.

### 10.1 The Linux packages (FR-93 P6d, P6e)

Before P6d the companion shipped as an x86_64 `.deb` only, so a system install on Fedora,
the RHEL family or openSUSE, and every aarch64 host (Asahi, Ampere), had no consent prompt
and no "being viewed" banner. The release now carries four Linux packages, built from the same
binary per architecture:

| | x86_64 | aarch64 |
|---|---|---|
| `.deb` (`cargo deb`) | `roomler-desktop-<v>-x86_64-unknown-linux-gnu.deb` | `roomler-desktop-<v>-aarch64-unknown-linux-gnu.deb` |
| `.rpm` (`cargo generate-rpm`) | `roomler-desktop-<v>-x86_64-unknown-linux-gnu.rpm` | `roomler-desktop-<v>-aarch64-unknown-linux-gnu.rpm` |

```mermaid
flowchart LR
  B["cargo build -p roomler-desktop<br/>(Ubuntu 22.04: the oldest glibc we support)"] --> D["cargo deb"]
  B --> R["cargo generate-rpm<br/>requires written out"]
  R --> G{"guard: every DT_NEEDED<br/>soname required? no ICU,<br/>no roomlerd? the arch?"}
  G -- yes --> U["the release: .deb, .rpm, .sha256, .asc"]
  G -- no --> F["the job fails"]
```

- ⚠️ **The `.rpm`'s requires are written out** in `agents/roomler-desktop/Cargo.toml`
  (`auto-req = "no"`). `cargo generate-rpm`'s discovery runs `ldd`, which names the build host's
  *transitive* libraries, Ubuntu 22.04's ICU 70 among them, and no Fedora satisfies those. The
  list is the binary's own `DT_NEEDED`, by soname (the spelling every RPM distro provides), plus
  `libayatana-appindicator3.so.1`, which the tray dlopens. A hand-kept list drifts, so the
  release job reads both back: a library the binary links that the package does not require
  fails the job.
- There is **no `roomlerd` require** in the `.rpm`: those hosts install the daemon from the
  tarball, not a package. The `.deb` keeps its `Depends: roomlerd`.
- `install.sh` picks the package by the host's tools: `dpkg` takes the `.deb`; otherwise `rpm` with
  `dnf`, `zypper` or `yum` takes the `.rpm`, and the package manager pulls webkit2gtk, GTK and the
  tray library from the distro's repositories. zypper needs `--allow-unsigned-rpm` for a local
  package; the sha256 is checked first.
- The daemon reports the installed companion's version from `dpkg-query`, else from
  `rpm -q --qf '%{VERSION}'` (`companion.rs`, `installed_version_inner`).
- Every asset is signed (`.asc`) like the daemon's. The server's daemon-first ordering
  (`agent_release::order_assets_daemon_first`) keys on the `roomler-desktop-` prefix, so an
  `.rpm` sorts after the daemon's packages too. The daemon's own picker never takes a companion
  package: it matches the daemon's `.deb` and `.tar.gz` only. From FR-117 the updater moves an
  INSTALLED companion package to the daemon's version through a picker of its own (§13).

### 10.2 The tray's start for a prompt on Linux, off the async worker (#3855)

The tray is the surface a "Prompt on host" device asks through when the daemon draws no panel of
its own (a root daemon has no display), so a consent prompt starts it when it is not running
(`ensure_running`, [`companion.rs:1158`](../agents/roomlerd/src/companion.rs#L1158)). On Linux that
start asks four other processes: logind for the graphical session (`loginctl`), the process list
for the person's tray (`pgrep -u`), the person's user manager for the session's environment (a
root daemon reads it as them, which looks their account up in the account database), and
systemd-run. Until #3855 the start was an async function that never awaited, so every step ran
on the async worker that polled it, for as long as the step took, and the callers' 3 s timeout
(`COMPANION_START_BUDGET`, [`signaling.rs:341`](../agents/roomlerd/src/signaling.rs#L341)) could
not fire: a future that never yields is never timed out. A stopped logind or a stalled SSSD held
that worker, and every socket and timer on it, the shape #3830 measured.

```mermaid
flowchart TD
    P([a prompt with no native panel]) --> E["ensure_running: a verdict that still stands,<br/>20 s, or 2 s after a failure (#3884), or a start"]
    E --> A{a start in flight?}
    A -->|one began before this ask| W[wait it out, its answer is<br/>not this ask's]
    W --> A
    A -->|none, or one begun since| S[join it, or start one<br/>on the tray-start thread]
    S --> T[path, then logind, then pgrep,<br/>then the session's environment,<br/>then systemd-run]
    S --> K{answered within 2.5 s?}
    K -->|yes| V[its outcome: Started only<br/>when systemd-run exited 0]
    K -->|no| F[an error naming the step<br/>the start was still waiting on]
    F --> N[could not start the desktop companion:<br/>no surface, then the notification]
```

| Piece | Where | What |
|---|---|---|
| the start, off the worker | `ensure_running_inner`, [`companion.rs:1785`](../agents/roomlerd/src/companion.rs#L1785) (one for Linux, Windows and macOS since #3858, §10.4); `TrayStarts::start`, [`:2166`](../agents/roomlerd/src/companion.rs#L2166) | the prompt's task awaits the start; it never runs a step itself |
| one start in flight | `TRAY_STARTS`, [`companion.rs:2140`](../agents/roomlerd/src/companion.rs#L2140), a `one_in_flight::OneInFlight` on a thread named `tray-start` | while a step hangs, every prompt that asks waits on that one start and starts none of its own, so a hang holds one thread however often a viewer asks. A start that began before an ask is waited out, and the ask then starts afresh: the person at the screen may have changed meanwhile |
| the steps | `TraySteps`, [`companion.rs:2251`](../agents/roomlerd/src/companion.rs#L2251); `start_tray`, [`:2299`](../agents/roomlerd/src/companion.rs#L2299) | in the order the start always took them, each recorded as it begins (`TrayStep`, [`:2185`](../agents/roomlerd/src/companion.rs#L2185)). Who the tray is for (logind's active graphical session, `pgrep -u` per uid), the user manager for a per-user daemon and the system manager as the person for a root one, #2713's environment and #2729's `KillMode=process` are as before. Since #3887 the companion's argv carries `--for-prompt` (§10.6) |
| the bound | `TRAY_START_WAIT`, [`companion.rs:2359`](../agents/roomlerd/src/companion.rs#L2359): 2.5 s | under the callers' 3 s, so the start's own reason lands before their fallback fires (with equal timers the hub's fallback fired first, FR-27). A start that works takes tens to a few hundred milliseconds, under half the wait, which one start in flight needs to answer an ask that comes just after another began |
| the failure | `started_in_time`, [`companion.rs:2223`](../agents/roomlerd/src/companion.rs#L2223) | a start's own failure keeps its words (`finding a graphical login session: …`, `systemd-run failed: …`); no answer in time is `the tray's start did not finish within 2.5s: it was still waiting on logind (…)`, naming the step. `ensure_running` logs either as `could not start the desktop companion` and the prompt has no surface, as for any failed start. The start goes on on its thread: if it later brings the tray up, the tray reads the `.pending` marker and shows the question. Since #3884 that verdict stands 2 s, not 20 s, so the next prompt asks again (§10.5) |

The session's environment is read as the person through `exec::drop_to_std`, whose account lookup
is bounded too (1.5 s, `docs/roomler-ssh.md`, "The blocking callers"); a lookup that does not answer
starts the tray with logind's four variables, as any failed read always did. The environment's own
read keeps its 5 s (`stdout_bounded`), so a wedged user manager fails the prompt's wait and the
start goes on without it.

- Every caller gets the bound: the remote-control prompt on the WS loop (started there and
  waited for by the session's consent task since #3888, §10.5), the exec prompt, a
  recording's and an SSH session's `Companion::up`, and the "being viewed" banner (FR-93 P6g),
  two of which had no timeout at all.
- After a failed start the remote-control prompt tries FR-93 P6f's notification, whose
  session lookup (the portal helper's `loginctl` walk) this did not bound: with logind
  stopped, the WS loop waited out `loginctl`'s own D-Bus timeout (~25 s) there. #3857 takes
  the notification off the loop and bounds the walk (§10.3).
- Tested with the steps stood in for each test's own tasks (`tray_start_tests`,
  [`companion.rs:2484`](../agents/roomlerd/src/companion.rs#L2484); rust-agent's "Test agent under
  vp9-444"): with logind, the environment or systemd-run held, a start fails within 2.5 s naming
  the step; a 10 ms timer on a two-worker runtime stays on time while prompts ask; asks at once
  and in a row share one start; and a start in time does what it always did, the argv included.
  Field cells: a paused logind during a prompt, the start after it resumes, and the control.

### 10.3 The notification fallback on Linux, off the WS loop (#3857)

Where neither the native panel nor the tray can show a remote-control question (no tray
installed, its start failed, or no marker for it to read), FR-93 P6f puts it up as a desktop
notification (`docs/remote-control.md`, "Prompt surfaces"). The session helper that posts it
runs as the person at the screen, so its start asks logind for the graphical session
(`loginctl list-sessions`, then `show-session` for each), the account database for the
person's account, and the person's session bus. Until #3857 the org's WS loop waited for all
of that inline, and the session walk had no bound: with logind stopped, every `loginctl` waits
out its D-Bus timeout, about 25 s. No async worker was held (the show ran on the blocking
pool), but that org's control channel read no frame meanwhile: another viewer's request, a live
session's ICE, a `Terminate` for this very question and Fleet RPC all waited behind it. And the
agent's `no_prompt_surface` came after the hub's own timer (5 s past the window,
`CONSENT_VERDICT_GRACE`), so the controller was told "nobody answered".

```mermaid
sequenceDiagram
    participant L as WS loop
    participant T as consent task
    participant S as the show, on the blocking pool
    participant W as session-walk, one in flight
    L->>L: no native panel: the tray's start, started and not awaited
    L->>T: spawn it, with the start's handle
    Note over L: reads its next frame
    T->>T: the tray's verdict (2.5 s at most)
    T->>S: where the tray cannot show it: start the notification, not awaited
    S->>W: the graphical session, 1.5 s at most
    W-->>S: a session, or logind's nobody, or not known in time
    S->>S: the account (1.5 s), the helper, its first line (5 s)
    S-->>T: up, or why not
    T->>T: the marker says Notification, the surface line is logged
    T->>T: the prompt's window starts
```

| Piece | Where | What |
|---|---|---|
| the start | `notification_for_rc`, [`signaling.rs:7623`](../agents/roomlerd/src/signaling.rs#L7623); `ViewerIndicator::start_notification_prompt`, [`indicator/mod.rs:287`](../agents/roomlerd/src/indicator/mod.rs#L287) | only where the tray cannot show the question, in FR-27's order. It STARTS the show and returns: the show runs on the blocking pool as it always did, and nothing waits for it there. The loop started it until #3888; the consent task starts it now, once the tray's verdict is in (§10.5) |
| the consent task | `rc_surfaces_up`, [`signaling.rs:7570`](../agents/roomlerd/src/signaling.rs#L7570); `notification_shown`, [`:7638`](../agents/roomlerd/src/signaling.rs#L7638) | waits for the show's verdict before it first looks at the prompt, so the window still starts after the show. Then it writes the marker's `Notification` and logs `consent prompt surface` (`native`, `companion` since #3888, `notified`, `have_surface`), as the loop did |
| the session walk | `graphical_session_within`, [`companion.rs:8372`](../agents/roomlerd/src/companion.rs#L8372); `SESSION_WALKS`, [`:8400`](../agents/roomlerd/src/companion.rs#L8400) | one walk in flight per kind of ask (the uid asked for, and whether only a person's session will do), on a thread named `session-walk`. Each caller waits `SESSION_WALK_WAIT`, 1.5 s ([`:8356`](../agents/roomlerd/src/companion.rs#L8356)), the seat and account lookups' wait. No answer in time is "not known" with its reason (`walked_in_time`, [`:8413`](../agents/roomlerd/src/companion.rs#L8413)), never "nobody at the screen" |
| a question that ends on the way | `Prompts::coming`, [`indicator/notify.rs:358`](../agents/roomlerd/src/indicator/notify.rs#L358); `launch`, [`:505`](../agents/roomlerd/src/indicator/notify.rs#L505) | the question is on the books as coming before its show starts. `hide_prompt` (a `Terminate`, an answer from another surface, the closed socket the consent task watches) takes it off, and the show then starts no helper, or drops the one it started, whose helper closes the notification and exits. Its verdict is `its question ended while the notification was being put up`, once |

Every caller of the portal's session helper takes the walk through `helper_session`
([`capture/portal/mod.rs:1330`](../agents/roomlerd/src/capture/portal/mod.rs#L1330)), all on
blocking threads: the capture, the portal probe, the audio, keep busy, FR-95's notification
probe, the notification prompt and the being-viewed notice (FR-93 P6g). So does the recorder's
launch (`session_of`,
[`recording/launch.rs:1827`](../agents/roomlerd/src/recording/launch.rs#L1827)). A walk out of
time is `SpawnError::Other` with the reason for the helper's callers (so the capture says the
session is not known, the audio is a `backend_error`, keep busy a spawn error), and an error
naming the uid for the recorder. logind's own answer keeps its words and its kind
(`NoSession`).

- **The bounds on the consent path**, with logind stopped: the tray's start gives up at 2.5 s
  (§10.2) and the walk at 1.5 s, so the consent task hears "not up" about 4 s after the request.
  The window starts then, and the agent's `no_prompt_surface` reaches the controller about a
  second ahead of the hub's own timer. With logind answering and the session bus wedged, the
  helper's own 5 s (`SHOW_BUDGET`) still applies: the verdict comes then, as before, and the
  helper is let go, its stdin closed, and killed 1 s later if it is still there (#3886, below).
- **A late prompt: nothing comes up late.** A walk out of time starts no helper (its late
  answer reaches no one, and the next ask waits for it or walks afresh); a helper that has not
  posted within 5 s is let go and then killed, and takes down a notification it did post
  (#3886); a question that ended on the way gets none. The viewer waits out
  the window and is told `no_prompt_surface`, unless someone answers on another surface (the
  CLI). The log has `no notification prompt (FR-93 P6f)` with the session and the reason, then
  `consent prompt surface … notified=false have_surface=false`.
- **Fork safety** is unchanged: the session and the account are resolved in the parent, before
  the fork, and nothing new runs in `pre_exec`.
- **Unchanged:** which surface is chosen and when, the question's words, and the tray's start
  (§10.2). While logind hangs, the tray's start holds its `tray-start` thread and the
  notification's walk its `session-walk` thread, one each however many prompts ask.
- **Tested** in rust-agent's "Test agent under vp9-444" (`portal-capture`):
  `signaling::notification_off_the_loop_tests` (a frame sent while logind holds the
  notification is handled at once, and the verdict comes within the walk's wait; a request that
  ends while its notification comes up drops it, with nothing up and one verdict; a socket that
  closes meanwhile drops it at once; prompts asking again and again delay no 10 ms timer and
  share one walk; the control keeps P6f's order), `companion::session_walk_tests` (asks at once
  and in a row share one walk, and after logind answers the next ask walks afresh; the
  control) and `capture::portal::helper::tests` (a held walk refuses the helper within its wait,
  root and per-user; the control). The recorder's in rust-recorder's "Test the recorder
  (FR-85)": `recording::launch::unix::tests` (a held walk refuses the recorder's session within
  its wait, naming the uid; the control). Field cells: a paused logind while a viewer asks, the
  next prompt after it resumes, and the control.

#### 10.3.1 A helper given up on takes its notification down (#3886), and still passes on a press (#3942)

The helper says its notification is up with one marked line, the reply to its `Notify` turned
into words. On a slow session bus, the case `SHOW_BUDGET` exists for, the notification can be
up on screen while that line is still on its way. Until #3886 the daemon killed the helper at
5 s with `SIGKILL`. A helper killed that way never closes its notification, so it stayed up,
and its Approve and Deny reached no one: the person saw a question while the viewer was told
nobody could be asked.

A helper given up on is now let go the way the daemon takes any notification down. Its stdin
closes, and it takes down a notification it did post, then exits. It is killed only if it is
still there 1 s later. The verdict, "not up", still comes at 5 s.

In that 1 s an answer the helper reports still counts. Its reader passes it on
([`:591`](../agents/roomlerd/src/indicator/notify.rs#L591)) to the consent broker, and the
question is still being asked: "not up" only means that no surface was seen, and the question's
window runs on. So an Approve reaches the viewer as `granted`, and a Deny as the person's
refusal, where the viewer would otherwise be told `no_prompt_surface` at the window's end. On a
slow session bus the helper hears the daemon's close before the server's reply to `Notify`, and
a press's `ActionInvoked` comes after that reply. Until #3942 the helper took the close first,
closed the notification and returned, so such a press reached no one.

Now the daemon's close starts a take-down
([`:1349`](../agents/roomlerd/src/indicator/notify.rs#L1349); `take_down`,
[`:1383`](../agents/roomlerd/src/indicator/notify.rs#L1383)). The helper sends
`CloseNotification` at once, so the notification comes down as early as before. Then it reads
the server's signals until the server says the notification closed. On the one stream, a press
the server took while the notification was up comes before the `NotificationClosed` that the
close brings, so the helper passes it on, as an `ACTION` line, and exits.

| What comes after the close | What the helper does |
|---|---|
| a press: `ActionInvoked`, any key but `default` | passes it on, and exits |
| `NotificationClosed`, reason 3 | exits: reason 3 is its own close, and no answer |
| `NotificationClosed`, any other reason | passes it on, as the loop does, and exits: 2, the person's own dismissal before the close, is a Deny |
| a click on its body: `default` | no answer, and nothing is posted again; the dismissal a server sends after it (GNOME 50) is not passed on |
| nothing, until the wait is over | exits |

- **The wait** ends 500 ms after the helper heard the daemon's close, at most (`TAKE_DOWN_WAIT`,
  [`:1118`](../agents/roomlerd/src/indicator/notify.rs#L1118)). That is half of the daemon's 1 s
  grace. The other half is margin for the helper's exit and for the daemon's read of it on a
  loaded host, so the helper exits before the kill. It lasts at least 50 ms after the helper's
  own close (`TAKE_DOWN_LEAST`, [`:1125`](../agents/roomlerd/src/indicator/notify.rs#L1125)).
  That matters for a `Notify` that answered later than the 500 ms. A bus that stalled delivers
  the answer and a press behind it at once, and the press comes off the bus on the signals' own
  thread. A server that keeps the spec says the notification closed within milliseconds of the
  close, and the wait ends there.
- **The close is sent without waiting for its reply** (`call_noreply`,
  [`:1163`](../agents/roomlerd/src/indicator/notify.rs#L1163)). A reply on the slow bus could
  otherwise hold the helper past the grace. The call returns once the whole message is written to
  the bus's socket, so the close is on its way before the helper exits. That holds on every path
  that closes: an answer, the lifetime's end, the daemon's close (the close goes before the wait),
  and a click on the body (the close goes before the new `Notify`). Read at zbus 5.15.0:
  `Connection::send` writes until every byte of the message is out.
- **Every close the daemon makes** ends the same way. After an ordinary one (the question answered
  elsewhere, timed out or withdrawn), the question is already off the broker's books, so a press
  passed on then is refused there (`record_decision`,
  [`consent.rs:934`](../agents/roomlerd/src/consent.rs#L934)). The being-viewed notice is taken
  down only once its session has ended, so its Disconnect then finds no session to end, and no
  kill is sent (`RcSessionRegistry::disconnect`,
  [`rc_sessions.rs:272`](../agents/roomlerd/src/rc_sessions.rs#L272)).

```mermaid
sequenceDiagram
    participant T as consent task
    participant S as the show, on the blocking pool
    participant R as the helper's reader
    participant H as session helper
    participant N as notification server
    participant B as consent broker
    participant G as notify-let-go
    T->>S: start the notification
    S->>H: start it, and write the question to its stdin
    H->>N: Notify, on a slow session bus
    Note over N: the question is on the screen, and the person presses Approve
    Note over S: no up line within 5 s
    S->>H: close its stdin, the daemon's take-it-down
    S->>G: kill it if it is still there 1 s later
    S-->>T: not up, at 5 s as before
    N-->>H: the late reply, and the press behind it
    H->>N: CloseNotification at once, its reply not waited for
    H->>R: the press, as an ACTION line
    R->>B: Approve, while the question is still asked
    B-->>T: granted, at its next look
    H->>H: exits, and its reader marks it ended, then reaps it
    G->>G: marked within 1 s, so nothing is killed
    Note over G: still unmarked at 1 s - SIGKILL, under the lock
```

| Piece | Where | What |
|---|---|---|
| the give-up | `put_up`, [`indicator/notify.rs:543`](../agents/roomlerd/src/indicator/notify.rs#L543); the stdin's drop, [`:616`](../agents/roomlerd/src/indicator/notify.rs#L616) | no "up" line within `SHOW_BUDGET`: the helper's stdin is dropped, which is what takes any of its notifications down, and the verdict is returned at once, as before: `the session helper did not post it within 5 s` |
| the kill, 1 s later | `let_go`, [`:665`](../agents/roomlerd/src/indicator/notify.rs#L665); `Ended`, [`:632`](../agents/roomlerd/src/indicator/notify.rs#L632); `GIVE_UP_GRACE`, [`:319`](../agents/roomlerd/src/indicator/notify.rs#L319) | a thread named `notify-let-go` waits up to 1 s for the helper's stdout to close, and sends `SIGKILL` only if it has not. The reader marks that close under a lock before it reaps the helper ([`:596`](../agents/roomlerd/src/indicator/notify.rs#L596)), and the kill is sent under the same lock only while unmarked: it reaches our own unreaped child, never a process that took its pid. With no thread for the wait, the kill comes at once, as before |
| the helper's side | `helper::run`, [`:1213`](../agents/roomlerd/src/indicator/notify.rs#L1213); `serve`, [`:1299`](../agents/roomlerd/src/indicator/notify.rs#L1299) | the helper listens for its stdin's close from its start. A close that comes while it is still reaching the session bus posts nothing, and its stderr says `the daemon took it down before it was posted; nothing posted`. After its `Notify` has answered, the close starts the take-down |
| the take-down (#3942) | `serve`, [`:1349`](../agents/roomlerd/src/indicator/notify.rs#L1349); `take_down`, [`:1383`](../agents/roomlerd/src/indicator/notify.rs#L1383); the close, [`:1163`](../agents/roomlerd/src/indicator/notify.rs#L1163) | the close goes at once, its reply not waited for. Then the server's signals are read until it says the notification closed: 500 ms from the daemon's close at most, 50 ms from the helper's own close at least. A press the server took before the close is passed on, and the stderr says `passed on a press of notification N that came after the daemon's close` |
| the expiry | `expire_timeout`, [`:1459`](../agents/roomlerd/src/indicator/notify.rs#L1459); the question's end, [`:1206`](../agents/roomlerd/src/indicator/notify.rs#L1206) | the question is posted to expire at its end: what is left of `lifetime_ms`, counted from the request's arrival, and never 0, which is "never expires". A server that keeps the spec's `expire_timeout` then takes down a notification whose helper was killed with its `Notify` still in flight |

- **The bounds:** the verdict comes at 5 s, as before, so the viewer's `no_prompt_surface` keeps
  its timing against the hub's timer (5 s past the window). The 1 s runs on its own thread, after
  the verdict: no async worker and no caller waits for it. A helper given up on is gone 6 s after
  its start at the latest, and reaped by its reader. Since #3942 it exits by itself 500 ms after
  the give-up, or 50 ms after a later answer.
- **What is left:** a `Notify` the server has still not answered 1 s after the give-up. The
  helper is killed with it in flight, and the server may still put the question up. It expires
  at the question's end on a server that keeps `expire_timeout`. A server that ignores it keeps
  the question until the person closes it, and a press on it reaches no one. Also left: a press
  that reaches the helper after its wait (#3942). The helper has exited, and the press reaches
  no one. On a bus that is slow both ways, a press made once the question shows takes a slow trip
  back after the reply, and can come past the wait and the kill. The take-down reaches a press
  that the bus delivers with the late reply, as a bus that stalls with the question on the
  screen does, or soon after it.
- **GNOME may ignore `expire_timeout`.** On GNOME a question can then stay up until its helper
  closes it at its end, and a notification whose helper was killed (above) until the person
  closes it. Read on 0.4.223 with GNOME's own server (#3886): an unanswered question stayed up
  until its helper closed it when its lifetime ended.
- **The being-viewed notice** (FR-93 P6g) has the same helper, the same give-up and the same
  take-down. It expires after its day (`lifetime_ms`, 24 h), when its helper closes it anyway. A
  Disconnect passed on after its session ended ends nothing (above).
- **Tested** in rust-agent's "Test agent under vp9-444" (`portal-capture`), with `sh` standing in
  for the helper and a file for its notification: `indicator::notify::parent::given_up_tests`
  ([`:820`](../agents/roomlerd/src/indicator/notify.rs#L820)). A helper that posts and answers
  only after the give-up gets the verdict "not up" at the give-up, then takes its notification
  down and is gone. It was red before #3886: killed at the give-up, with its notification left
  up. The controls: a helper that ignores its stdin is still killed within the bound, and one
  that answers in time stays up past the grace until its stdin is dropped.
  `indicator::notify::helper::tests::the_notification_expires_with_its_question` pins the
  expiry: never 0, and capped at `i32::MAX`, never wrapped.
- **Tested (#3942)** in the same step, with a stand-in for the notification server behind the
  helper's `Server` seam ([`:1144`](../agents/roomlerd/src/indicator/notify.rs#L1144)) and no
  session bus: `indicator::notify::helper::tests`
  ([`:1464`](../agents/roomlerd/src/indicator/notify.rs#L1464)).
  - `a_press_behind_a_late_notify_answer_is_passed_on`: the server answers `Notify` after the
    daemon's close, with a press behind its answer. The press is passed on and the notification
    closed once, also for an answer later than the wait. Red before #3942: the helper returned at
    the close, and the press reached no one.
  - `a_press_that_comes_after_the_close_is_passed_on`: a press after the helper's close is
    passed on. Red before #3942.
  - `after_the_close_the_helper_says_what_the_person_did`: a dismissal (2) and an expiry (1) are
    passed on. The helper's own close (3), a click on the body with the dismissal after it, and
    another notification's signals are not, and nothing is posted again. Red before #3942.
  - `the_close_goes_at_once_and_the_helper_returns_when_the_server_says_so`: the helper returns
    when the server says the notification closed, inside the daemon's grace.
  - `a_server_that_never_says_it_closed_holds_the_helper_no_longer_than_the_bound`: such a server
    holds the helper 500 ms, inside the grace. Red before #3942, which returned at once.
  - `after_a_late_notify_answer_the_helper_reads_a_moment_after_its_close`: after a late answer
    the helper still reads 50 ms after its close.
  - On the daemon's side, `given_up_tests::a_press_passed_on_after_the_give_up_reaches_the_consent_sender`
    ([`:1013`](../agents/roomlerd/src/indicator/notify.rs#L1013)): a press the helper passes on
    after the give-up reaches the consent sender within the grace.
- **Field cells:** a Linux system install with no companion and a session bus slowed so that the
  helper answers after 5 s: after the viewer is told `no_prompt_surface`, no consent
  notification stays on screen. The control is the same cell on 0.4.222. For #3942, the same
  install, with the session bus held on its way to the helper while the question is on the screen
  and released just after the give-up. The person presses Approve as soon as the question
  appears: the viewer is granted, and no notification stays up. The control is the same cell on
  0.4.223, where the press is lost and the viewer is told `no_prompt_surface` at the window's end.

### 10.4 The tray's start for a prompt on Windows and macOS (#3858)

The same start runs off the async worker on Windows and macOS. Until #3858 their
`ensure_running_inner` was what §10.2 describes on Linux before #3855: an async function that never
awaited, so the process list, `CreateProcessAsUserW`, `pgrep`, `stat`, `id -u` and `launchctl` ran
on the worker that polled it, and the callers' 3 s budget could not fire. There is now one
`ensure_running_inner` for the three OSes ([`companion.rs:1785`](../agents/roomlerd/src/companion.rs#L1785)), over §10.2's start in flight (`TRAY_STARTS`,
a thread named `tray-start`), its 2.5 s wait (`TRAY_START_WAIT`) and its failure
(`started_in_time`: `the tray's start did not finish within 2.5s: it was still waiting on <step>`).
Each OS has only its own steps: `TrayStep` (what a refusal names), `TraySteps` (the seams a test
stands in, `TraySteps::real` the daemon's) and `start_tray` (the steps in the order the start always
took them, each recorded as it begins).

```mermaid
flowchart LR
    P(["a prompt: ensure_running_inner,<br/>one for the three OSes"]) --> F["TRAY_STARTS: one start in flight<br/>on the tray-start thread, a 2.5 s wait"]
    F --> W["Windows: the tray check, the install path,<br/>the daemon's own token, start_desktop"]
    F --> M["macOS: the app, pgrep, stat /dev/console,<br/>id -u, launchctl kickstart"]
    F --> L["Linux: §10.2's steps"]
    F -->|"no answer in 2.5 s"| E["an error naming the step<br/>the start was still waiting on"]
```

| OS | Step, in order | What runs | A refusal out of time names |
|---|---|---|---|
| Windows: `start_tray`, [`companion.rs:1883`](../agents/roomlerd/src/companion.rs#L1883); `TrayStep`, [`:1800`](../agents/roomlerd/src/companion.rs#L1800) | the tray check | `desktop_running`: `win_service::companion_procs::find`, a process snapshot, then each candidate's loaded image (#1686) | `the process list (…)` |
| | the install path | `installed_desktop`: the companion beside the daemon | `the companion's install path` |
| | the daemon's own token | `respawn_context_for_self`: is this the SYSTEM service? | `the daemon's own token (…)` |
| | the start | `start_desktop`, #3848's, with its session choice: the SYSTEM service starts the tray in the console session as its person (`WTSQueryUserToken`, `CreateEnvironmentBlock`, `CreateProcessAsUserW`), the person's own daemon a plain `CreateProcessW` in its session; since #3887 with `--for-prompt` (§10.6) | `the start in the console session (…)`, or `the start in the daemon's own session (…)` |
| macOS: `start_tray`, [`companion.rs:2040`](../agents/roomlerd/src/companion.rs#L2040); `TrayStep`, [`:1957`](../agents/roomlerd/src/companion.rs#L1957) | the app | `/Applications/Roomler.app` exists | `the companion's app bundle (…)` |
| | the tray check | `pgrep -x roomler-desktop` | `the process list (pgrep)` |
| | the console's owner | `console_user_name`: `stat -f %Su /dev/console`, the way the pkg's postinstall reads it | `the console's owner (stat /dev/console)` |
| | the uid | `account_uid`: `id -u <user>`, from the account database | `the account database (id -u, …)` |
| | the start | `launchctl kickstart -k gui/<uid>/com.roomler.desktop`: launchd starts the LaunchAgent in the console user's GUI domain, with no argument (the companion asks why it came, §10.6) | `launchd (launchctl kickstart)` |

- **`Started` only when the process was created.** On Windows `started_outcome` (#3824) reads
  `start_desktop`'s answer: nobody signed in, or a refused `CreateProcessAsUserW`, is an error in its
  own words. On macOS only a `launchctl` that exited 0 is a start.
- A start's own failure keeps its words as before (`locating own exe: …`,
  `nobody is signed in at this computer's screen`, `resolving the console user: …`,
  `launchctl kickstart gui/<uid>/com.roomler.desktop failed: …`). `ensure_running` logs either kind
  as `could not start the desktop companion`, and the prompt has no surface. FR-93 P6f's
  notification (§10.3) is Linux's alone; on Windows and macOS the next surface is the CLI.
- Unchanged: who the tray is started for (the console session's person on Windows, the console's
  owner on macOS), #3848's start and session choice, the launchctl domain, the callers and their
  3 s budget, and the 20 s verdict (`ENSURE_COOLDOWN`; since #3884 a failed start's verdict
  stands 2 s, §10.5).
- `one_in_flight` compiles on Windows for this; its per-key lookups (`OneInFlightPerKey`) stay
  Unix-only.
- Tested in `tray_start_tests` ([`companion.rs:2484`](../agents/roomlerd/src/companion.rs#L2484)), now one module for the three OSes: a shared harness
  and three tests that run on each OS against its own stood-in steps (`on_linux`, `on_windows`,
  `on_mac`): a held step fails the start within 2.5 s naming it, a 10 ms timer on a two-worker
  runtime stays on time while prompts ask, and asks at once and in a row share one start. Each OS
  keeps its control, `a_start_in_time_works_as_today` (the systemd-run argv, the start's context,
  path and arguments, the launchctl argv). Linux's tests keep their names. They run in "Windows
  agent unit tests" (the whole `--lib` suite), in the macOS job's "cargo check + test the agent's
  Mac-only units" (`companion::tray_start_tests::`), and on Linux as in §10.2. Field cells: the
  control on a per-Machine Windows lab guest and a lab Mac, and a start held there (a thread frozen
  at `CreateProcessAsUserW`, a stopped `opendirectoryd`).

### 10.5 The prompt's start: asked again after a failure, and off the WS loop (#3884, #3888)

Two things about the tray's start for a remote-control prompt (§10.2, §10.4) changed together.

**A failure stands 2 s, not 20 s (#3884).** `ensure_running`
([`companion.rs:1158`](../agents/roomlerd/src/companion.rs#L1158)) keeps its verdict so that a burst of
prompts does not become a burst of starts. Until #3884 every verdict stood 20 s, a failure
included. But a start that ran out of its 2.5 s goes on on its thread and may bring the tray up a
moment later, and a person may sign in at the screen meanwhile. The prompt the refusal invited was
answered "no surface" from the cache, with no look, for up to 20 s. Now each verdict stands for as
long as it can stay true (`EnsureVerdicts`, [`companion.rs:1221`](../agents/roomlerd/src/companion.rs#L1221)):

| Verdict | Stands | Why |
|---|---|---|
| the tray is up (`AlreadyRunning`, `Started`) | 20 s, `ENSURE_COOLDOWN` ([`companion.rs:1204`](../agents/roomlerd/src/companion.rs#L1204)) | a tray that started is visible within a second |
| no companion on this host (`Unsupported`) | 20 s | it changes only with an install, so no prompt in those 20 s starts anything |
| the start failed, or ran out of its wait | 2 s, `ENSURE_RETRY_AFTER_ERROR` ([`companion.rs:1215`](../agents/roomlerd/src/companion.rs#L1215)) | it can change at once: a late tray, a person who signed in. Still a rate limit: the prompts in those 2 s are answered from it with no start of their own, and one that asks while a start still hangs joins that start (`TRAY_STARTS`, one in flight), never a second |

A start that ran out of time answers 2.5 s after its prompt, and its verdict stands until 4.5 s,
so a prompt 5 s after that one asks afresh. A short hold was chosen over a look at whether the
tray runs before answering from the cache: on these hosts that look is logind and `pgrep`
(Linux) or the process snapshot (Windows), the very steps that hang, and it would not see a person
who signed in meanwhile, whose tray nobody has started yet. A failure's log line says when the
next prompt asks again: `could not start the desktop companion … asks_again_in=2s`.

**The start is off the WS loop (#3888).** The org's WS loop awaited the start inline, up to 3 s
(`COMPANION_START_BUDGET`, [`signaling.rs:341`](../agents/roomlerd/src/signaling.rs#L341)), and read
no other frame meanwhile: another viewer's request, a hangup, a live session's ICE. One person's
start held everyone else's signalling. The start now goes the way of the notification (§10.3):

```mermaid
sequenceDiagram
    participant L as WS loop
    participant S as the tray's start, a task
    participant T as consent task
    participant N as the notification's show (Linux)
    L->>L: arm the question, no native panel, write the marker
    L->>S: start_for_prompt: start it, not awaited (3 s at most)
    L->>T: spawn it, with the start's handle
    Note over L: reads its next frame
    S-->>T: the tray is up, or not (it failed, or ran out of time)
    T->>T: is the question still on the broker's books?
    T->>N: where the tray cannot show it: start the notification
    N-->>T: up, or why not
    T->>T: have_surface, and the surface line is logged
    T->>T: the prompt's window starts
```

| Piece | Where | What |
|---|---|---|
| the loop's side | `start_for_prompt`, [`companion.rs:1308`](../agents/roomlerd/src/companion.rs#L1308); the hand-off, [`signaling.rs:4550`](../agents/roomlerd/src/signaling.rs#L4550) | starts `ensure_running` on a task of its own, 3 s at most, and returns its handle (`Starting`, [`companion.rs:1326`](../agents/roomlerd/src/companion.rs#L1326)) at once. The loop hands it to the consent task with the question, in `RcSurfaces` ([`signaling.rs:7544`](../agents/roomlerd/src/signaling.rs#L7544)) |
| the consent task's side | `rc_surfaces_up`, [`signaling.rs:7570`](../agents/roomlerd/src/signaling.rs#L7570), called at [`:4632`](../agents/roomlerd/src/signaling.rs#L4632) | waits for the start's verdict, then for the notification where the tray cannot show the question, before its first look at the prompt starts the window. `have_surface` is the native panel, or the marker with the tray up, or a notification that came up, so a start that failed or ran out of time is still `no_prompt_surface`, never a timeout |
| a question that ends while the start runs | `ConsentBroker::is_asking`, [`consent.rs:853`](../agents/roomlerd/src/consent.rs#L853) | a `Terminate` the loop reads meanwhile takes the question off the broker's books and finds no notification to take down. The consent task starts none for a question off the books, or whose socket closed, and takes down one started just as it was withdrawn |

- **The bound is as before:** the start's 2.5 s and the 3 s budget, on the start's own task and
  the `tray-start` thread, never on an async worker. The question is still armed on the loop before
  anything shows it (#1632, #3442), and the window still starts after the surfaces are up.
- **The log:** `consent prompt surface` carries `companion` (`Some(true)`, `Some(false)`, or
  `None` when a native panel was up and no start was asked for) beside `native`, `notified` and
  `have_surface`.
- **Unchanged:** which surface is chosen and when, the question's words, the exec prompt (which
  never waited on the start), and a recording's and an SSH session's `Companion::up`, which keep
  their own bounded wait. Each benefits from the 2 s verdict all the same.
- **Tested** in `companion::tray_start_tests`
  ([`companion.rs:2484`](../agents/roomlerd/src/companion.rs#L2484)) against each OS's stood-in
  host (`Host`, [`:2768`](../agents/roomlerd/src/companion.rs#L2768), and `host_steps` in
  `on_linux`, `on_windows`, `on_mac`): a prompt a few seconds after a start that ran out of time
  finds the tray that start brought up
  ([`:2814`](../agents/roomlerd/src/companion.rs#L2814)); a failed start is asked again 2 s on,
  not per prompt, and finds a person who signed in
  ([`:2862`](../agents/roomlerd/src/companion.rs#L2862)); no companion, and a tray that is up,
  stand with no start per prompt (the control,
  [`:2891`](../agents/roomlerd/src/companion.rs#L2891)); while the start is held the loop handles a
  frame within 0.5 s ([`:2944`](../agents/roomlerd/src/companion.rs#L2944)); a start that failed
  or ran out of time is still `no_prompt_surface`, and one that worked is the person's timeout
  ([`:3012`](../agents/roomlerd/src/companion.rs#L3012)); and on Linux a question withdrawn while
  the start is held gets no notification
  ([`:3368`](../agents/roomlerd/src/companion.rs#L3368)). They run in rust-agent's "Test agent
  under vp9-444", in "Agent unit tests on Windows (the whole --lib suite)" and in the macOS job's
  "cargo check + test the agent's Mac-only units" (`companion::tray_start_tests::`).
- **Field cells:** a Linux system install whose tray start is held by a paused logind, where the
  first ask is `no_prompt_surface` and an ask 5 s later shows the question; two viewers asking at
  once while the start is held, the second request handled at once; and both on the release
  before, as the control.

### 10.6 A start for a prompt keeps the Welcome tour for the person (#3887)

The daemon starts the companion to show a consent question or a being-viewed banner (§10.2,
§10.4, §10.5). Until #3887 the companion could not tell that start from any other, and for a
person who had not finished or skipped the Welcome tour it opened the tour too: `decide_startup`
showed it "however this started". On a person's first remote session, the usual case, the tour
came up together with the question, and the question is the one with a deadline.

Now the start says what it is for, and the tour waits for the person's own start:

| The start | Made by | Its argument | The tour, while it is not done |
|---|---|---|---|
| for a question or a banner, Windows and Linux | the daemon's `start_tray`: Windows [`companion.rs:1902`](../agents/roomlerd/src/companion.rs#L1902), Linux [`:2326`](../agents/roomlerd/src/companion.rs#L2326) | `--for-prompt` (`FOR_PROMPT_ARG`, `crates/agent-core/src/companion_autostart.rs:36`) | held back: the companion stays in the tray, and the question's or the banner's own window shows as before |
| for a question, macOS | the daemon's `launchctl kickstart` ([`companion.rs:2040`](../agents/roomlerd/src/companion.rs#L2040)) | none: launchd's start carries none | the companion asks the daemon why it came (below), and holds the tour back when there is a question |
| at login | the `Run` value, the XDG entry, the LaunchAgent | `--autostart` (macOS: none) | shows |
| after install | `launch_once` | `--first-run` | shows |
| a person's own | a double-click, the Start or applications menu, Finder | none | shows |
| after an update; a tray the refresh lost (§13.3) | the daemon's update swap; its restart | none; `--autostart` | shows, as before |

```mermaid
flowchart TD
    S([the companion starts]) --> V{"--view=…?"}
    V -->|yes| SV[that view]
    V -->|no| P{"--for-prompt?<br/>(macOS: a bare start whose<br/>tour is not done asks the daemon)"}
    P -->|yes| T["the tray; the tour, not done,<br/>is held for the person"]
    P -->|no| D{tour done?}
    D -->|no| W[the Welcome tour]
    D -->|yes| R["as before: the tray, the Overview<br/>after install, or leave (login start, opted out)"]
    T --> O{"the person opens the window:<br/>a double-click, the tray icon,<br/>Device status"}
    O --> W2[the Welcome tour, once]
```

| Piece | Where | What |
|---|---|---|
| the decision | `decide_startup`, `agents/roomler-desktop/src/first_run.rs:75` | an explicit `--view=` first, then a start for a prompt stays in the tray, the tour done or not. It never leaves either: `Exit` stays for `--autostart` with the login-start switch off |
| the held tour | `holds_the_tour`, `first_run.rs:154`; set in `main.rs:87` | a start that stayed in the tray with the tour not done, which only a start for a prompt does. The person's own opening of the window shows the tour once: a second launch forwarded to the running companion (`decide_second_instance`, `first_run.rs:118`), a reopen on macOS (`decide_reopen`, `first_run.rs:264`; §10.7), the tray icon and its Device status (`tray_path`, `first_run.rs:188`, called at `tray.rs:83` and `:111`). The explicit items (Welcome tour…, Onboarding…, Check for Updates) are as before, and the daemon's next start for a prompt, forwarded, shows nothing |
| macOS | `asks_the_daemon`, `first_run.rs:200`, asked at `main.rs:83`; `daemon_has_something_to_show`, `:222`; `something_to_show`, `:208` | only a start with no argument at all, for a person whose tour is not done, asks; Windows and Linux are told. The answer is the LocalAPI's `consent_pending` and `rc_sessions`, read as the consent watch reads them, 300 ms for all of it: a question the daemon does not show natively, or a session the banner is for (`wants_banner`). No answer is no reason, and the tour shows as before. The process that answers is whoever serves the person's `$TMPDIR/roomler.sock`, the one the consent watch polls: the screen worker on a one-row Mac, the per-user agent on a two-row Mac. Since #3864 the screen worker's answer holds the root daemon's questions too (§10.8), so on a one-row Mac the root's question keeps the tour back. Since #3967 so does the per-user agent's on a two-row Mac (§10.8.1) |

- **Old and new together.** An older companion started with `--for-prompt` ignores it, as it
  ignores any argument it does not know, and shows the tour as before. A newer companion that
  an older daemon starts on Windows or Linux gets no argument and behaves as before. On macOS a
  newer companion asks an older daemon with verbs that daemon has, so its question keeps the
  tour back too.
- **Unchanged:** the start's 2.5 s and the callers' 3 s, the verdicts (§10.5), who the tray is
  started for, the post-install `--first-run`, the login starts, the update swap's restart and
  the `--autostart` restart of a tray the refresh lost, and the restart of a companion a Linux
  package update stopped (`companion/package_sync.rs`).
- **Every caller of the start is a question or a banner:** the remote-control prompt
  (`start_for_prompt`, `companion.rs:1308`, from `signaling.rs:4550`), the exec prompt
  (`exec_surfaces_up`, `signaling.rs:7675`), a recording's and an SSH session's question (`Companion::up`,
  `companion.rs:1370`), and Linux's being-viewed banner (`indicator/notify.rs:727`). The other
  callers are `tray_start_tests`' own.
- **Tested:** the companion's `first_run::tests` (`startup_table`, the forwarded launches, the
  held tour offered once, `--for-prompt` read by equality, which starts ask on a Mac and what
  the answer means) in "Rust unit tests (server + crates)", step "Test the desktop companion
  (roomler-desktop)"; the three start arguments distinct, none a prefix of another
  (`companion_autostart::tests`), in its "Run the remaining crates' unit tests"; and the
  daemon's argv in `a_start_in_time_works_as_today` (Linux's systemd-run argv, the Windows
  start's arguments), in rust-agent's "Test agent under vp9-444" and in "Agent unit tests on
  Windows (the whole --lib suite)".
- **Field cells:** a Windows per-machine install, a Linux system install, a one-row and a
  two-row Mac: a fresh install whose person has not seen the tour, with the companion not
  running. A remote session's consent start shows the question and no Welcome window; the
  person's own start, the tray icon and the next sign-in then show the tour. The control is the
  release before, where the tour opens together with the question.

### 10.7 Opening the app again while it runs: the second launch and the macOS reopen (#3941)

A person who opens the companion while it runs with its window closed expects the window back.
How that reaches the running companion depends on the platform:

| Platform | What opening the app again does | What the companion hears |
|---|---|---|
| Windows, Linux | starts a second process, which the single-instance plugin ends after it forwards its arguments to the running one | the plugin's callback (`agents/roomler-desktop/src/main.rs:107`): `a second launch` |
| macOS | starts nothing: AppKit hands the running app a **reopen** (`applicationShouldHandleReopen`). That is a double-click in Finder or Launchpad, Spotlight, or `open -a`. `LSUIElement` gives the app no Dock tile to click | `RunEvent::Reopen`, matched by `reopened` (`main.rs:285`) and handled by `reopen` (`:302`): `a reopen shows …` |

Until #3941 the companion had no handler for the reopen. It ran with `.run(context)`, whose
callback does nothing, so on a Mac the menu-bar icon was the only way back to the window.

```mermaid
flowchart LR
    W["Windows, Linux:<br/>a second process"] --> SI["the single-instance callback<br/>decide_second_instance(its arguments)<br/>log: a second launch"]
    M["macOS: Finder, Launchpad,<br/>Spotlight, open -a"] --> RO["RunEvent::Reopen → reopen<br/>decide_reopen: no argument<br/>log: a reopen shows …"]
    SI --> O["show_main_window → open_window<br/>route to the view, if any;<br/>show; focus"]
    RO --> O
```

| Piece | Where | What |
|---|---|---|
| the decision | `decide_reopen`, `agents/roomler-desktop/src/first_run.rs:264` | `decide_second_instance` with no argument: the window where the person left it (`Show`), or the Welcome tour while a start for a prompt holds it back and it is not done (§10.6). A reopen carries no argument, so it is never a login start or a start for a prompt, and never does nothing |
| the window | `show_main_window`, `agents/roomler-desktop/src/main.rs:264`; `open_window`, `first_run.rs:304` | the second launch's code, moved out of its callback, not copied. It routes the page to the view asked for, releasing a held tour as it shows it (offered once), then shows and focuses the window. `Show` leaves the page where the person left it, as a double-click does on Windows; the tray's icon and Device status route to the Overview instead (`tray_path`) |
| the line | `log_reopen`, `first_run.rs:283` | one line per reopen, beside `a second launch`: `a reopen shows the main window` or `a reopen shows the tour`, with `first_run_done`, `tour_held`, `has_visible_windows` and the decision. It is the lab's second witness beside the window |
| `has_visible_windows` | AppKit's answer, passed on by tao | logged, not obeyed: the being-viewed banner and the consent panel are windows too, so it can be true while the main window is closed |
| the run loop | `.build(…)` then `.run(…)`, `main.rs:247` | `.run(context)` was `build(context)` then `run` with a callback that does nothing (Tauri 2.11). The tray's and the menu's handlers, every plugin's `on_event` (the macOS single-instance socket's cleanup at exit) and the window bookkeeping still run before the callback. The callback never prevents an exit, and off macOS it matches nothing |

- **Unchanged:** the second launch on Windows and Linux (its decision, its line, what it shows),
  the tray, the close-to-hide, and exit requests.
- **Tested:** `first_run::tests`: a reopen with the window closed shows the main window
  (`a_reopen_shows_the_main_window`); the tour held back is shown once
  (`a_reopen_shows_the_held_tour_once`); the line names what was shown
  (`a_reopen_logs_what_it_shows`); and the moved window code does what the second launch did
  (`a_second_launch_opens_the_window_as_before`). They run in "Rust unit tests (server +
  crates)", step "Test the desktop companion (roomler-desktop)", and in "Windows overlay clippy",
  step "The desktop companion on Windows — clippy + unit tests (#3940)". The macOS match
  compiles in "macOS overlay compile check", step "cargo check the desktop companion". Nothing
  in CI sends a reopen.
- **Field cells:** a two-row and a one-row Mac, each with the companion running and its window
  closed. Opened from Finder and with `open -a`, the window shows and the log names it. While a
  start for a prompt holds the tour back, the reopen shows the tour once. The control is the
  release before, where a reopen shows nothing.

### 10.8 A one-row Mac's question reaches the menu-bar app (#3864)

On a one-row Mac (FR-43 P3) the root daemon holds the device's row, so it asks every
remote-control, exec and SSH question, on its own consent broker. That broker's folder is in
root's own home. The menu-bar app runs as the person at the console and asks over the LocalAPI.
The socket it reaches first is its own account's, `<TMPDIR>/roomler/roomler.sock`, and on a
one-row Mac the root daemon's screen worker serves it: `run --supervised --unenrolled`, run as
the person. Until #3864 the worker answered from its own broker. The root's native panel shows one
question at a time, so a question asked while it was busy went to the app, which never listed
it. The root still counted the app as the question's surface, so the viewer waited out the
window, and its end would have read as "nobody answered" (read in the 0.4.221 release lab).

Now the worker relays the root half's questions, and the root counts the app only while that
relay can work:

| Part | What it does | Where |
|---|---|---|
| the screen worker's list | its own questions, then the root half's, asked over the root daemon's person socket for this person, 1 s at most (`RELAY_WITHIN`). A root half that is slow or gone leaves the worker's own list, in time | `consent_pending_view`, [`localapi_state.rs:2229`](../agents/roomlerd/src/localapi_state.rs#L2229); `listed`, [`consent_relay.rs:115`](../agents/roomlerd/src/consent_relay.rs#L115) |
| the screen worker's answers | an answer to a question its own broker is not asking goes to the root half, 5 s at most (`DECIDE_WITHIN`). The root half's refusal comes back in its own words, and no answer is refused with a try-again | `consent_decide_view`, [`localapi_state.rs:2241`](../agents/roomlerd/src/localapi_state.rs#L2241); `decided`, [`consent_relay.rs:127`](../agents/roomlerd/src/consent_relay.rs#L127) |
| the worker's word | `relay-consent` in its caps frames, the first and the one it sends again when the person's recording gates change, said only by a worker with no enrollment. The daemon keeps it as a known word, and it goes with the channel | [`delegate_worker.rs:428`](../agents/roomlerd/src/delegate_worker.rs#L428), [`:611`](../agents/roomlerd/src/delegate_worker.rs#L611); `worker_features`, [`delegate.rs:233`](../agents/roomlerd/src/delegate.rs#L233); `relays_consent`, [`:1955`](../agents/roomlerd/src/delegate.rs#L1955) |
| the LocalAPI | a list and an answer are the daemon's views, `consent_pending_view` and `consent_decide_view`, on its own endpoint and on the person socket alike, after the same gates. Their defaults are the daemon's own lists | [`crates/localapi/src/lib.rs:4054`](../crates/localapi/src/lib.rs#L4054), [`:4153`](../crates/localapi/src/lib.rs#L4153), [`:4172`](../crates/localapi/src/lib.rs#L4172); the defaults, [`:3050`](../crates/localapi/src/lib.rs#L3050) and [`:3058`](../crates/localapi/src/lib.rs#L3058) |
| the root daemon's verdict | the app is a question's surface only while the road is open: a worker is attached that said `relay-consent`, its account is known, and the person socket for that account answers a list within 1 s. Asked fresh at every prompt, after the tray's own verdict and outside the start's 3 s budget, within its own 1 s (#3983, below) | `reaches_the_tray`, [`companion.rs:2371`](../agents/roomlerd/src/companion.rs#L2371), from `ensure_running` (`:1159`) and `up_for_a_question` (`:1284`); `reach_from`, [`consent_relay.rs:183`](../agents/roomlerd/src/consent_relay.rs#L183); the host is registered by `set_one_row`, [`delegate.rs:1288`](../agents/roomlerd/src/delegate.rs#L1288) |

```mermaid
sequenceDiagram
    participant V as viewer 2
    participant R as root daemon (the row)
    participant S as root's person socket for the person
    participant W as screen worker (as the person)
    participant A as menu-bar app (as the person)
    V->>R: rc:request, while the native panel shows viewer 1's
    R->>R: the question on its own broker, marked "companion"
    R->>R: the app started, if it was not running
    R->>S: the road: relay-consent said? a list within 1 s?
    S-->>R: its list: the road is open, the app is the surface
    A->>W: ConsentPending, on its own socket
    W->>S: ConsentPending, as the person
    S-->>W: the root's questions
    W-->>A: the worker's own, then the root's
    A->>W: ConsentDecide (Approve)
    W->>S: ConsentDecide, and the seat is asked again first
    S-->>W: ConsentDecided { ok: true }
    W-->>A: ConsentDecided { ok: true }
    R->>V: rc:consent granted
```

- **Who can read it.** Nothing new. The question crosses the root daemon's person socket,
  `/var/run/roomler-person/<uid>.sock` (FR-97 P2g). That socket is served for `/dev/console`'s owner alone
  ([`person_socket.rs:386`](../agents/roomlerd/src/person_socket.rs#L386)). It drops every connection whose kernel
  uid is neither that person's nor root's (`person_peer_ok`,
  [`crates/localapi/src/lib.rs:5062`](../crates/localapi/src/lib.rs#L5062), applied at `:5154`). It is 0600 and the
  person's, in a folder only root writes (`:5149`). An answer is taken only after the seat is asked again (#3845).
  The worker's own socket is 0600 in the person's private temporary folder. Nothing is written anywhere: the root's
  folder stays in root's home.
- **The floor is every host's.** With the road closed, the app is not the question's surface: no
  worker, a worker that did not say `relay-consent`, an account that is not known, or a person socket
  that does not answer in time (it is gone, `ROOMLERD_PERSON_SOCKET=0`, or a console switch is
  under way). The question then stands its window for `roomlerd consent`, and a window nobody answered
  ends as `no_prompt_surface`, not as the person's timeout. Nothing is shared without an answer.
  Telling the viewer at once is a question for every platform, and is not this fix's.
- **The start's budget and the road (#3983).** A question that waits for the companion before its
  window starts (a remote-control prompt's consent task, an SSH session's question, a recording's)
  gives the tray's start 3 s, `COMPANION_START_BUDGET`
  ([`signaling.rs:341`](../agents/roomlerd/src/signaling.rs#L341)). The road is asked after that
  budget, within its own 1 s (`RELAY_WITHIN`), not inside it: `up_for_a_question`,
  [`companion.rs:1280`](../agents/roomlerd/src/companion.rs#L1280), from `start_for_prompt`
  ([`:1308`](../agents/roomlerd/src/companion.rs#L1308)) and `Companion::up`
  ([`:1370`](../agents/roomlerd/src/companion.rs#L1370)). Until #3983 the 3 s held both. The
  start waits up to 2.5 s (`TRAY_START_WAIT`) and the road up to 1 s, so a slow first start and a
  person socket slow to answer ran out of the budget. The question then read as having no surface,
  though the tray came up and the road was open.

  | Host | The wait before the window starts (#3888) |
  |---|---|
  | a Mac's root daemon: one row, or a two-row Mac's root half | at most 4 s, the start's 3 s budget and then the road's 1 s. In practice 3.5 s, since a start that works answers within its own 2.5 s. Both are under the 5 s that the hub gives the agent's verdict past the window (`CONSENT_VERDICT_GRACE`, `crates/remote_control/src/consent.rs:149`), and a compile-time assertion keeps the budget and the road under it ([`companion.rs:1293`](../agents/roomlerd/src/companion.rs#L1293)) |
  | Linux, Windows, a Mac's per-user daemon | the start's alone, as before: the road is `Direct` there and answers at once |

  An exec question waits for neither: its surfaces go up beside its window, and `ensure_running`
  gives it the start and then the road, with no budget, as before.
- **One viewer, with the native panel up.** The app now receives that question too, marked
  `native`, and filters it out (`consent_watch_loop`,
  [`main.rs:338`](../agents/roomler-desktop/src/main.rs#L338); the prompt window,
  `panel-consent.js:191`), as a Windows SYSTEM install's tray does with a question its own panel
  shows. A Linux system daemon has no native panel, so its tray shows every question. There is
  one Approve on the screen, the panel's: the app shows no button for that question.
  Whatever ends the question takes the panel down (`hide_prompt`), and the app's next poll no
  longer lists it.
- **Ids.** Each question's id is an ObjectId: the server's session or request id for the root's
  remote-control and exec questions, and one the asking daemon minted for an SSH or a recording
  question. Two brokers never hold the same id, so a list holds each question once, and an answer
  reaches the broker that asked it.
- **A two-row Mac.** Its per-user half holds an enrollment and says no `relay-consent`. Since #3967 it
  relays the root half's questions too, and tells the root half so over the person socket (§10.8.1).
- **Old and new together.** The app is unchanged: the 0.4.221 companion shows a relayed question.
  The worker is the root daemon's own binary, started by it, and an older worker would say no word,
  so its daemon would not count the app.
- **Tested:**
  - the worker's relay, in `localapi_state::tests`: `a_one_row_workers_app_lists_and_decides_the_root_halfs_questions`,
    `the_worker_and_the_root_half_each_take_the_answers_to_their_own` and `a_gone_or_stalled_root_half_leaves_the_workers_own_list_in_time`.
    #3864's two-row control asserted the gap #3967 closed, and §10.8.1's tests replace it;
  - the word, in `delegate::tests::a_one_row_worker_says_relay_consent_and_the_daemon_keeps_it`;
  - the bounds and the road, in `consent_relay::tests`.

  These run in rust-agent's "Test agent under vp9-444" and in "Agent unit tests on Windows (the whole --lib suite)".
  The root's verdict is `companion::tray_start_tests::a_one_row_tray_is_the_surface_only_while_its_road_is_open`, which
  also runs in the macOS job's "cargo check + test the agent's Mac-only units". So does #3983's
  `companion::tray_start_tests::a_slow_start_and_then_a_slow_road_is_still_the_surface`: a start of about 2.1 s and
  a road of about 0.95 s, each within its own wait, are a surface on one row and on two, and a road that never
  answers after that start is no surface within the budget and the road's 1 s. The LocalAPI's views are
  `tests::the_consent_verbs_are_answered_by_the_daemons_views`, in "LocalAPI on Windows — clippy + unit tests",
  rust-unit's "Run the remaining crates' unit tests", and the macOS job's "cargo test the LocalAPI on a real Mac (#3736)".
- **Field cells:** a one-row Mac with two viewers, where viewer 2 asks while viewer 1's panel is up
  and the menu-bar app shows viewer 2's question; Approve and Deny each do what they say. A two-row
  Mac's cells are §10.8.1's.

#### 10.8.1 A two-row Mac's root-half question reaches the menu-bar app too (#3967)

On a two-row Mac (the default install, FR-43) the root half holds a row of its own, the `-daemon` row.
It asks that row's exec and SSH questions on its own consent broker. On a supervised two-row Mac it
asks that row's remote-control questions too, since a session's request is not handed to the worker
(`delegable_inbound`, [`delegate.rs:881`](../agents/roomlerd/src/delegate.rs#L881)). Its native panel
shows one question at a time, so a question asked while the panel is busy goes to the menu-bar app.

The app reaches the per-user half's socket. Until #3967 that socket listed only the per-user half's
own broker's questions, while the root half's verdict looked at no road and counted the app. A
question asked while the panel was busy reached nobody, and the viewer waited out the window. #3864's
PR named this gap.

Now the per-user half relays the root half's questions as a one-row Mac's worker does (§10.8), and
says so to the root half. The root half counts the app only while that announcement is live:

| Part | What it does | Where |
|---|---|---|
| the per-user half's list and answers | as §10.8's worker: its own questions, then the root half's, over the root half's person socket for this person, 1 s at most. An answer to an id its own broker is not asking goes to the root half, 5 s at most. Every Mac daemon that runs as a person relays: a one-row Mac's worker and a two-row Mac's per-user half | `relays_the_root_half`, [`localapi_state.rs:711`](../agents/roomlerd/src/localapi_state.rs#L711); `consent_pending_view`, [`:2229`](../agents/roomlerd/src/localapi_state.rs#L2229); `consent_decide_view`, [`:2241`](../agents/roomlerd/src/localapi_state.rs#L2241) |
| the announcement | a two-row Mac's per-user half (a Mac daemon that runs as a person and holds an enrollment) says `consent_relaying` over the root half's person socket for its person, then holds that connection. It dials again after the connection ends or a dial finds no socket: 1 s, doubling to 5 s while the root half is out of reach. A root half that refused, and whose connection then ended within 30 s (`HELD_STEADY`), is dialled on that backoff too: at an update the per-user half restarts about 3 s before the root half and meets the old one. Only a second such refusal in a row, with no take between them, waits a minute (`ANNOUNCE_AFTER_A_REFUSAL`, [`consent_relay.rs:404`](../agents/roomlerd/src/consent_relay.rs#L404); the dial's end, [`:450`](../agents/roomlerd/src/consent_relay.rs#L450)). Each change of state is logged once, never a retry | `keep_telling`, [`consent_relay.rs:428`](../agents/roomlerd/src/consent_relay.rs#L428); `tell_the_root_half`, [`:622`](../agents/roomlerd/src/consent_relay.rs#L622); started by `person_socket::run`, [`person_socket.rs:630`](../agents/roomlerd/src/person_socket.rs#L630), for `announces_consent_relay`, [`localapi_state.rs:2264`](../agents/roomlerd/src/localapi_state.rs#L2264) |
| what the root half keeps | the announcement, in memory, for as long as that connection stays open. It is taken only on the person socket and only from that socket's own person, never root. The connection's loop holds it, and it ends when the per-user half closes the connection (it stopped or restarted), or when the listener stops serving the connection (the seat changed, or the daemon is stopping) | `Request::ConsentRelaying`, [`crates/localapi/src/lib.rs:1460`](../crates/localapi/src/lib.rs#L1460); the dispatcher's arm, [`:4061`](../crates/localapi/src/lib.rs#L4061); `HeldForTheConnection`, [`:2966`](../crates/localapi/src/lib.rs#L2966); `Announcements`, [`consent_relay.rs:309`](../agents/roomlerd/src/consent_relay.rs#L309); `consent_relaying`, [`localapi_state.rs:2257`](../agents/roomlerd/src/localapi_state.rs#L2257) |
| the root half's verdict | the app is a question's surface only while an announcement is live for the person at the screen and that person's socket answers a list within 1 s. Asked fresh at every prompt, after the tray's own verdict and outside the start's 3 s budget, within its own 1 s (§10.8, #3983) | `reach`, [`consent_relay.rs:237`](../agents/roomlerd/src/consent_relay.rs#L237); `reach_two_rows`, [`:287`](../agents/roomlerd/src/consent_relay.rs#L287); `reaches_the_tray`, [`companion.rs:2371`](../agents/roomlerd/src/companion.rs#L2371) |

```mermaid
sequenceDiagram
    participant U as per-user half (as the person)
    participant S as root half's person socket for the person
    participant R as root half (the -daemon row)
    participant A as menu-bar app (as the person)
    participant V as caller (exec or SSH)
    U->>S: consent_relaying, on a connection it holds
    S->>R: taken, for as long as that connection stays open
    V->>R: a request, while the native panel shows another question
    R->>R: the question on its own broker, marked "companion"
    R->>R: the road: an announcement live? a list within 1 s?
    A->>U: ConsentPending, on its own socket
    U->>S: ConsentPending, as the person
    S-->>U: the root half's questions
    U-->>A: its own, then the root half's
    A->>U: ConsentDecide (Approve)
    U->>S: ConsentDecide, and the seat is asked again first
    S-->>U: ConsentDecided { ok: true }
    U-->>A: ConsentDecided { ok: true }
    R->>V: approved
```

- **Who can read it.** Nothing changes from §10.8. The question crosses only the root half's person
  socket, which serves the person at the console and root alone, and the per-user half's own socket,
  which is 0600 in the person's private temporary folder.
- **What the announcement is.** It reads nothing and grants nothing. It decides only whether the root
  half counts that person's app as its questions' surface. It is taken only over the person socket and
  only when the kernel's uid for the connection is the socket's own person; root is refused. Nothing is
  written anywhere: the root half keeps the announcement in memory, for the connection's life. The
  person socket holds an idle connection with no deadline (`serve_person_listener`,
  [`crates/localapi/src/lib.rs:5126`](../crates/localapi/src/lib.rs#L5126)), so a held one does not
  flap and needs no keepalive. It still ends when the seat moves, since the listener aborts every
  connection it holds.
- **The floor.** The app is not counted when no announcement is live, or when the person socket does
  not answer within 1 s. The question then stands its window for `roomlerd consent`:
  - a remote-control question ends `no_prompt_surface`;
  - an SSH question says "nobody could be asked at this device — it has no prompt surface …";
  - an exec question says the same, naming the execution policy (#3982). Until #3982 it ended "did
    not approve the command (Timeout)", since exec named no surface on any host.

  `ROOMLERD_PERSON_SOCKET=0` on the root half gives this floor, because no announcement can arrive.
- **Mixed versions.**
  - **A newer root half beside an older per-user half:** nothing announces, so the root half gives the
    floor.
  - **A newer per-user half beside an older root half:** the older root half's person socket admits
    `ConsentPending` and `ConsentDecide` (`person_may`), so its questions are listed and answered
    through the per-user half, and it counts the app as 0.4.224 did. It answers the announcement
    "unknown variant" and keeps the connection, logging nothing. The per-user half holds that
    connection and does not dial again until it ends. When it ends soon after (the older root half
    stops at an update), the next dial comes on the backoff, so the root half that replaces it is
    told within seconds.
- **Both halves at once.** Each question's id is an ObjectId: the server's session or request id for
  remote control and exec, or one the asking daemon minted for SSH and recording. So an id is asked by
  one half's broker only. The per-user half answers an id itself only while its own broker asks it,
  and sends every other id to the root half, which refuses an id nobody asks.
- **A seat change.** The root half's listener follows the seat every 5 s. The new person's per-user
  half dials at most 5 s apart while the socket is not there. So the new person's app is counted
  within about 10 s of the switch, and a question asked in that gap gets the floor.
- **The shapes.**
  - **Covered:** the default two-row Mac (the per-user half is a LaunchAgent), and a supervised
    two-row Mac, whose worker holds an enrollment and so relays and announces like the LaunchAgent.
  - **The floor:** an older per-user half; the person socket switched off; nobody at the console; no
    per-user half running for the person at the console.
  - **A correction, not a regression:** a Mac with a root half and no per-user half at all. Its app
    reaches no daemon there, because its sockets are its own temporary folder's and the system
    socket, which is root's alone. Its root half counted the app and now does not.
- **Logs.**
  - The root half logs at INFO when an announcement starts, `consent relay: a two-row Mac's per-user
    half lists this daemon's consent questions …`, with `uid`.
  - It logs at INFO when the announcement ends, `consent relay: the per-user half no longer lists
    this daemon's consent questions …`, with `uid` and `why`: the connection closed, or the person
    socket stopped serving it.
  - At a prompt whose road is closed, its verdict says `a two-row Mac's root-half question cannot
    reach the menu-bar app, which is not its surface (#3967) …`, with `why`.
  - The per-user half logs each change once: told, refused by an older root half, and the root
    half's person socket closed.
  - Neither half logs the app's 750 ms polls, nor the per-user half's dials to the root half's person
    socket.
- **Tested:**
  - the per-user half's relay and the root half's hold, in `localapi_state::tests`:
    - `a_two_row_per_user_halfs_app_lists_and_decides_the_root_halfs_questions`;
    - `a_two_row_per_user_halfs_own_questions_stay_its_own_brokers`;
    - `a_two_row_macs_halves_each_take_the_answers_to_their_own`;
    - `another_accounts_per_user_half_gets_no_root_questions`;
    - `a_gone_or_stalled_root_half_leaves_the_per_user_halfs_own_list_and_answer_in_time`;
    - `only_a_two_row_macs_per_user_half_tells_its_root_half`;
    - `a_two_row_root_half_takes_its_persons_announcement_while_the_connection_is_open`.
  - the announcer, the road and the registry, in `consent_relay::tests`:
    - `a_per_user_half_tells_its_root_half_and_tells_it_again_once_it_is_back`;
    - `a_root_half_that_refuses_or_never_answers_is_not_dialled_in_a_loop`;
    - `a_root_half_that_refused_and_then_stopped_is_dialled_again_on_the_backoff` (the update's
      order);
    - `the_two_row_road_is_open_only_while_its_person_has_announced_and_the_list_answers`;
    - `an_announcement_lives_as_long_as_its_connection`.

  These run in rust-agent's "Test agent under vp9-444" and in windows-agent-sessions' "Agent unit
  tests on Windows (the whole --lib suite)".
  - The root half's verdict is `companion::tray_start_tests::a_two_row_tray_is_the_surface_only_while_its_per_user_half_announces`.
    It runs in those two steps too, and in the macOS job's "cargo check + test the agent's Mac-only
    units".
  - The verb and its hold are `tests::a_relay_announcement_is_held_for_its_connections_life` in
    `roomler-localapi`. That runs in "LocalAPI on Windows — clippy + unit tests", in rust-unit's "Run
    the remaining crates' unit tests", and in the macOS job's "cargo test the LocalAPI on a real Mac
    (#3736)".
  - The Mac-only dial and the per-user half's start are compiled by the macOS job's `cargo check -p
    roomlerd --lib --bins`.
- **Field cells**, all on a two-row Mac:
  - a root-half exec or SSH question asked while the root half's panel shows another question, which
    the app shows;
  - Approve and Deny;
  - both halves asking at once;
  - a newer root half beside an older per-user half, which gives the floor;
  - `ROOMLERD_PERSON_SOCKET=0` on the root half, which gives the floor;
  - a seat change;
  - ten quiet minutes with the app running.

  A one-row Mac is the control.

### 10.9 A click on the prompt is acted on, or logged (#3873)

Read in the 0.4.221 release lab on a Linux system install: the daemon started the tray for a
question, the prompt window showed it, and two clicks on Approve, 6 s and about 3 min after it
showed, did nothing. Neither the tray's log nor the daemon's journal has a line at either click,
and the question timed out. When the second click came, the window still counted down the
question's own deadline, so the page was alive and showed that question.

No step of a click said anything, and none had a bound:

| Step | Until #3873 |
|---|---|
| the page's `decide()` | left early, without a word, while an earlier answer was "on its way" or after the page had let go of the question |
| the page's words | "Approving…" and "Failed: …" went into the countdown, which repaints every second |
| the app's `consent_decide` | no line on any outcome |
| the LocalAPI reply | read with no deadline (`Client::request`, [`crates/localapi/src/lib.rs:5922`](../crates/localapi/src/lib.rs#L5922)) |
| the page's read of the questions | a failure read as "no question": the page let go of the question while it stayed on screen |

The answer never reached the daemon: 0.4.221 answers a `ConsentDecide` at once, and logs a
refusal or the grant, and its journal has neither. The likeliest path is that the first click's
answer never came back, so the page stayed "sending" and dropped the second click. Whether the
first click reached the app cannot be told from those logs, because nothing logged a click's
arrival. The earlier tray's WebKit processes, still running when this tray started, are not shown
to be the cause: a later tray approved at once beside three of them.

Now a click is acted on, or the person reads why not, and the tray's log has it either way:

```mermaid
sequenceDiagram
    participant P as the person
    participant W as prompt window (panel-consent.js)
    participant A as the app (answer_click)
    participant D as roomlerd
    P->>W: Approve
    W->>W: buttons off, "Approving…" on the status line
    W->>A: cmd_consent_approve(the question shown, or none, and how long it has been shown)
    alt its question came to the front under 1 s ago (§10.9.1)
        A->>A: log WARN "consent: a click arrived too soon …"
        A-->>W: "Nothing was sent: …", and nothing goes to the daemon
    else in front for 1 s or more
        A->>A: log "consent: a click arrived"
        A->>D: ConsentDecide, 10 s for connect and reply
        D-->>A: taken, refused in its words, or nothing
        A->>A: log the outcome
        A-->>W: "approved", or the reason
    end
    W->>P: "Approved.", or the reason, until the next click or question
    Note over W: no reply from the app in 15 s:<br/>the page's own limit, the buttons come back
```

| Piece | Where | What |
|---|---|---|
| the click, in the log | `answer_click`, [`agents/roomler-desktop/src/commands.rs:2634`](../agents/roomler-desktop/src/commands.rs#L2634) | `consent: a click arrived` with the verb and the question's id, then `consent: the device service took the answer`, or a WARN with the reason: `consent: the answer was not taken` (the daemon's own words, `connect: …`, or no such question any more) or `consent: no answer from the device service in time`. A click with no question on the page is logged, and nothing is sent (the commands take `Option<String>`). Since #3987 a click whose question came to the front under 1 s ago is logged as one WARN, `consent: a click arrived too soon after the question came to the front; nothing was sent`, and nothing is sent (§10.9.1) |
| the bound | `CONSENT_ANSWER_WITHIN`, [`commands.rs:2598`](../agents/roomler-desktop/src/commands.rs#L2598): 10 s | the connect and the daemon's reply. Above the daemon's own bounds on an answer: who is at the seat (1.5 s, #3796) and a Mac's relay (5 s, §10.8, §10.8.1) |
| the page | `decide`, [`panel-consent.js:138`](../agents/roomler-desktop/src/front/panel-consent.js#L138); `paintButtons`, [`:71`](../agents/roomler-desktop/src/front/panel-consent.js#L71); `PAGE_LIMIT_MS`, [`:54`](../agents/roomler-desktop/src/front/panel-consent.js#L54); the status line, `panel-consent.html:103` | the click is sent before anything else can stop it, with how long its question has been shown (`shown`, #3987). The buttons are off only while no question is shown, an answer is on its way, or this window answered the question, and the status line says which. It is an element of its own (`p-status`), so the countdown never overwrites it, and its words stay until the next click or question. The page's own 15 s limit fires only when the app never replies, and the buttons then come back. Escape acts as the Deny button does |
| the read | `cmd_get_pending_consents` and `pending_answer`, [`commands.rs:2726`](../agents/roomler-desktop/src/commands.rs#L2726) | a failed read is an error, not an empty list. The page keeps the question it shows and clears it only on an empty list. The failure is in the log through the refresh ledger (`refresh failed surface="consent_pending"`, once a streak; §3); a daemon that is not running is quiet |
| the fallback modal | `decide`, [`consent.js:70`](../agents/roomler-desktop/src/front/consent.js#L70) | the same: the click is sent first, the buttons are off while an answer is on its way, the same 15 s limit, and an answered question is not shown again |

- **The WARN at every start** (`refresh failed surface="encoder_caps" … person-at-the-device
  socket of a system install`, in the same lab) is gone since #3891 (0.4.223): the person socket
  serves `encoder_caps`, and an older daemon's refusal reads `root_only` with no WARN (§8).
- **Unchanged:** what the daemon decides and how (`consent_decide_view`, #3845's seat check),
  the prompt's window and placement (both changed later, in #3987: §10.9.1), the watch that shows
  and hides it (`consent_watch_loop`), and the words a viewer reads.
- **Tested** in rust-unit's "Test the desktop companion (roomler-desktop)" and in "The desktop
  companion on Windows — clippy + unit tests (#3940)": `a_click_whose_answer_never_comes_ends_at_the_bound`,
  `every_click_is_logged_when_it_arrives_and_with_its_outcome`, `a_click_with_no_question_is_logged_and_sends_nothing`,
  `a_failed_read_of_the_questions_is_an_error_and_in_the_log`, and the pages' own pins:
  `the_prompt_sends_every_click_and_says_why_its_buttons_are_off`, `the_prompt_keeps_its_question_on_a_failed_read`,
  `the_fallback_modal_sends_every_click_too`.
- **Field cells:** a Linux system install; a tray the daemon starts for a question about 40 s
  after the earlier tray was stopped; Approve, then Deny on the next question. Each click has its
  arrival and outcome lines in the tray's log and the outcome on the window's status line. A click
  with the daemon stopped reads its reason within 10 s, and the buttons come back. The control is
  the release before, with no line at the click.

#### 10.9.1 Where the prompt opens, and a click as it appears (#3987)

Read in the 0.4.224 lab on a Mac (the default two-row install, macOS 15 on Apple Silicon). Two
viewers asked for the same device about 20 s apart. The daemon's native panel showed the first
question and the app's prompt the second. While both stood, the window list had the panel,
480×240 at (272, 48) on layer 1000, and the prompt, 460×260 at (282, 48) on layer 5. The panel
covered the prompt except for a strip about 20 pt high at its bottom edge. Deny on the panel
closed the panel. One click on the prompt's Approve 55 s later left no line in `desktop.log` and
no decision in the daemon's log. In the next run, a click on the prompt window and then one on
Approve granted the session.

| | Until #3987 | Since |
|---|---|---|
| Where the prompt opens | on macOS and Windows, the top centre, 48 down: where the daemon's own panel is. On a Mac that panel is at the screen-saver level and covers the prompt. On Windows the prompt, shown later and focused, covers the overlay. Linux has opened it in the middle since #2286 (§12.2) | the middle of the main screen on every OS. Where the middle comes within 16 of the daemon's panel, 16 below the panel instead (`consent_origin`, `panels.rs:701`) |
| When it is placed | when its window is built | each time it comes up from hidden. A prompt that is already up stays where it is (`placed_on_show`, `panels.rs:717`) |
| A click while it is not the key window | AppKit spent the click on making the window key, so the page never saw it and nothing logged it. The prompt is key when shown, and stops being key at the person's next click anywhere else, such as on the daemon's panel | acted on: both panels take their first click (`takes_first_click`, `panels.rs:428`) |
| A click as it appears | held back by the rule above, which also held back a deliberate click 55 s later | held back for 1 s after its question came to the front, logged with the reason, and the page says why (`CONSENT_ARMING`, `panels.rs:727`) |

**The place.** Every daemon panel fits in one box: the top centre of the main screen, 48 down,
at most 480 × 240 logical.

| Daemon panel | Size | Where |
|---|---|---|
| macOS | 480 × 240 pt | `agents/roomlerd/src/indicator/mac.rs:72`, `:477` |
| X11 | 480 × 236 at the desktop's scale | `agents/roomlerd/src/indicator/x11.rs:67`, `:299` |
| Windows | 460 × 232 physical px, no larger in logical units | `agents/roomlerd/src/indicator/win.rs:135`, `:611` |

The prompt opens in the middle of the main screen. Where the middle comes within 16 of that
box, the prompt opens 16 below it instead, at y 304. On a screen too short for that, the
prompt's bottom edge sits on the screen's bottom edge, so its buttons stay in view. On any
screen from 564 to 867 tall the prompt therefore opens at y 304, and 1024 × 768, as wide as the
lab's Mac, is one of them.
`the_daemons_panels_fit_the_box_the_prompt_keeps_clear_of` reads the three panels' sizes and
places from the daemon's sources, so a panel that moves or grows fails a test before it reaches
anyone's screen.

```mermaid
flowchart TD
    S[the app shows the prompt] --> V{already up?}
    V -- yes --> K[it stays where it is]
    V -- no, or not known --> M[the middle of the main screen]
    M --> N{within 16 of the daemon's panel?<br/>the top centre, 48 down, 480 × 240}
    N -- no --> P[it opens there]
    N -- yes --> B[16 below the panel, at y 304]
    B --> C{does its bottom fit on the screen?}
    C -- yes --> P2[it opens there]
    C -- no --> E[its bottom on the screen's bottom edge,<br/>so the buttons stay in view]
```

**The click.** Both panels take their first click. Neither lets a first click grant anything
without the arming below. The banner's buttons only end a session or a recording. The prompt
answers through `cmd_consent_approve` and `cmd_consent_deny` alone, and both pass through
`answer_click`. A click, or Escape (the Deny button's key), is held back while its question has
been in front of the person for less than `CONSENT_ARMING`, 1 s. A click that early may have been
aimed at whatever was under the pointer before the question came up: one already on its way when
the question appeared (a reaction takes about a quarter of a second), or the second half of a
double click (0.5 s by default on macOS and Windows). Nobody reads a question and answers it
faster than that. The click is still logged when it arrives:
`consent: a click arrived too soon after the question came to the front; nothing was sent`,
with the verb, the question's id and how long after the question came up the click came
(`after`). The page's status line reads
`Nothing was sent: this click came 0.3 s after the question came up …`. The buttons come back at
once, and the next click is sent.

What starts that second again, and what does not:

| Event | Arms again | Where |
|---|---|---|
| the app shows the prompt: built, or up from hidden | yes | `show`, `panels.rs:314` |
| the app raises a prompt that is up, for a question its watch has not seen | yes | `show` |
| the app raises the main window for the fallback modal (#2815) | yes | `show` |
| a new question replaces the one under the pointer | yes | `shownAt`, set in `render` (`panel-consent.js`) and `show` (`consent.js`) |
| a poll that changes nothing, the page's or the watch's | no | the page puts a question up only once the one it shows is gone. The watch raises the prompt only for a question it has not seen (`consent_watch_loop`, `agents/roomler-desktop/src/main.rs:361`) |

Each click sends `shown`, the time in ms that the page has shown its question. The app counts
from the later of that and its own last show or raise of the window (`in_front_for`,
`panels.rs:751`).

**Key.** The prompt still becomes the key window when it is shown, as before. Keys typed into
another app after it appears go to the prompt and are dropped. No button has focus, so typing
cannot approve. An Escape in the prompt's first second is held back like a click; after that,
Escape denies.

**#3973 (GNOME on Xwayland: the prompt at the work area's top-left).** The code shows a likely
path, not a proven one. Before #3987, a prompt shown again was not placed again, and GTK forgets
a hidden window's position, so the window manager placed it as a new window. The tray's watch
hides the prompt when a read of the questions fails and shows it again on the next good read
(`consent_watch_loop`, `agents/roomler-desktop/src/main.rs:326-365`). The prompt is now placed
each time it comes up from hidden. Whether that is all of #3973 is for the lab to read.

- **Tested** in rust-unit's "Test the desktop companion (roomler-desktop)" and in "The desktop
  companion on Windows — clippy + unit tests (#3940)":
  `the_prompt_opens_clear_of_the_daemons_panel_on_the_labs_mac`,
  `the_prompt_opens_in_the_middle_where_that_is_clear_of_the_panel`,
  `on_a_short_screen_the_prompts_buttons_stay_on_it`,
  `the_prompt_never_meets_the_panel_on_a_screen_tall_enough_for_both`,
  `the_daemons_panels_fit_the_box_the_prompt_keeps_clear_of`,
  `a_prompt_already_up_stays_where_it_is_and_a_hidden_one_is_placed_again`,
  `every_panel_takes_its_first_click`, `a_click_within_the_arming_time_is_not_taken`,
  `a_new_question_under_the_pointer_arms_the_prompt_again`,
  `a_click_too_soon_after_the_question_came_up_is_logged_and_not_sent`,
  `a_poll_that_changes_nothing_does_not_arm_the_question_again` and
  `the_pages_shown_time_is_read_only_when_it_is_a_time`. The macOS arms (`show_without_focus`,
  the capture exclusion, the frame) compile in "macOS overlay compile check", step "cargo check
  the desktop companion"; no test runs there.
- **Field cells:** a two-row and a one-row Mac with two questions pending: the first is answered
  on the panel, then ONE click on the second's Approve in the app connects the session. A click
  as the prompt appears is logged and not acted on, and so is an Escape. On Windows with the
  overlay holding question 1, the app's prompt for question 2 opens in the middle, clear of the
  overlay. On GNOME (Xwayland), the lab reads where the prompt opens after a failed read
  (#3973; read only).

## 11. Testing it

- **Unit:** `cargo test -p roomler-desktop` (the Rust side), `cargo test -p roomler-localapi
  --lib` (the pipe pool and backoff, including the back-to-back-open test that fails with 231
  on the single-instance server), `cargo test -p roomlerd --lib route_reconciler`.
- **In CI, on all three OSes** (`ci.yml`): Linux runs the unit tests in "Rust unit tests
  (server + crates)", step "Test the desktop companion (roomler-desktop)". Windows runs clippy
  (`--all-targets -D warnings`) and the unit tests in "Windows overlay clippy", step "The
  desktop companion on Windows — clippy + unit tests (#3940)". macOS `cargo check`s it in
  "macOS overlay compile check", step "cargo check the desktop companion". Each OS compiles
  only its own `#[cfg]` arms, so a mistake in Windows-only code (the login start in the Run
  key, the service start's spawn, the panels' capture exclusion) fails the Windows step alone.
  Before #3940 no lane compiled that code, and the release build found such mistakes first.
- **The front without a daemon:** serve `src/front/` over loopback with a fake
  `window.__TAURI__` that answers the commands from fixtures, and drive it in a browser.
  That is how the Routes page (stale-while-error, the sequence guard, stable rows, edit) and
  the grouped Settings were verified before they shipped.
- **The install matrix:** vmtest's `desktop` check asserts every view renders on fresh
  Windows and Linux installs.

⚠️ A browser tab driven by automation can be a **background** tab (`visibilityState:
"hidden"`): timers are throttled to once a minute after a while and synthetic mouse presses
are dropped. Script clicks through the page (`element.click()`) or run timing checks right
after a reload.

## 12. The session banner and the red frame (FR-27 P9)

On macOS and Linux the companion IS the "Being viewed by …" indicator (Windows draws its own,
in the daemon, `indicator/win.rs`). P9 gives it the Windows badge's manners:

⚠️ **Only where the companion can reach its daemon.** On a Linux **system** install the
daemon's own LocalAPI is root-only (`/var/run/roomler`, 0700, and the socket 0600), and the
companion runs as the desktop user. Until FR-27 P11 it read "Service offline". There was no
banner, no Disconnect, and on Wayland no companion consent prompt (measured 2026-10-09;
#1911).

**P11 fixes it** without widening that ACL. The root daemon also serves a **person socket**
at the person's own `/run/user/<uid>/roomler.sock`, which is the path the companion already
tries first:
- it is theirs, mode 0600, and only they or root may connect (`SO_PEERCRED`);
- it follows the person signed in at the machine's own screen: logind's active session on
  `seat0`, when that session is a person's (`companion::seat0_uid`). Never a remote login,
  which sits on no seat although logind calls it active (#2871's review: the first active
  graphical session anywhere, which the socket followed before, could be an xrdp user's).
  Before it applies a config change, and since #3845 before it takes a consent answer, it
  asks again whether that person is still at the seat ([security baseline](security-baseline.md)
  §4). A look that cannot tell who is there keeps the socket where it is (§12.3);
- it serves the safety surface: status, the sessions viewing the screen, their Disconnect,
  consent, the recording state and keep busy (FR-92); and, since FR-97 P2g, the Devices page
  and the mesh, read-only (§6); since P2h, the peers too, without the endpoints of either end
  or of a relay, the carrier snapshot, the path decision or the probes (`localapi::person_view`);
  since FR-85 P2e, recording: the Recordings view's start, stop, list and delete, and the
  **Stop recording** on the banner and the tray; and since #2871 (design A) the
  configuration — the listing, with every key the socket will not set marked `read_only`,
  and `ConfigSet` of exactly the keys that are the person's own call
  (`localapi::CONSOLE_PERSON_KEYS`, §5), never the device-owned gates; and since #3831 what
  this device can encode, the Overview's card (§8): the probe's cached summary, read-only.

Everything else on it — a config key that is not the person's own included, refused by name
with the `sudo` command — is refused with a pointer to `sudo roomler …`, so the companion's
other views show that refusal on a system install. The kill switch is
`ROOMLERD_PERSON_SOCKET=0`.

A Mac's root half serves its console user one as well (FR-97 P2g), at
`/var/run/roomler-person/<uid>.sock` in a directory only root writes: there the per-user half
is the person's own daemon already, and what it asks the root half for is the machine's
private-network view (§6).

⚠️ **A session the compositor says nothing about starts the companion** (FR-93 P6g). On
Wayland the desktop's own screen-sharing indicator (GNOME's pill, KDE's tray icon) shows for
the portal and mutter arms, but the DRM arm reads the scanout below the compositor, so neither
appears. A session that needs no prompt also never started the companion, so nothing at the
screen said anyone was watching. Now each pump, once its capture opens on DRM, starts the
companion (`companion::ensure_running`), whose banner lists every live session. Where no
companion can run, the daemon posts a notification instead: *"This screen is being viewed —
Alice is viewing this screen and can use its keyboard and mouse."*, with **Disconnect**, which
ends the session as the banner's does. The notification stays on screen (critical urgency),
comes down with the session, and stays down for that session once the person closes it.
The kill switch is `ROOMLERD_VIEWING_NOTICE=0`.
A per-user install, and a supervised Mac through its worker (FR-43), never had this problem.

⚠️ **The banner waits while the desktop's own dialog asks** (FR-93 P6h). A per-user Wayland
session that the portal must ask about (KDE's *Remote Control*, a portal's *Share screen*) is
registered the moment it connects, and the banner used to come up then. It said "Being viewed"
while nothing was shared yet. On a desktop that centres new windows, it also sat right over the
dialog the person had to answer. Measured on Plasma 6.6, its **Disconnect** sat beside the
dialog's **Approve**. Now the capture open's wait hook marks its session `asking` while that
dialog is the wait (`agents/roomlerd/src/peer.rs:5887`, `AskingMark`
`agents/roomlerd/src/rc_sessions.rs:43`). The LocalAPI row carries the mark (`asking`,
`crates/localapi/src/lib.rs:1370`), and the banner comes up only for a session that sees the
screen (`wants_banner`, `agents/roomler-desktop/src/main.rs:389`). The mark goes
when the open does, whether the dialog was approved, denied or timed out, so a dialog nobody
answers still ends with the banner. The kill switch is `ROOMLERD_BANNER_WAITS_FOR_DIALOG=0`.

⚠️ **On a system install this banner is what lets a Wayland desktop be captured by itself**
(FR-95 P2, [`linux-capture.md`](linux-capture.md) §1.2.1). With no capture key set, a root
daemon switches DRM and `uinput` on for the person's session on screen only where that
person can be told: the companion runs for them, or their session's notification server
shows buttons. That is checked live, at every capture open (`indicator::notify::can_tell`).
An installed companion that is not running does not count on its own. Without either,
nothing switches on, and the viewer's hint says so. `ROOMLERD_VIEWING_NOTICE=0` therefore
also keeps a system install's Wayland capture opt-in.

| | Behaviour | Where |
|---|---|---|
| Shown | when a session starts, for at least `FIRST_SHOW` (4 s), **without taking focus** | `panels.rs:127`, `:434` |
| Hides | 2.5 s after the pointer leaves it (`HIDE_DELAY`) | `panels.rs:54` |
| Comes back | after the pointer rests within 4 px of the top edge for 1.2 s (`TOP_ZONE`, `REVEAL_DWELL`) — the Windows badge's numbers | `panels.rs:50-52` |
| Stays | while a recording runs, this device's or a controller's (`tray::recording_now`) | `tray.rs:382` |
| Moves | by dragging its body (`data-tauri-drag-region`, unchanged) | `front/panel-viewing.html` |
| Frame | a 2 px red frame round the main screen for the whole session — four click-through strips (macOS) | `panels.rs:531` |

```mermaid
stateDiagram-v2
    [*] --> Shown: a session starts
    Shown --> Hidden: pointer away 2.5 s<br/>(after the 4 s first show,<br/>nothing recording)
    Hidden --> Shown: pointer rests at the top 1.2 s
    Hidden --> Shown: a recording starts
    Shown --> [*]: the session ends (banner and frame go)
```

The decision is a pure state machine (`Reveal::step`, `panels.rs:274`) fed every 120 ms by a
watch thread; its six unit tests are the spec.

⚠️ **On macOS `show()` makes a window KEY**, whatever `focused(false)` said when it was built.
Every session's banner therefore took key status from the window the person was using, and
their next click into it was spent making it key again and did nothing else (measured
2026-10-06: the first click into roomler-desktop after each new session was lost). The banner
is now brought up with `orderFrontRegardless` (`show_without_focus`, `panels.rs:450`), the way
the native consent panel shows itself.

⚠️ **The pointer cannot always be read** (a Wayland session keeps it to itself, and under
Xwayland it is not trusted, §12.2). Then the banner never hides: one that tucked itself away
with no way back would be worse than one that stays.

⚠️ **Both the banner and the frame appear in the captured stream on macOS.** Our capture is
`CGDisplayStream`, which ignores `NSWindowSharingNone`; the flag is set anyway and becomes
true the day capture moves to ScreenCaptureKit (FR-27's deviations). The controller sees a red
frame round a Mac's screen during a session; that was the operator's call (2026-10-06).

Kill switches, each on its own: `ROOMLER_DESKTOP_BANNER_AUTOHIDE=0` (the banner stays, as
before P9) and `ROOMLER_DESKTOP_FRAME=0` (no frame).

### 12.1 The banner's Disconnect (FR-27 P10)

The banner's **Disconnect** ends every live session (`panel-viewing.js` sends one
LocalAPI `RcDisconnect` per session, `cmd_rc_disconnect`) — and since P10 it ends them
**for good**: control ends at the click (no more input, clipboard, files or frames),
the daemon tells the server `host_disconnect` before it closes the peer,
the viewer is told "The person at the device ended the session." and stays
disconnected, and coming back is an explicit Reconnect on the viewer's side, through
consent like any new session. Before P10 the viewer read the closed peer as an ordinary
drop and was back within ~3 s of being sent away (measured on this banner, 0.4.119,
2026-10-08). The mechanism, the ordering it relies on and the version matrix are in
`docs/remote-control.md` §11.5; the companion itself did not change — the fix is in the
daemon's arm that every Disconnect surface reaches, so the Windows badge gets it too. Over the
daemon's own endpoint the Disconnect is taken only from the person at the screen the daemon
shows, SYSTEM or an elevated administrator (§12.4); the badge's never crosses the pipe.

On a supervised Mac the session lives in the GUI worker (FR-43), and so does the
Disconnect: the worker's `host_disconnect` rides the delegation link to the daemon's
WS, the way the session's answer did.

### 12.2 Pinned to the top on Wayland (#2286)

`position_top_centre` (`panels.rs:473`) asks for the top centre of the primary monitor.
Windows, macOS and X11 grant it. **A Wayland compositor ignores it**, because a client
cannot place its own window there. Measured on Plasma 6.6 and GNOME (2026-10-16), both put
the banner in the middle of the screen for the whole session. That is where the controller
was working, and on Linux the banner is in the captured stream too. Each desktop now gets
the one placement it allows a client (`agents/roomler-desktop/src/pin.rs`):

| Desktop | How the banner gets to the top | Where |
|---|---|---|
| **KDE Plasma** (Wayland) | A KWin script, loaded into the running KWin over D-Bus (`org.kde.kwin.Scripting`). It finds the banner by its title (`BANNER_TITLE`), puts it at the top centre of its screen, 48 px down, and keeps it above everything and out of the task switchers. It places the banner again each time it maps, because a hidden banner comes back as a new window. Nothing is written to the person's KWin configuration, and the script goes when that KWin does. | `pin.rs:120`, the script `pin.rs:82` |
| **GNOME** (Wayland) | mutter offers no placement, no layer-shell and no scripting. The companion runs under **Xwayland** instead (`GDK_BACKEND=x11`), where an X11 window places itself and stays above. It is set as `main`'s first statement: GTK reads it when it starts, and an environment variable may only be set while no other thread exists. | `pin.rs:63`, `agents/roomler-desktop/src/main.rs:53` |
| X11, Windows, macOS | Unchanged: the window places itself. | `panels.rs:473` |

```mermaid
flowchart LR
    A[the banner is shown] --> D{the session}
    D -- X11 · Windows · macOS --> T[it places itself<br/>position_top_centre]
    D -- KDE on Wayland --> K[a KWin script, over D-Bus,<br/>places it and keeps it above]
    D -- GNOME on Wayland --> X[the companion runs under Xwayland,<br/>so it places itself]
```

⚠️ **Under Xwayland the banner never tucks itself away.** Its pointer is known only over X11
windows. At GNOME's top bar it reads wherever it last was, so the top-edge gesture that brings
a hidden banner back would never be seen. The pointer is treated as unreadable there
(`Sample::read`, `panels.rs:220`), and an unreadable pointer keeps the banner up (§12).

⚠️ **A non-resizable GTK window takes its webview's natural height.** The banner asked for
76 px and came out 198 px tall, the rest of it empty. On Linux the panels are resizable with
min = max, which is the same fixed size without that (`panels.rs:339-345`).

⚠️ **The consent window goes in the middle, not at the top**: on Linux since #2286, and on
every OS since #3987, which also keeps it 16 below the daemon's panel where the middle meets it
(§10.9.1). The daemon's own X11 consent panel holds the top centre
(`agents/roomlerd/src/indicator/x11.rs`). With the companion on Xwayland, a consent window
placed there too landed right under it when both were asking. `WebviewWindow::center` put the
window's corner at the middle under Xwayland at 2×, half a window off, so the place is computed
the way the top centre is (`position_consent`, `panels.rs:401`).

Measured on Fedora Asahi Remix 42 (an Apple M2 Pro Mac mini), 2026-10-16:

| Session | The banner | Its Disconnect | The consent window |
|---|---|---|---|
| Plasma 6.6 (KWin 6.6.4) | 360×76 at the top centre, above the other windows | ended the session (`host_disconnect`) | — |
| GNOME, at 2× | `720x152+1688+96` (360×76 logical, 48 down), still there after its first show | ended the session | centred, clear of the daemon's panel; **Approve** on each granted its own session |

The kill switch is `ROOMLER_DESKTOP_BANNER_PIN=0`. It loads no KWin script, and on GNOME the
companion stays on native Wayland.

### 12.3 Who is at the seat: not known is not nobody (#3845)

On a Linux system install, who is at the seat is logind's answer. logind is another process,
and its answer may not come: a paused logind is one case, and `loginctl`'s D-Bus call gives
up after about 25 s. The person socket's poll used to read a look that failed as "nobody". It
closed the person's socket (`person socket: nobody at the seat now; closing`), and the
person's own `roomler` commands then fell through to the root-only socket:
`Permission denied (os error 13)`. Measured on 0.4.220 with logind paused for 45 s, the close
came 26.3 s into the pause. A look now has three answers (`person_socket::Look`,
`agents/roomlerd/src/person_socket.rs:135`, from `look_of`, `:426`):

| The look found | When | The poll (`step`, `:151`) | A change or a consent answer over the socket |
|---|---|---|---|
| a person | seat0's active session is a person's (`companion::seat0_uid`) | starts, keeps or moves the socket to them | taken from that person, refused in words from anyone else |
| nobody | the seat itself says so: an empty seat, the login screen, a session that is not a person's, a host with no logind (`companion::is_nobody_at_the_screen`, the rule a received file follows, §9.4); root at the seat; no person logged in at all | closes the socket, as before: `person socket: nobody at the seat now; closing` | refused in words |
| not known | any other failure: logind or the bus did not answer, `loginctl` failed, no answer within the look's wait, a lookup that panicked | keeps what it serves, with no close, start or move. It says so once a run, `person socket: who is at the seat is not known (<why>); serving as before`, and once as the run ends, `person socket: who is at the seat is known again` | refused with "Could not tell who is signed in at this computer's screen. Try again." (`AtTheSeat::NotKnownYet`) |

```mermaid
flowchart TD
    P["the poll, every 5 s"] --> L{"look at seat0<br/>one lookup in flight, 5 s at most"}
    L -- "a person" --> F["start, keep or move<br/>the socket to them"]
    L -- "nobody: the seat says so,<br/>or root" --> S["close the socket"]
    L -- "not known: no answer in time,<br/>loginctl failed" --> K["keep what is served<br/>(said once a run)"]
    C["a ConfigSet or a ConsentDecide<br/>over the socket"] --> Q{"at_the_seat_now<br/>1.5 s at most"}
    Q -- "this person" --> Y["applied"]
    Q -- "someone else, nobody" --> N["refused in words"]
    Q -- "not known" --> T["refused: try again"]
```

- **Failing closed stays.** Nothing changes through the socket while it is not known. Every
  `ConfigSet` and every `ConsentDecide` asks the seat again first (`at_the_seat_now`, `:450`,
  1.5 s at most), and a look that cannot tell refuses with the try-again text, however it came
  about (`seat_for`, `:504`). A failed lookup used to read "the seat changed". The person now
  gets the try-again text instead of a closed socket, and the banner, its Disconnect and keep
  busy keep working.
- **The poll's look** goes through the lookup in flight the checks already share,
  `PERSON_AT_SEAT0` on a thread named `seat0-person` (`:463`, `look_at_the_seat`, `:470`), and
  waits 5 s at most (`LOOK_WAIT`, `:93`, the time between looks). It used to wait on a blocking
  thread for as long as `loginctl` took. A hung logind now holds that one thread, however
  often the poll and the checks ask. The wait is longer than a check's 1.5 s on purpose. A
  look cut short is not known and keeps the socket where it is, so the poll waits long enough
  to follow a slow logind that still answers.
- **A consent answer comes from whoever is at the seat now.** A consent answer lets someone
  see and control this screen. So the socket asks the seat again before it takes one, an
  approval or a denial, as it does before a change (`crates/localapi/src/lib.rs:4149`). It
  used to take the answer from whoever held the socket. A person who had switched away could
  answer for the next person's screen until the next look, up to 5 s, and while who was at the
  seat was not known, for as long as that lasted. Root, and the daemon's own socket, are not
  asked: that socket takes an answer only from the person at the screen the daemon shows,
  SYSTEM or an elevated administrator (§12.4).
- **What still rides on the poll.** A recording's start has its own identity rule (#3830): the
  recorder's console lookup (`recording::launch::decide`) refuses with the same try-again text
  when it cannot tell, and the recorder runs as the person at the screen. Disconnect
  (`RcDisconnect`) and keep busy only end a session or keep the screen awake. They stay with
  the person the socket was made for until the poll moves it.
- **A seat that changes hands while not known** moves only once logind answers, in one move and
  never by way of "nobody". It is logind that switches the seat, so a switch it has not answered
  for is rare.

The tests: `person_socket::following_the_seat_tests` drives the real poll, a real person socket
and the real check, with who is at the seat stood in (`:1032`, Linux, as the runner in CI's
agent lane). `person_socket::tests::a_look_that_cannot_tell_keeps_what_is_served` and
`only_the_seats_own_answer_is_nobody` hold the pure rules, and `crates/localapi`'s
`a_consent_answer_over_the_person_socket_comes_from_whoever_is_at_the_seat_now` holds the
dispatcher's (both on every OS).

### 12.4 Who answers over the daemon's own endpoint

A consent answer, an approval or a denial, decides whether someone sees and controls this
screen, and a Disconnect ends what they see. Both are the call of the person at that screen.
The person socket asks the seat (§12.3). The daemon's own endpoint, the Windows pipe that
admits every interactive user (§1) or a Unix daemon's owner-only socket, takes `ConsentDecide`
and `RcDisconnect` only from these callers, as the OS read them when they connected:

| Caller | Read from | Answer |
|---|---|---|
| a process in the session whose screen the daemon shows | the client's session, read at accept (`pipe_client_peer`) | taken |
| SYSTEM (`S-1-5-18`) | the client's token | taken |
| an administrator who elevated | the client's token (its elevation) | taken |
| a Unix daemon's own account, or root | the kernel (`SO_PEERCRED`, `getpeereid`) | taken |
| another signed-in account; LocalService or NetworkService; a peer whose session and account could not be read | — | refused in words, with a warn line; nothing reaches the daemon |

On Windows the screen's session depends on what serves the pipe (`screen_session`,
`crates/localapi/src/lib.rs:2627`):

| The daemon serving the pipe | The screen's session |
|---|---|
| a worker, which the service host starts in the active console session | its own session, the console's |
| a per-user daemon, which runs where its person signed in, over RDP too | its own session. The person at the console is someone else, and is refused |
| a daemon in session 0, the service host | the console session (`WTSGetActiveConsoleSessionId`) |
| its own session unreadable, or session 0 with no console session | none: only SYSTEM and an elevated administrator are taken |

```mermaid
flowchart TD
    R["ConsentDecide or RcDisconnect<br/>on the daemon's own endpoint"] --> S{"SYSTEM, or elevated?<br/>the client's token"}
    S -- yes --> Y["taken"]
    S -- no --> W{"a Windows session<br/>was read?"}
    W -- yes --> Q{"the screen's session?<br/>screen_session"}
    Q -- yes --> Y
    Q -- no --> N["refused in words<br/>nothing decided"]
    W -- no --> U{"the daemon's own uid,<br/>or root?"}
    U -- yes --> Y
    U -- no --> N
```

| Piece | Where | What |
|---|---|---|
| the decision | `peer_may_decide_for_the_screen`, `crates/localapi/src/lib.rs:2654` | pure: the peer, the console session, the daemon's own session and uid. SYSTEM is named by its SID, because `ClientPeer::is_root` reads a uid, which a pipe client never has |
| the daemon's own session | `own_session_id`, `:2658` | `ProcessIdToSessionId` of this process; `None` off Windows |
| the dispatch | `:4028` (consent), `:4033` (Disconnect) | after the person socket's seat check (`:4009`), on every scope but the person socket's, so a new scope starts refused |
| the refusals | `not_the_screens_person`, `:3785` | "The answer was not taken: …" and "The session was not ended: …", which the tray and the CLI show as they arrive |

Who calls them, and how each is read:

| Caller | Runs | Taken as |
|---|---|---|
| the tray's Approve and Deny (`cmd_consent_approve`, `agents/roomler-desktop/src/commands.rs:2549`) and the banner's Disconnect (`cmd_rc_disconnect`, `:2789`) | as the person, in their session | the screen's session |
| `roomlerd consent --session <id> --approve` and `roomler rc --disconnect <id>` (`consent_cmd`, `agents/roomlerd/src/app.rs:2507`; `rc_cmd`, `:2404`) from the person's own shell | the same | the screen's session, and from an elevated prompt as an elevated administrator too |
| the native consent panel and the Windows badge's Disconnect | in the daemon | never on the pipe |

The tests, in `crates/localapi`: `the_screen_decision_table` (every row above, a per-user daemon
in an RDP session among them) and
`a_consent_answer_or_a_disconnect_on_the_daemons_own_endpoint_comes_from_the_screens_person`
(the dispatch: approve, deny and Disconnect, nothing reaching the daemon when refused, and the
person socket unchanged) run on every OS. On Windows,
`a_client_at_the_screen_answers_and_ends_a_session_over_a_real_pipe` reads a client from the OS
over a real pipe, as the tray and the CLI are read.

#### The console person's gates follow the same screen

The daemon's own endpoint keeps a second set of gates, for the console person: a console
person's `ConfigSet` key (`CONSOLE_PERSON_KEYS`: `rc_clipboard`, `files_dir`, `lock_on_end`, the
`record_*` and `external_*` keys and the rest), the recording verbs, keep busy on, and the
external-access password's set and clear. On Windows each takes the person in the session whose
screen the daemon shows (`screen_session`, above), read at accept as a consent answer's caller
is. Each gate's other conditions are as they were:

| Gate | Where | Taken on Windows | Also, as before |
|---|---|---|---|
| the external-access password's set and clear (`is_console_user`) | `peer_is_console_user`, `crates/localapi/src/lib.rs:2552`; the dispatch `:4180` | a peer in the screen's session | Unix: the daemon's own uid, or root |
| a console person's key | `peer_may_set_console_key`, `:2582`; the dispatch `:3981` | the same | the person socket: its person, and the seat asked again (§12.3) |
| the recording verbs: start, stop, delete | `peer_may_record`, `:2516`; the dispatch `:4081` | the same | the person socket: its person |
| keep busy on | `peer_may_start_keep_busy`, `:2483`; the dispatch `:4077` | a peer in the screen's session whose token reads as a person's account | never root, SYSTEM, LocalService, NetworkService or an account that could not be read; stopping is open to every peer |

| The daemon serving the pipe | Who passes these gates on Windows |
|---|---|
| a worker of a machine-wide install, in the active console session | the person at the console: the screen's session is the console session, so these gates decide as they always did |
| a per-user daemon in an RDP session | the person in that RDP session; the person at the console is refused |
| a daemon in session 0 | the person at the console |
| its own session unreadable, or session 0 with no console session | no Windows peer |

Unlike a consent answer, these gates take no SYSTEM or elevated caller from outside the screen's
session, as before, and Unix reads no session, so there each gate decides by the uid.

Each refusal names the person these gates take (#3907): "only the person at this device's screen
can …", in its gate's own words (`not_the_console_user`, `not_the_console_user_for_external_access`
and `not_the_person_for_keep_busy`, `crates/localapi/src/lib.rs:3335-3363`, and each
`CONSOLE_PERSON_KEYS` entry's `refusal`). On a machine-wide install the screen is the console's,
so they read as before. On a per-user install used over Remote Desktop the person at the console
is the one refused, whom the words of 0.4.222 and earlier ("only the person at this device's
console …") named.

The tests, in `crates/localapi`, run on every OS:
`the_console_persons_gates_follow_the_screens_session` (a table per gate: a per-user daemon in
an RDP session, a worker in the console session, a daemon in session 0 and nothing known, with
each gate's other conditions and Unix's uid rows),
`on_a_machine_wide_install_the_console_persons_gates_decide_as_before` (a worker in the console
session, or a daemon in session 0, decides as the console-session rule did, for every peer and
both scopes) and `the_console_persons_verbs_follow_the_screen_the_daemon_shows` (the dispatcher,
with the two session reads pinned for the test's thread: every verb from the screen's person
reaches the daemon, from any other session none does, and stopping keep busy stays open). On
Windows, `a_client_at_the_screen_sets_a_console_persons_key_over_a_real_pipe` reads a client from
the OS over a real pipe, as the tray is read. One test a gate pins its refusal's words, asked as
the person at the console of a per-user daemon in an RDP session:
`a_recording_verbs_refusal_names_the_person_at_the_screen`,
`a_keep_busy_starts_refusal_names_the_person_at_the_screen`,
`the_external_access_passwords_refusal_names_the_person_at_the_screen` and
`a_console_persons_keys_refusal_names_the_person_at_the_screen` (every `CONSOLE_PERSON_KEYS`
entry).

## 13. One version: the companion follows the daemon (FR-117)

After an update a device runs one version: the daemon (`roomlerd`), the CLI (`roomler`) and the
companion (`roomler-desktop`). Each OS packages the companion differently, so each keeps it in
step differently:

| OS | How the companion ships | What moves it with the daemon |
|---|---|---|
| Windows | a standalone EXE beside the daemon, in neither MSI | `refresh_if_stale` (`agents/roomlerd/src/companion.rs:4720`): at the start of the process that owns it (#1686), and from FR-117 P2 again every 4 h (`refresh_periodically`, `companion.rs:93`), within minutes while the tray is too old to connect (§13.2), and within a minute while a tray it stopped is not running again (§13.3). It goes by the EXE's own version when the marker does not name the daemon's (§13.4), and starts the tray again in each session it stopped one in (§13.5) |
| macOS | inside the daemon's `.pkg` (`/Applications/Roomler.app`) | the same `.pkg`: one install moves both |
| Linux | its own `.deb`, or `.rpm` (§10.1) | FR-117 P1: the root update owner moves the package (`companion/package_sync.rs`) |

The CLI needs nothing of its own: it ships inside every daemon package (the MSI's `roomler.exe`
shim, the `.pkg`, the `.deb`, and the tarball, where `usr/bin/roomler` is a required member).

**Where the release is looked up (#3818).** Windows and Linux both move the companion to the
daemon's OWN release, `agent-v<own>`. Both look that release up in the update feed on roomler.ai:
`GET /api/agent/latest-release`, or `ROOMLERD_UPDATE_URL`, the list the daemon's update check
reads. They take the exact tag from that list (`fetch_release_by_tag`,
[`agents/roomlerd/src/updater.rs:1695`](../agents/roomlerd/src/updater.rs#L1695); where each
lookup asks is `release_sources`, `:1674`). They never ask GitHub's API. Until #3818 they asked it
by tag, unauthenticated, from every device at every release. Behind a corporate network's shared
egress address, GitHub's per-address limit answered `403 Forbidden` during the agent-v0.4.218 push,
and the tray stayed a release behind until its next try, 4 h later.

| Case | What happens |
|---|---|
| the feed lists the tag | the companion's asset comes from that release. Its bytes come from the asset's URL in the list, through `download_asset`'s checks, as the daemon's own update does |
| roomler.ai cannot answer (unreachable, a 5xx, a 429) | the refresh logs `desktop companion refresh skipped` with the feed's error, and the retry tries again (§13.2). The Linux sync tries again at the updater's next tick. Neither asks GitHub |
| the feed lacks the tag | the same skip, logged as `release agent-v<own> is not in the update feed …`. The feed carries GitHub's newest page of releases (48 agent releases on 2026-11-13), so the cause is a daemon further behind than that, or a build that was never released |

**Why Linux needed it.** The companion's package is installed once, by `install.sh` or the wizard,
from a release asset. There is no apt or dnf repository, so nothing ever upgraded it. **Update
all** moved the daemon and left the companion behind, and the grid showed the skew: a Fedora
Asahi box read `v0.4.147 · desktop v0.4.139` on 2026-10-20. `refresh_if_stale` had been
Windows-only on the false premise that apt upgrades the companion.

```mermaid
flowchart TD
    S([the root update owner:<br/>at its start, every check,<br/>an operator's push]) --> K{ROOMLERD_COMPANION_SYNC=0?}
    K -->|yes| N0[nothing]
    K -->|no| Q{roomler-desktop<br/>installed? dpkg / rpm}
    Q -->|no| N1[nothing: headless stays headless]
    Q -->|yes| V{its version vs<br/>this daemon's}
    V -->|equal| N2[nothing]
    V -->|newer| N3[logged once, never a downgrade]
    V -->|older| F[release agent-v OWN, from the update feed:<br/>the companion for this arch,<br/>.deb for dpkg, .rpm for rpm]
    F --> D[download_asset: digest, the pinned key,<br/>the version binding]
    D --> I[the host's package manager,<br/>needrestart suspended]
    I --> P[stop the companions on the replaced<br/>image; start one in the session<br/>where one was stopped]
```

- **Who:** the process that installs updates as root on this host, so a person is never asked for
  a password. That is a system install's root daemon, or a per-user install's root update
  helper (FR-96). A per-user daemon that sees the skew wakes its helper. Mechanics:
  [linux-self-update.md](linux-self-update.md#the-desktop-companion-follows-the-daemon-fr-117).
- **What it reads:** `installed_package` (`companion.rs:1509`) asks dpkg, then rpm. ⚠️ dpkg's
  answer counts only for an INSTALLED package (`dpkg_installed_version`,
  `companion/package_sync.rs:56`). `dpkg-query -W` also answers, with a version, for one that was
  removed with its config kept, and a companion a person removed must stay removed.
- **Never a downgrade, never a new install** (`plan`, `package_sync.rs:79`). A companion newer
  than the daemon is someone's deliberate install, and the next daemon update catches up. A host
  with no companion stays without one.
- ⚠️ **The picker is a positive match** on `roomler-desktop-…-unknown-linux-gnu.<deb|rpm>`
  (`pick_asset`, `package_sync.rs:103`), the mirror of the daemon picker's `is_daemon_asset`. So
  the two can never take each other's package (§10.1's freeze, one axis over).
- **A running companion is restarted** (`restart_replaced`, `package_sync.rs:412`). The package
  manager renames the new binary over the old, so a running companion's `/proc/<pid>/exe` reads
  `… (deleted)` (`is_replaced_companion`). Those are stopped, and a new one starts in the
  graphical session where one was stopped, through FR-27's `systemd_run_command`, as the Windows
  swap does (#1686). A companion that was not running stays stopped.
- **The grid catches up at once:** the sync drops `installed_version`'s 10-minute cache
  (`forget_installed_version`, `companion.rs:1446`).
- **Kill switch:** `ROOMLERD_COMPANION_SYNC=0` turns off FR-117's two additions, the Linux sync and
  the Windows retry (§13.2's faster one with it). Windows' refresh at start is older and stays.

### 13.1 Checking for updates: one click from the tray (#2936)

The tray's **Check for Updates** and the Overview's **Updates** card run one routine. A tray click
shows the window at once and hands the rest to the page:

```mermaid
sequenceDiagram
    participant T as tray.rs check_updates
    participant P as overview.js
    participant C as cmd_check_update
    T->>T: show and focus the main window
    T->>P: raise the pending flag, fire roomler-tray-check-update
    P->>P: clear the flag, show "Checking for updates…" in the card
    P->>P: switch to Overview, scroll the card into view (instant), ring it once
    P->>C: invoke (off the main thread)
    Note over P: the page becomes visible: scroll again
    C-->>P: what roomlerd self-update --check-only said
    P->>P: after at least 0.7 s, the answer and "Checked at 21:45:25."
    P->>P: "Install update" when one is available, scroll again
```

| Piece | Where | Why |
|---|---|---|
| the tray's half | `check_updates`, `src/tray.rs:591` | it used to run the check on the menu thread and show the window only afterwards, at the top of Overview. So: seconds of nothing, then a result in the last card, below the fold, and people scrolled down and clicked again |
| the event and the flag | `tray_check_update_js`, `src/tray.rs:577` | the flag covers a page that is not listening yet. A test pins the event name on both sides (`the_tray_update_check_fires_what_the_overview_listens_for`) |
| one check | `runUpdateCheck`, `src/front/overview.js:184` | the card's button and the tray share it: a stale "Install update" is hidden first, the button is disabled while it runs, and a failure lands in the card |
| a check you can see | `MIN_CHECKING_MS`, `src/front/overview.js:173` | "Checking for updates…" stays up at least 0.7 s, and the answer carries the time it was asked ("Checked at 21:45:25."). On 0.4.177 an answer that came back at once read as no check at all (field, a Mac, 2026-10-26) |
| into view | `scrollToUpdates`, `src/front/overview.js:214`; `revealUpdates`, `:234` | the Updates card is the last one on Overview, and `#content` is what scrolls. The scroll is INSTANT and repeated: once the page is visible, and when the answer lands. On 0.4.177 a smooth scroll never happened on a Mac: it is driven by animation frames, and the tray runs it while the window is still coming up |
| the ring | `flashUpdates`, `src/front/overview.js:220`; `.card.flash` in `styles.css` | draws the eye to the card. A static outline instead when the OS asks for reduced motion |
| off the main thread | `cmd_check_update`, `agents/roomler-desktop/src/commands.rs:2083` | a synchronous Tauri command runs on the app's main thread, and this one waits for a spawned `roomlerd` and a network round-trip |

⚠️ **Installing stays a separate click.** The menu item says "Check for Updates", and on macOS
**Install update** queues the root update helper (FR-27).

### 13.2 A tray too old for its daemon (Windows, #3781)

From `agent-v0.4.217` the LocalAPI pipe gives an interactive user only the open a tray makes from
`agent-v0.4.216` on: reading and writing data, and nothing that adds an instance of the pipe
(#3742, #3760; the DACL is §1's trust-boundary note). A tray from before 0.4.216 opens the pipe
with `GENERIC_WRITE`. Every connect it makes is refused, and none of its pages reach the daemon.
That holds where the daemon runs elevated or as SYSTEM: a machine-wide install, unless its worker
runs as a person without administrator rights. A daemon that runs as the person owns its pipe (a
per-user install, or such a worker), so that person's old tray still connects, through the
owner's entry.

The refresh (§13's table) normally replaces the tray within seconds of the daemon's update. On
2026-11-12 every online Windows device had its tray refreshed within seconds of its 0.4.216 update.
This section is for a device whose refresh keeps failing, such as a corporate laptop whose
endpoint protection blocks the swap. Before #3760 that device kept a stale tray that still worked;
now it keeps one that cannot connect. So while the tray is that old, the daemon retries the
refresh within minutes, and `roomler status` says what is wrong.

```mermaid
flowchart TD
    W([the retry loop:<br/>one plan per wait]) --> K{ROOMLERD_COMPANION_SYNC=0?}
    K -->|yes| N[4 h, no refresh, no line]
    K -->|no| M{the tray's version: its marker,<br/>or the EXE's own when the marker<br/>does not name this daemon's, §13.4}
    M -->|0.4.216 or later| R[4 h]
    M -->|older, or no version<br/>either can name| F[2, 5, 10, 15, 15, 15 min,<br/>then 4 h]
    F --> L[one warn line before the wait:<br/>installed, needs, next_try_in_min]
```

| Piece | Where | What |
|---|---|---|
| the floor | `MIN_COMPANION_VERSION`, `agents/roomlerd/src/companion.rs:148`; `predates_min_companion`, `:154` | `0.4.216`, the first release whose tray opens the pipe exactly (`agent-v0.4.215` was cut before #3742). It moves only with the pipe's DACL or the client's open. A marker that is missing, unreadable or names no version counts as older, unless the EXE's own version resource names one (#3850, §13.4): nothing else vouches for the open such a tray makes |
| the schedule | `next_refresh_wait`, `companion.rs:491`; `FAST_REFRESH_WAITS`, `:122`; `RefreshSchedule::next`, `:463` | keyed on the version marker. While it reads too old the waits are 2, 5, 10 and 15 minutes, then 15 to the end of the first hour, so the tries land 2, 7, 17, 32, 47 and 62 minutes after the loop starts; after that the updater's 4 h. A marker that reads current starts them over. While a tray the refresh stopped is not running again, the waits are 1, 5 and 15 minutes when those are shorter (§13.3) |
| the loop | `retry_refresh`, `companion.rs:513` | one plan per wait, so one line per wait while the tray is too old: `desktop companion is too old to connect to this daemon; its refresh is retried`, with `installed`, `needs` and `next_try_in_min`. It runs in the one refresher per install (#1686): on a machine-wide install the service host, whose log is `roomler logs --source service`. The version it plans from is read off the async worker (`sibling_version`, §13.4) |
| what status is told | `too_old_to_connect`, `companion.rs:570`; `too_old_here`, `:663`; `too_old_beside`, `:694` | `NodeStatus.companion_too_old` (`crates/localapi/src/lib.rs:496`), read fresh on every answer, because the service host, another process, may have just refreshed the tray. The tray's version is the one the refresh goes by (§13.4, #3908): the marker when it names the daemon's, else the EXE's own. Absent means fine or not applicable, never unknown: a tray whose version neither names is reported without one, and a token or a file the daemon cannot read counts as reportable |
| the log the hint names | `newest_service_log`, `crates/agent-core/src/logging.rs:992` | `--source service` reads the service host's `roomlerd-service.log` alone, the newest of its daily files (#3814). It used to take whichever was newer of that and the SystemContext worker's `roomlerd.log` in the same `service-logs` folder. The busier worker's nearly always was, so the hint's own command showed none of the warn lines. The worker's log is `--source daemon`, since the worker answers the verb |
| whether the retry is off | `retry_off_where_it_runs`, `companion.rs:600`; `sync_enabled_in`, `:623`; `service_sync_enabled`, `:641` | `companion_too_old.retry_off` (#3814): `ROOMLERD_COMPANION_SYNC=0` is set where the retry runs. A daemon that runs the retry itself reads its own environment. So does the SystemContext worker: the service host starts it with the host's own environment. A worker the host starts as the person gets the person's environment, which leaves the service's `Environment` value out, so it reads that value: once a process, because a restart of the service starts a new worker. A value it cannot read, or one that does not name the switch, leaves the retry on |
| what status says | `tray_lines`, `agents/roomler-cli/src/localclient.rs:2499` | the lines below |

`roomler status` on a machine-wide install whose tray is 0.4.212:

```
  tray        OUT OF DATE: 0.4.212 cannot connect to this daemon, which needs 0.4.216 or later
              Roomler replaces it by itself, retrying every few minutes for an hour after a start, then every 4 h
              To retry now: Restart-Service Roomler (as an administrator)
              Why it failed: roomler logs --source service --grep "desktop companion"
```

- A tray whose marker is missing, or names no version, reads `version UNKNOWN (no version
  marker): one older than 0.4.216 cannot connect to this daemon`.
- A daemon that refreshes the tray itself, not the service (an elevated `roomlerd run`), names
  `restart roomlerd` and its own log instead.
- With `ROOMLERD_COMPANION_SYNC=0` set where the refresh runs, the second line reads
  `Roomler replaces it only at a start: its automatic retry is off (ROOMLERD_COMPANION_SYNC=0)`
  (#3814). The refresh at a start stays on with the switch, so the other three lines stand.
- A worker that runs as the person reads the switch from the service's `Environment` value,
  where it is documented to go. A machine-wide system variable `ROOMLERD_COMPANION_SYNC=0` also
  turns the host's retry off, but such a worker's hint keeps the retry's wording: the safe
  direction.
- The fields are additive: an older daemon sends none, an older CLI ignores them, and
  `status --json` carries them as `companion_too_old`, with `retry_off` only when it is true.
- **Kill switch:** `ROOMLERD_COMPANION_SYNC=0` turns the whole retry off, the faster one with it,
  and `status` says so.

### 13.3 The tray is stopped last, and a tray the refresh lost comes back (Windows, #3824)

The refresh replaces `roomler-desktop.exe` beside the daemon, and the person's tray usually runs
from it. Until #3824 the refresh stopped the tray first and only then renamed the EXE aside. In the
agent-v0.4.218 release lab that rename was refused (a deny for SYSTEM on the EXE; in the field a
scanner or endpoint protection can refuse it the same way). The start of the old tray then failed
as well, and the person had no tray while the log said `respawned it: true`. A later refresh that
did swap the EXE started no tray, because none was running.

From `agent-v0.4.221` the tray is stopped last, once the new EXE is in place:

```mermaid
flowchart TD
    S([a refresh pass whose marker is stale]) --> G[stage: copy the downloaded tray<br/>in as .exe.new]
    G --> A[rename the installed EXE aside,<br/>to .exe.old or the first free .exe.old.N,<br/>while the tray still runs from it]
    A --> P[rename .exe.new into place]
    P --> R[stop the running trays,<br/>found by the path they started from]
    R --> V{did every one stop?}
    V -->|no| B[the old EXE back over the new one,<br/>start what stopped, marker unchanged:<br/>the retry tries again]
    V -->|yes| W[write the marker, start what stopped,<br/>remove the copies moved aside]
    G -.->|fails| K[put back what moved:<br/>nothing stopped, the tray runs on,<br/>the retry tries again]
    A -.->|fails| K
    P -.->|fails| K
```

| Piece | Where | Why |
|---|---|---|
| the order | `swap`, `agents/roomlerd/src/companion.rs:5058`, over `SwapOps`, `:4923`; on disk `FsSwap`, `:5456` | NTFS renames a running image aside, while it refuses a delete of that file or a replace onto it (`a_running_program_is_found_by_its_started_from_path_and_stopped_by_pid`, `agents/roomlerd/src/win_service/companion_procs.rs:496`). So the rename aside and the promote both run while the tray runs, and the tray is stopped only after both worked. What refuses the rename is an ACL, a handle opened without `FILE_SHARE_DELETE` (a scanner's, #3005's err 32) or endpoint protection: each now leaves the tray running, and the retry tries again |
| a tray that would not stop | `swap`'s branch, `companion.rs:5093` | it keeps the single-instance lock, so a new tray could not take over. The old EXE goes back over the new one, which nothing runs, what was stopped is started again, and the marker stays as it was, so the retry tries again |
| a name for the copy | `free_aside_name`, `companion.rs:5428`; `ASIDE_NAMES`, `:5420`; the sweep, `FsSwap::remove_old`, `:5537`, with `is_aside_name`, `:5444` | a copy an earlier swap left that is still mapped (a tray that would not stop) or held (a scanner) refuses a delete and a replace onto it alike, and used to fail the next swap's rename. The copy now takes the first free name of `.exe.old`, `.exe.old.1` … `.exe.old.8`, and a swap that lands removes every one it can. The sweep takes nothing else: never the marker (`.exe.version`) or the staged `.exe.new` |
| the line | `refresh_line`, `companion.rs:5294`, logged by `log_refresh`, `:4836` | one renderer, tested: "respawned" only after a start that worked, and a start that failed is a warning with its error. The prefixes are the ones the lab kits read |
| the consent prompt's start | `started_outcome`, `companion.rs:1928` | FR-27's `ensure_running` was told the tray started even when its start had failed, so the prompt waited out its window for nobody. It now reports `no_prompt_surface` then |

The refresh's lines, message first, then its fields:

```
INFO desktop companion refreshed version="0.4.221" stopped=[4120] tray=respawned (pid 5236, session 1)
INFO desktop companion refreshed version="0.4.221" stopped=[] tray=none was running
WARN desktop companion refreshed, but the tray could not be started again version="0.4.221" stopped=[4120] error=CreateProcessAsUserW(…\roomler-desktop.exe) failed (err 5)
WARN desktop companion refresh skipped error=moving the old companion aside failed: renaming the desktop EXE aside: Access is denied. (os error 5) (no tray was stopped)
```

From `agent-v0.4.223` (#3844) the `tray=` and `error=` say what came of the start in each session,
`session 1: CreateProcessAsUserW(…) failed (err 5)` for a failed one (§13.5).

**A tray the refresh lost.** A tray this process's refresh stopped and could not start again is
marked lost, in the session it ran in (#3844): its start there failed, or, for a tray whose session
could not be read, nobody was signed in at the console to start it for. A session whose person
signed out since lost nothing. Only while a tray is lost does a refresh start one that is not
running. A tray the person quit, or one that never ran, stays as it is; on a first run the
post-install launcher owns the first launch (§10).

```mermaid
flowchart TD
    E([after a refresh pass]) --> S{did this pass stop a tray?}
    S -->|yes| N1[nothing more: its own start<br/>brought the tray back or marked it lost]
    S -->|no| L{is a tray marked lost?}
    L -->|no| N2[nothing: a tray the person quit stays quit]
    L -->|yes, in a session| U{in that session: its person<br/>signed in, no tray, the Run value?}
    U -->|a tray runs there| C[forget the loss]
    U -->|its person signed out| C
    U -->|all three| T["start it there with --autostart"]
    T -->|it started| C
    T -->|it did not| W[a warn line, then debug lines:<br/>tried again after 1, 5 and 15 min,<br/>then at the next refresh]
    U -->|no Run value| W
```

| Piece | Where | What |
|---|---|---|
| the flag | `TrayLost`, `companion.rs:5215`, one per process (`TRAY_LOST`, `:5274`), set and cleared by `start_stopped`, `:5118` | set when a swap stopped the tray and its start did not bring it back; cleared when a start works or a tray is seen running. In memory only: after a restart the sign-in start owns the tray |
| when it is tried | `refresh_if_stale`, `companion.rs:4720`; `bring_back_lost_tray`, `:4859`; `restore_lost_tray`, `:5368` | after every pass that stopped no tray: nothing to swap (the marker reads current, the case after a swap whose start failed), a swap with none running, one that gave up before any stop, or a failed download. A pass that stopped a tray has tried its own start already |
| how it is started | `start_lost_tray`, `companion.rs:5757`; `signed_in_session`, `:5791`; `set_to_start_at_sign_in`, `:5808` | in the session it was lost in, while its person is signed in there (#3844), or, for a tray whose session was not known, where someone is signed in (the console session's person, for the service); no tray runs in that session, and the tray is set to start at sign-in: the `Roomler Desktop` Run value, in `HKLM` for a machine-wide install (present while the device's switch is on) or in `HKCU` for a per-user one (gone once its person opted out). With `--autostart`, the sign-in start's argument, so a person who switched that off sees nothing (`decide_startup` exits) |
| the retry | `LOST_TRAY_WAITS`, `companion.rs:135`; `RefreshSchedule::next`, `:463` | a lost tray is tried again within a minute, then at 5 and 15 min, then at the next refresh or sign-in. While it is lost the loop waits 1, 5 and 15 minutes (§13.2's wait when that is shorter), then its usual period, and each refresh tries the start |
| the lines | `bring_back_lost_tray`, `companion.rs:4859` | `desktop companion is back after a refresh stopped it`, with `session=` and `how=` (INFO); `desktop companion is still not running after a refresh stopped it; it is tried again`, with `session=` and `why=`: WARN for the first try that did not bring it back, DEBUG for the rest; `desktop companion is not started again after a refresh stopped it: nobody is signed in to that session any more …` (INFO, #3844) |

- While a lost tray is being retried, a tray the person quits within those minutes can be started
  once more.
- **Kill switch:** `ROOMLERD_COMPANION_SYNC=0` turns this retry off with the rest. A lost tray then
  waits for the next sign-in, or for the next start of the process that refreshes it.
- Each place is tried on its own (#3844, §13.5): the session the tray was lost in, or, for a tray
  whose session could not be read, the console session as before. One verdict per pass: its first
  try that did not bring a tray back is the WARN.
- The tests (`swap_tests`, `companion.rs:5829`) drive the order through a fake swap and start that
  fail on cue: a refused rename or promote stops nothing, a tray that would not stop gets the old
  EXE back, a start that failed is never "respawned" and marks the tray lost, the next pass starts
  it, and a tray the person quit, or a first run, gets none.

### 13.4 The version check: the marker, then the EXE's own version (Windows, #3850)

The refresh decides whether to swap from the version marker beside the EXE
(`roomler-desktop.exe.version`), which a swap writes once the new EXE is in place. Until #3850 a
write that failed (a read-only marker, a deny, a scanner holding it) was a warning only, and every
later pass read the old marker and swapped again: it downloaded the same release, moved the EXE
aside, put the same version back, and stopped and started the person's tray, at every start of the
service and every 4 h. A marker that was missing or unreadable also counted as too old to connect
(§13.2), so those swaps came every few minutes in the first hour after a start.

From `agent-v0.4.223` the marker stays the fast path, and the EXE's own version resource is the
second source:

```mermaid
flowchart TD
    P([a refresh pass]) --> E{the EXE there?}
    E -->|no| S[swap: the refresh puts one in place]
    E -->|yes| M{the marker names<br/>this daemon's version?}
    M -->|yes| N[nothing to do:<br/>one small file read]
    M -->|missing, unreadable,<br/>another version| V{the EXE's own<br/>version resource}
    V -->|this daemon's version| W[no swap: the marker<br/>is written again, once a pass]
    V -->|another version, none,<br/>or a malformed resource| S
    W -->|the write fails| L[one WARN for the process,<br/>then DEBUG]
```

| Piece | Where | What |
|---|---|---|
| the decision | `installed_state`, [`agents/roomlerd/src/companion.rs:216`](../agents/roomlerd/src/companion.rs#L216); read by `look_beside`, [`:957`](../agents/roomlerd/src/companion.rs#L957), on a blocking thread, `LOOK_WAIT` (10 s) at most | the marker first; only when it does not name the daemon's version, the EXE's. An EXE at the daemon's version is current: no download, no swap, no stop of the tray. A version that cannot be read is not known, which is stale, as before: the refresh swaps |
| the EXE's version | `exe_version`, [`:886`](../agents/roomlerd/src/companion.rs#L886); `version_resource_version`, [`:305`](../agents/roomlerd/src/companion.rs#L305) | the EXE is mapped as data and as an image resource only (`LoadLibraryExW`), so nothing in it runs, and it is freed on every path. Its `VS_VERSIONINFO` gives `ProductVersion`, else `FileVersion`. Through kernel32, which the daemon already imports: `GetFileVersionInfoW` would add `version.dll` to what the SYSTEM service loads. Every offset and length is checked against its block, and a malformed resource is not known. The release's companion carries the workspace version there, as its marker does: the 0.4.221 tray reads `0.4.221` |
| the marker written again | `look_beside`; `MarkerWrites`, [`:252`](../agents/roomlerd/src/companion.rs#L252), one per process (`MARKER_WRITES`, [`:295`](../agents/roomlerd/src/companion.rs#L295)); `log_marker_line`, [`:987`](../agents/roomlerd/src/companion.rs#L987) | a marker that does not name the version of an EXE that does is written again, once a pass. The swap's own write and the passes after it share one latch: the first failure in a process is a WARN, the rest DEBUG, and a write that works after one that did not is an INFO |
| the fast waits | `known_version`, [`:181`](../agents/roomlerd/src/companion.rs#L181); `sibling_version`, [`:745`](../agents/roomlerd/src/companion.rs#L745) | §13.2's "too old" reads the same version, off the async worker: the marker when it names the daemon's, else the EXE's, else the marker's own words. An EXE of a known version from 0.4.216 on is not too old, whatever its marker reads |

The lines, message first:

```
INFO desktop companion is already this version by its own version resource; its version marker is written again, and nothing is replaced version="0.4.223" recorded="0.4.222"
WARN could not write desktop version marker; the refresh goes by the companion EXE's own version until it can, and does not replace that EXE again error=Access is denied. (os error 5) marker=C:\Program Files\Roomler\roomler-desktop.exe.version version="0.4.223"
INFO desktop version marker written again marker=C:\Program Files\Roomler\roomler-desktop.exe.version version="0.4.223"
INFO desktop companion is stale/missing — refreshing own="0.4.223" recorded="0.4.222" installed="0.4.222" present=true
```

- A wizard-placed EXE with no marker that is already the daemon's version gets its marker without a
  swap. Before, the first start downloaded the same release and restarted the tray.
- From `agent-v0.4.223`, `roomler status` (`too_old_here`, §13.2) and the heartbeat's
  `companion_version` (`installed_version`) read the tray's version by the same rule, through
  `known_version` and the same read of the EXE (#3908, below). Before, both read the marker alone:
  with a marker that cannot be written beside a current EXE, status called the tray too old to
  connect, and the device list showed the version the marker names.
- Tests, in `swap_tests` ([`companion.rs:5829`](../agents/roomlerd/src/companion.rs#L5829)):
  `a_marker_that_cannot_be_written_is_no_reason_to_swap_again` (no second swap, one WARN, no fast
  waits) and its control `the_version_check_swaps_what_it_always_did`; in `version_resource_tests`
  ([`:6836`](../agents/roomlerd/src/companion.rs#L6836)) a good
  resource, a truncated, garbage or lying one, and on Windows a real EXE's read, freed afterwards.

#### Status and the heartbeat (#3908)

`roomler status` answers on the LocalAPI, and the heartbeat goes out on a timer, so neither may wait
on the EXE. Their read of it runs on a thread of its own, and its answer is kept for the process:

```mermaid
flowchart TD
    A([roomler status, a heartbeat]) --> M{the marker names<br/>this daemon's version?}
    M -->|yes| K[the marker's version:<br/>one small file read]
    M -->|no| C{an answer read for the files there now?<br/>the marker's last write,<br/>the EXE's last write and size}
    C -->|yes| V[the EXE's version,<br/>else the marker's words]
    C -->|no| R[the marker's words for now,<br/>and the read starts on a thread of its own,<br/>one at a time, waited for by nobody]
    R -.->|it lands, milliseconds later| C
```

| Piece | Where | What |
|---|---|---|
| the readers | `too_old_beside`, [`:694`](../agents/roomlerd/src/companion.rs#L694); `installed_beside`, [`:1471`](../agents/roomlerd/src/companion.rs#L1471); both through `known_beside`, [`:866`](../agents/roomlerd/src/companion.rs#L866) | status's and the heartbeat's cores, over the folder beside the daemon: the marker, then `known_version` with the EXE's answer for the files there now. With no EXE there is no tray to refuse or report, and nothing is read |
| the read and its answer | `ExeVersions`, [`:770`](../agents/roomlerd/src/companion.rs#L770), one for the process (`EXE_VERSIONS`, [`:761`](../agents/roomlerd/src/companion.rs#L761)); `Stamp`, [`:830`](../agents/roomlerd/src/companion.rs#L830) | `exe_version` on a thread of its own, through `OneInFlight::start_unless_running` ([`one_in_flight.rs:222`](../agents/roomlerd/src/one_in_flight.rs#L222)): one read at a time, so a read that a filter driver holds keeps one thread however often status is asked. Its answer stands while the marker's last write and the EXE's last write and size are what they were when it was read. The EXE's part covers a swap whose marker write failed, which changes the EXE and not the marker |
| a cold read | `known_beside`; `BesideNow`, [`:854`](../agents/roomlerd/src/companion.rs#L854) | no answer yet for the files there now (a daemon that just started, or a swap since the last read): the marker's words, not settled, and the read starts. The next ask has the EXE's answer |
| the heartbeat's cache | `kept_version`, [`:1420`](../agents/roomlerd/src/companion.rs#L1420) | `installed_version` keeps an answer for 10 minutes, and only a settled one: an answer from the marker while the read runs goes out once, and the next heartbeat, 30 s later, reads again. The first heartbeat after a connect starts the read, so a `roomler status` a minute after a start has the EXE's answer |

- On a machine-wide install the worker answers status and sends the heartbeat, so the two share one
  answer. The refresh runs in the service host and reads the EXE itself (`sibling_version`,
  `look_beside`), uncached.
- Tests, in `beside_version_tests` ([`companion.rs:7089`](../agents/roomlerd/src/companion.rs#L7089)),
  each on a real EXE in a folder of its own (a copy of the test's own executable, given a version
  resource with `UpdateResourceW`): the issue's case, a read-only old marker beside a current EXE,
  read as the EXE (`a_read_only_old_marker_beside_a_current_exe_reads_as_the_exe`); a marker naming
  the daemon's version, and no EXE, as before, the EXE not read; a malformed resource, by the marker
  as before; a cold read that answers from the marker while the read is held, one read however
  often asked; and an EXE swapped under a marker that cannot be written, read again. In
  `version_tests`, `the_heartbeat_keeps_only_a_settled_answer`
  ([`:1589`](../agents/roomlerd/src/companion.rs#L1589)).

### 13.5 The tray starts again in each session it was stopped in (Windows, #3844)

On a system install the swap stops every tray started from the installed EXE, in every session
(`companion_procs::find`, each with its Terminal Services session). Until #3844 it then started one
tray, in the active console session, for whoever was signed in there. On a host with Remote Desktop
beside the console, fast user switching, or a server, a person whose tray ran in another session lost
it, at most one of several came back, and with nobody at the console none did.

From `agent-v0.4.223`:

```mermaid
flowchart TD
    S([the swap stopped trays]) --> P[the sessions they ran in,<br/>each once, in order]
    P --> T{that session's person<br/>still signed in?}
    T -->|yes| C[a start with that session's<br/>own token, 10 s at most]
    T -->|no token, or<br/>the session is gone| O[none started:<br/>the line says so, nothing lost]
    C -->|it started| B[respawned, in the line]
    C -->|it failed, or<br/>gave no answer| L[lost in that session:<br/>tried again there, §13.3]
```

| Piece | Where | What |
|---|---|---|
| the plan | `restart_plan`, [`companion.rs:5146`](../agents/roomlerd/src/companion.rs#L5146), over `swap`'s stopped trays, [`:5058`](../agents/roomlerd/src/companion.rs#L5058) | for the service, once in each session a stopped tray ran in, in order, and nowhere else. A tray whose session could not be read starts where the start has always gone, the console session's person. A per-user install's daemon starts one, in its own session, as before. None stopped, none started |
| the start | `start_desktop_in_session`, [`:5660`](../agents/roomlerd/src/companion.rs#L5660); `query_session_user_token`, [`win_service/supervisor.rs:218`](../agents/roomlerd/src/win_service/supervisor.rs#L218) | that session's own person's token (`WTSQueryUserToken`), its environment, and `CreateProcessAsUserW` on `winsta0\default`, as the console start does. No token (`ERROR_NO_TOKEN`), or no such session any more (`ERROR_CTX_WINSTATION_NOT_FOUND`), is a person who signed out: nothing is created. A disconnected Remote Desktop session still has its person, so the tray starts there and waits for them |
| the bound | `start_again`, [`:5715`](../agents/roomlerd/src/companion.rs#L5715); `start_within`, [`:5735`](../agents/roomlerd/src/companion.rs#L5735); `RESPAWN_WAIT`, [`:5703`](../agents/roomlerd/src/companion.rs#L5703) | each start runs on the thread of the one start in flight for its session (`RESPAWNS`, threads named `tray-respawn`, `one_in_flight`'s rules), and the refresh waits 10 s at most for it. A start that hangs fails in time, saying so, holds that session alone, and the next session's start goes ahead. Never on the async worker: the swap and the lost-tray retry run on the blocking pool |
| the line | `refresh_line`, [`:5294`](../agents/roomlerd/src/companion.rs#L5294); `TrayRestart`, [`:4968`](../agents/roomlerd/src/companion.rs#L4968) | one part per session, `; ` between them: `respawned (pid …, session …)`, `session 3: nobody is signed in to that session any more, so none was started`, or `session 3: ` and why the start failed. INFO when every session got its tray back or has nobody to get one; WARN `…could not be started again` when a start failed |
| a tray lost | `TrayLost`, [`:5215`](../agents/roomlerd/src/companion.rs#L5215); `start_lost_tray`, [`:5757`](../agents/roomlerd/src/companion.rs#L5757) | per session: §13.3's retry goes to the session the tray was lost in, while its person is signed in there, never to the console, whose person may be someone else. A session signed out of since is forgotten, with an INFO line |
| unchanged | `start_desktop`, [`:5616`](../agents/roomlerd/src/companion.rs#L5616) | the console start of the consent prompt (§10.4) when no tray runs, and a per-user daemon's start in its own session |

The lines, message first:

```
INFO desktop companion refreshed version="0.4.223" stopped=[4120, 6044] tray=respawned (pid 5236, session 1); respawned (pid 7310, session 3)
INFO desktop companion refreshed version="0.4.223" stopped=[4120, 6044] tray=respawned (pid 5236, session 1); session 3: nobody is signed in to that session any more, so none was started
WARN desktop companion refreshed, but the tray could not be started again version="0.4.223" stopped=[4120, 6044] error=respawned (pid 5236, session 1); session 3: CreateProcessAsUserW(…\roomler-desktop.exe) failed (err 5)
INFO desktop companion is not started again after a refresh stopped it: nobody is signed in to that session any more, and the sign-in start owns their next tray session=3
```

- Tests, in `swap_tests`: `trays_stopped_in_two_sessions_start_again_in_those_two`,
  `a_session_signed_out_of_since_gets_no_start_and_the_line_says_so`,
  `where_no_session_is_known_the_start_goes_where_it_always_went` and
  `a_lost_tray_is_tried_again_in_the_session_it_was_lost_in`; in `start_within_tests`
  ([`:7461`](../agents/roomlerd/src/companion.rs#L7461))
  `a_start_that_hangs_holds_its_session_alone`. They run in the Windows agent unit lane
  (`windows-agent-sessions`, step "Agent unit tests on Windows (the whole --lib suite)") and in the
  Linux lanes that run the agent's whole `--lib` suite.

## 14. Opening links: View screen and Open Roomler (#2713)

**View screen** on the Devices page (`cmd_open_remote`, `src/commands.rs:1173`), **Open Roomler**
on the Overview page and the tray all open a link through one function, `open_web_or_browser`
(`src/commands.rs:976`):

| OS | Where the link opens | When nothing can open it |
|---|---|---|
| Windows | the in-app WebView2 window (Chromium: the full viewer works), else the default browser | the browser fallback's error |
| macOS | the default browser (`open`) | the shell's error |
| Linux | **the app registered for the link's scheme**, through GIO, with an activation token that brings a running browser forward (`open_in_browser`, `src/commands.rs:1026`; §14.2) | a dialog and the button's error as its tooltip: *No web browser is set up on this computer…*, until the next success puts the button's own back (`src/front/devices.js:261`, #2729) |

⚠️ **Linux never uses `xdg-open` for this.** When no app claims a link's scheme, xdg-open (GNOME:
`gio open`) falls back to the link's **content** type: it fetches the page, sees `text/html` and
hands the link to the `text/html` default. Measured 2026-10-22 on a Fedora Asahi GNOME box with no
browser installed: View screen opened the viewer's URL in GNOME Text Editor, which showed the
page's HTML. A missing browser must be an error the person can act on, never a text editor.

```mermaid
flowchart TD
    C([View screen / Open Roomler]) --> S{the link's scheme<br/>url_scheme}
    S -->|none| E1[error: not a link]
    S -->|https| D{GIO: the default app<br/>for x-scheme-handler/https}
    D -->|none| E2[dialog + red button:<br/>no web browser is set up]
    D -->|a browser| L[launch THAT app with the link<br/>and an activation token, §14.2]
    X[xdg-open / gio open] -. would have .-> T[sniffed text/html:<br/>the text editor]
```

### 14.1 A companion the daemon starts carries the desktop's environment

The lookup above reads the desktop's own defaults, which depends on the companion's environment:

| Variable | What it changes |
|---|---|
| `XDG_CURRENT_DESKTOP` | GIO reads the desktop's `<desktop>-mimeapps.list` only with it (GNOME's names Firefox, KDE's Falkon) |
| `XDG_DATA_DIRS` | where apps are found, `~/.local/share/flatpak/exports/share` included: a browser installed as a **per-user Flatpak** is invisible without it |
| `XAUTHORITY` | X11 authorisation beside `DISPLAY` |
| `LANG`, `LANGUAGE`, `LC_*` | the person's locale rather than the system's |

A companion the desktop starts (its autostart entry) has all of these. One the **daemon** starts
did not: FR-27's consent launch, FR-117's restart after an update
(`restart_replaced`, `agents/roomlerd/src/companion/package_sync.rs:412`) and the post-install
first run all go through `systemd_run_command` (`agents/roomlerd/src/companion.rs:4147`). For a
root daemon that is a transient **system** unit, which inherits nothing of the desktop; its
environment was logind's four variables (`XDG_RUNTIME_DIR`, the session bus, `DISPLAY`,
`WAYLAND_DISPLAY`) and nothing else.

Since #2713 a root daemon reads the session user's systemd manager environment, the store the
desktop fills at login, by running `systemctl --user show-environment` **as that user** (the one
privilege-drop path, `exec::drop_to_std`), bounded at 5 s (`user_manager_environment`,
`companion.rs:4227`). It passes on an **allowlist** of it (`SESSION_ENV_KEYS`, `companion.rs:4175`;
parsing in `session_env_pairs`, `companion.rs:4201`):

- ⚠️ **An allowlist, not the whole environment.** The values are the user's own, and the companion
  runs as that user, so no privilege crosses. But loader plumbing (`LD_*`, `GTK_MODULES`) has no
  business in a process the daemon starts, and the bus, display and runtime dir keep coming from
  logind.
- ⚠️ A value systemd had to shell-escape (`KEY=$'…'`), an empty one, or one with a control
  character is skipped, never decoded. Nothing on the allowlist needs one.
- A per-user daemon's `systemd-run --user` unit inherits the manager's environment already, so it
  reads nothing. When the manager's environment cannot be read, the companion starts as it always
  did.
- 🔑 **Check it on a host:** read the running companion's `/proc/<pid>/environ` (as root) for
  `XDG_CURRENT_DESKTOP`, and its `/proc/<pid>/cgroup` for which manager started it: a
  `system.slice/run-…service` is the daemon's launch, an `app-…` scope or service under
  `user@<uid>.service` is the desktop's.

### 14.2 A browser that is already running comes forward (#2729)

With the browser already open behind the companion, a launch without a token opened the tab and
left the window where it was. GNOME's focus-stealing prevention does not raise a window for a
request that names no user action, so to the person View screen did nothing. The companion now
asks GDK for an **activation token** tied to the click and hands it to the browser
(`launch_default_handler`, `src/commands.rs:1065`; the token, `activation_token`,
`src/commands.rs:1084`):

```mermaid
sequenceDiagram
    autonumber
    participant P as the person
    participant C as companion (GTK main thread)
    participant M as compositor
    participant N as new browser process
    participant B as the running browser
    P->>C: clicks View screen
    C->>M: xdg_activation_v1: a token for this click
    M-->>C: the token
    C->>N: launch the https handler with the link,<br/>XDG_ACTIVATION_TOKEN and DESKTOP_STARTUP_ID set
    N->>B: the link and the token (its single-instance hand-off)
    B->>M: activate my window, with the token
    M-->>P: the browser comes forward on the new tab
```

- ⚠️ **The token goes in the environment explicitly**, in a plain GIO launch context. GLib asks a
  launch context for a token only when the app's desktop file says `StartupNotify=true`, and
  Fedora's Chromium's has no such key. Measured 2026-10-22 on the lab's Fedora Asahi GNOME box
  (GTK 3.24.52, GLib 2.84, mutter 48, Chromium 148), three launches from a GTK 3 window with the
  browser open behind it:

  | Launch | New tab | Window raised |
  |---|---|---|
  | no launch context (the companion before #2729) | yes | **no** |
  | GDK's own launch context | yes | **no**: GLib asked it for no token |
  | GDK's token, set explicitly in a plain GIO context | yes | **yes** |

- ⚠️ **A plain context, never GDK's own with the token set on it.** On Wayland, GDK's launch
  context answers `get_display` with the compositor socket's name, and GIO writes that answer into
  the launched program's `DISPLAY`. An X11 client the browser starts would then look for an X
  server called `wayland-0`. Measured on the same box: GDK's context answers `'wayland-0'`, a
  plain one `None`.
- ⚠️ **GTK main thread only.** The token comes from GDK, while View screen and Open Roomler are
  async commands on Tauri's runtime, so the launch hops to the main thread and waits for the
  answer (`on_main_thread`, `src/commands.rs:1098`). The tray's Open Roomler is already there,
  and Tauri runs a main-thread task in place for a caller on the main thread, so the wait cannot
  deadlock.
- Best-effort: with no GDK display, or off its thread, there is no token and the link opens as
  it did before.
- **GNOME Web is covered**, although it runs as one instance. Fedora's package (48.5) has no
  `DBusActivatable`, so GIO starts `epiphany <link>`, and that process hands the link, with the
  token from its environment, to the running GNOME Web over D-Bus. Measured 2026-10-23 on the same
  box, with the companion on 0.4.158: GNOME Web was minimized, and View screen restored it and
  brought it forward on the new tab.
- Not covered: a browser whose desktop file says `DBusActivatable=true`. GIO starts it over D-Bus
  itself, so it takes no environment from the companion, and GLib puts a token into the activation
  only from a context that answers for one, which a plain context never does. It would open the tab
  and may stay behind. None measured.

### 14.3 A browser the companion opened outlives the companion (#2729)

A browser that View screen or Open Roomler **started**, because none was running, is the
companion's child, so it starts in the companion's cgroup. Chromium moves its own browser process
into an `app-org.chromium.Chromium-<pid>.scope` once it is up, but everything it started before
that stays where it was launched: its zygotes, the GPU process, the renderers, the network and
storage services and its crash handlers (measured on Chromium 148: the browser process in its
scope, 10 helpers in the companion's unit). A companion the desktop autostarted sits in an
`app-…` scope, and a scope does not stop when one process in it exits. One the **daemon** started
(§14.1) is the main process of a transient `systemd-run` **service**: systemd stops a
`Type=simple` service when its main process exits, and under the default `KillMode=control-group`
kills everything left in its cgroup. Quitting the companion, a crash, and FR-117's restart after
**every** update (`restart_replaced` sends the old companion SIGTERM,
`agents/roomlerd/src/companion/package_sync.rs:418`) each killed those helpers, and the browser
crashed with them (the field check below).

Since #2729 the daemon starts the companion with `--property=KillMode=process`
(`systemd_run_command_with`, `agents/roomlerd/src/companion.rs:4312`), in a root daemon's system
unit and a per-user daemon's `--user` unit alike: stopping the unit signals the companion only.
Its own helpers were expected to end with it, since WebKit's web and network processes exit when
their UI process's connection closes. ⚠️ Not on every host: on the 0.4.221 lab's Ubuntu VM (software
rendering), two and then three of them kept running in the stopped unit after each tray stop
(§10.9).

Measured 2026-10-22 on the same box, with a replica of the daemon's unit whose main process starts
a child and exits after 4 s:

| Unit | The child at 2 s | The child at 8 s |
|---|---|---|
| `systemd-run --quiet --collect --uid=<uid>` (before #2729) | alive, in `system.slice/run-…service` | **killed** |
| the same with `--property=KillMode=process` | alive | alive |

- ⚠️ A unit keeps the mode it started with. On the update that brings this change, FR-117's
  restart of the OLD companion still takes a browser that companion opened with it, once; every
  companion the new daemon starts is `KillMode=process`.
- 🔑 **Check it on a host:** `systemctl show <unit> -p KillMode`, with the unit read from the
  companion's `/proc/<pid>/cgroup`. The helpers of a browser the companion started name the same
  unit in their own `cgroup` files (Chromium's browser process names its `app-…` scope), and all
  of them survive `kill -TERM <companion pid>`.

### 14.4 Field check (#2729)

Run on the lab's Fedora Asahi GNOME box (GNOME 48 Wayland, Chromium 148) with a companion the
daemon started. The before column comes from 0.4.153; the after column is the release that
carries #2729, run 2026-10-23 00:19–00:24 UTC.

| Check | 0.4.153 | 0.4.154 |
|---|---|---|
| A browser the companion opened, through FR-117's restart of that companion on an update | the old unit (`KillMode=control-group`) stopped at 00:19:55, and **in the same second** the browser died: SIGTRAP, a core dump, none of its 11 processes left; its next start offered "Restore pages? Chromium didn't shut down correctly" | the new unit reads `KillMode=process`. A SIGTERM to the companion, which is what FR-117 sends, leaves the browser and all its helpers running (the dead unit still holds them), the companion's two WebKit helpers exit by themselves, and the browser opens new tabs afterwards |
| View screen with the browser open behind the companion | a new tab, and the window stays behind | the window comes forward on the new tab (twice) |
| The button after a failure (the browser's entry masked with a user-level `Hidden=true`), then a success | the *No web browser is set up…* tooltip outlives the success | the failure shows it, and the success puts the button's own tooltip back and drops the red |

The update path itself was the operator's: `roomlerd self-update`, then a restart of the service
(#2724). The new daemon's FR-117 sync logged `companion: the desktop companion package now matches
this daemon … from=0.4.153 to=0.4.154 restarted=1`.
