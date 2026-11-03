# Self-hosting Roomler

Everything, on your own machine, with no licence key, no activation, no device
cap and no phone-home. The self-hosted edition is the same code that runs the
hosted service — there is no crippled community build. See
[`LICENSING.md`](../LICENSING.md) for why that is a licence commitment rather
than a current-version promise.

We collect no telemetry from self-hosted deployments. That is deliberate: the
product asks you to run a privileged daemon that can see your screen, so an
install that quietly reported home would be indefensible.

---

## What you are about to run

One container serves the web app and the API behind nginx. Around it: MongoDB,
Redis, MinIO for file storage, and optionally coturn as a relay.

| Pillar | Works self-hosted |
|---|---|
| Remote desktop in a browser tab | yes — peer-to-peer, the server only signals |
| Overlay mesh, tunnels, SOCKS5, exit nodes, MagicDNS | yes |
| `roomler ssh` (no `sshd`, no open port) | yes |
| Chat, rooms, files, search | yes |
| Video conferencing (mediasoup SFU) | yes, with the media-port caveat below |
| Push notifications, email, OAuth, billing | needs your own keys — see *Optional integrations* |

### Choose a profile

The server is one binary composed from six modules, and the published image
comes in five **profiles** — the same server with only the pillars you run.
A smaller profile is not a crippled one: it is the same code with fewer doors,
and `GET /health` on any of them lists the modules it actually mounts.

| Profile | Image tag | What it runs | Needs alongside it | What it skips |
|---|---|---|---|---|
| `full` (default) | `<tag>` | everything | MongoDB, Redis, MinIO, coturn, the media port range | — |
| `collab` | `<tag>-collab` | chat, rooms, files, video calls | MongoDB, Redis, MinIO, coturn, the media port range | the device fleet, the overlay mesh, tunnels, remote desktop |
| `remote` | `<tag>-remote` | devices + remote desktop | MongoDB, Redis, coturn | the SFU worker build, MinIO, the overlay mesh |
| `mesh` | `<tag>-mesh` | devices + overlay mesh, tunnels, `roomler ssh` | MongoDB, Redis, coturn (and DERP relays if you run them) | the SFU worker build, MinIO |
| `access` | `<tag>-access` | devices + remote desktop + mesh — no chat, no calls | MongoDB, Redis, coturn | the SFU worker build, MinIO |

Pick one by **tag** when you pull (`ROOMLER_IMAGE=ghcr.io/gjovanov/roomler-ai:<tag>-mesh`
in `.env.selfhost`) or by **`ROOMLER_PROFILE`** when you build from source. No published
`v*` image carries the hosted service's billing and newsletter module — that is what `roomler.ai`
runs, not what you run.

> The same package also holds the **`hosted-<date>-<sha7>`** tags and the moving `hosted`
> pointer: the image `roomler.ai` itself deploys, built from every merge to `master` by the
> `hosted image` workflow (FR-73). It is the `full` composition **plus** the billing module,
> holds no secret (everything is configuration), and is public because the source is — but it
> is not a release: it is whatever `master` was an hour ago, with the Stripe webhook and the
> newsletter routes mounted. Pull `latest` or a `v*` tag; leave `hosted-*` to the hosted service.

> A profile does not reject configuration it does not use: a `mesh` image given MinIO
> credentials simply never opens them. A web app built for `full` works against every
> profile — the navigation follows `GET /api/capabilities`, so a `mesh` server shows no
> chat or call surfaces even though the bundle carries them. Building your own bundle
> with `VITE_MODULES=fleet,network bun run build` prunes those routes out of it; the
> published image never needs that, the runtime answer alone decides.

---

## Quickstart

**You need** Docker with Compose v2, ~4 GB RAM, and ~10 GB disk. Below Docker Engine 29,
a Docker restart can [cost you the relay until you run `up -d`](#after-a-reboot-or-a-docker-restart);
the rest of the product comes back by itself.

```bash
git clone https://github.com/gjovanov/roomler-ai.git
cd roomler-ai

cp .env.selfhost.example .env.selfhost
```

Open `.env.selfhost` and fill in the four required values. Generate the two
secrets:

```bash
openssl rand -hex 32   # → ROOMLER_JWT_SECRET
openssl rand -hex 32   # → ROOMLER_TURN_SECRET
openssl rand -hex 24   # → MONGO_ROOT_PASSWORD
openssl rand -hex 24   # → MINIO_ROOT_PASSWORD
```

⚠️ Keep the datastore passwords alphanumeric. The Mongo one is interpolated into
a connection URL, so `@`, `:`, `/`, `?` or `#` inside it breaks the URL — and the
resulting failure looks like a wrong password rather than a quoting problem.

Then bring it up:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost pull
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d
```

⚠️ **Run `pull` first.** `up -d` on its own will *build* rather than fetch —
Compose treats a service with a `build:` section as buildable, so a missing
image is compiled from source instead of downloaded. Skipping the `pull` costs
you the twenty minutes the published image exists to save, and it does it
silently.

> Images are published at
> [`ghcr.io/gjovanov/roomler-ai`](https://github.com/gjovanov/roomler-ai/pkgs/container/roomler-ai)
> for **linux/amd64 and linux/arm64**. The tag is a manifest list, so a
> Raspberry Pi, an Apple Silicon Mac and an x86 VPS all pull the same name and
> get the right image. Each architecture is built on its own native runner and
> smoke-tested there before publication — neither is emulated.

### Or build it from source

Always available, and never merely a fallback: the server is AGPL-3.0, and a
published image must never be the only way to run it.

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d --build
```

> **The build compiles the Rust server and mediasoup from source.** Measured
> **6 minutes** on a 2024 laptop (16 cores, Docker Desktop + WSL2); budget
> 15–20 on a small VPS or 2–4 cores. Subsequent starts are seconds.

> **Countries in the analytics.** The published image carries DB-IP's
> *IP to Country Lite* database ([CC BY 4.0](https://creativecommons.org/licenses/by/4.0/),
> credited on the dashboard) for the platform analytics' Countries breakdown. A
> source build carries it only if you run `scripts/fetch-geoip.sh` before
> building. Without it every country reads `unknown`, which is a supported
> state, and the boot log warns once. To turn country lookups off in either
> case, set `ROOMLER__STATS__GEOIP_MMDB` to an empty value. Details:
> [`files/geoip/README.md`](../files/geoip/README.md).

Watch it come up:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost logs -f roomler
curl -fsS http://localhost:8080/health && echo OK
```

Open <http://localhost:8080> and register, then create your organization from
the dashboard. The organization owns your devices, invitations and rooms, and
you are its owner — there is no separate admin bootstrap step and no seeded
account.

⚠️ Registration through the web app creates the account only; the organization
is the next step, not part of the same form.

---

## Add your first machine

> ⚠️ **Do the TLS step below first if the machine is not this one.** Enrollment
> tokens must travel over TLS, so the agent **rewrites `http://` to `https://`**
> for any address that is not loopback (`crates/agent-core/src/enrollment.rs`).
> Against a plain-HTTP server that produces
> `SSL routines:ssl3_get_record:wrong version number` — an error that names
> neither the cause nor the fix. `http://localhost:8080` is exempt and works, so
> enrolling *this* machine needs nothing extra; every other machine needs
> [a real hostname with TLS](#putting-it-behind-a-real-hostname).
> Editing the address in a device's `config.toml` back to `http://` does not get
> around this. Every load of the config reads a remote `http://` as `https://`
> (FR-101 P1b, [security-baseline.md](security-baseline.md) §4). A lab that
> knowingly runs without TLS sets `allow_plaintext_server = true` in that
> device's config, which is the device owner's decision and never the server's.

Mint an enrollment token in the web app (**Devices → Enroll device**), then on
the machine you want to reach:

```bash
# Linux / macOS
curl -fsSL https://<your-host>/api/setup/install.sh | sh -s -- \
    --role daemon --token <enrollment-jwt>

# Windows (PowerShell, elevated)
& ([scriptblock]::Create((irm https://<your-host>/api/setup/install.ps1))) `
    -Role daemon-system -Token <enrollment-jwt>
```

The script you fetch **already points at your server**: the route that serves it
substitutes your `ROOMLER_PUBLIC_URL` for the built-in default before the bytes
leave the process, because a piped script has no way to see the URL it came from
(FR-50). You can still pass `--server` / `-Server` to override it.

⚠️ **This depends on `ROOMLER_PUBLIC_URL` being right.** If it is not a plain
`scheme://host[:port]`, the server logs a warning and serves the script with the
hosted default untouched — at which point the agent downloads from you and then
tries to enroll against `roomler.ai` with a token only your server can verify,
failing with an authentication error that says nothing about the real cause.
Pass `--server` explicitly if you see that warning. The same value drives your
OAuth returns, invite links and CORS policy, so it is worth getting right
regardless.

⚠️ **On Windows, `irm … | iex` cannot pass arguments at all** — no `-Server`,
no `-Token`. The `scriptblock` form above is the one that can, and it is what
`scripts/install.ps1`'s own header documents.

The token is single-use and expires in 10 minutes. The agent connects
**outbound only** — nothing needs to be opened on the machine you are enrolling.

Then, from the web app, open its desktop; or from any enrolled machine:

```bash
roomler ping   <name>          # every node has a stable IP and a MagicDNS name
roomler ssh    <name>          # a shell, with no sshd and no listening port
roomler forward --agent <name> --local 127.0.0.1:5432 --remote db.internal:5432
```

---

## Putting it behind a real hostname

Optional while you are trying it out on one machine, **required the moment you
add a second** — enrollment refuses plaintext to anything but loopback, as the
warning above explains.

Terminate TLS in front of the stack and point it at `ROOMLER_HTTP_PORT`. The app
needs WebSocket upgrades on `/ws` and `/derp`, and both are long-lived — set
generous read timeouts or sessions will be cut every 60 seconds.

⚠️ Whatever you put in front must **forward `Upgrade` and `Connection`**. A
proxy that drops them leaves a device that enrolls successfully and is then
never seen online again, because the agent's control socket can never open —
and nothing in the UI distinguishes that from a machine that is switched off.

```caddy
roomler.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy handles WebSockets and certificates with no further configuration. With
nginx, add the usual `Upgrade` / `Connection` headers and
`proxy_read_timeout 3600s`.

Then update `.env.selfhost` and restart:

```bash
ROOMLER_PUBLIC_URL=https://roomler.example.com
ROOMLER_ANNOUNCED_IP=<the host's public IP>
ROOMLER_PUBLIC_IP=<the host's public IP>
ROOMLER_TURN_URL=turn:roomler.example.com:3478
```

⚠️ `ROOMLER_PUBLIC_URL` is used for OAuth callbacks and for the links inside
invitation and notification email. A stale value produces invitations that lead
nowhere, and it is not obvious from the failure.

---

## Conference media — the one part that is not a proxy problem

Remote desktop, tunnels, SSH and the overlay mesh are peer-to-peer with a relay
fallback, and work through anything. **Conference video is different**: browsers
send RTP straight at an address the server advertises, on a UDP port range, and
no reverse proxy is involved.

Two settings decide whether it works:

- `ROOMLER_ANNOUNCED_IP` — the address browsers send to. `127.0.0.1` works only
  for calls made on the host itself.
- the RTC port range — 32 ports are mapped by default, enough to try it out.

On **Linux**, the clean answer is host networking: give the `roomler` service
`network_mode: host`, delete its `ports:` block, and the full 40000–49999 range
is reachable. That is how the hosted service runs. It is not available on Docker
Desktop for macOS/Windows, which is why the port-mapped form is the default.

If signalling looks perfect and no video arrives, this is nearly always the
cause. `connect_transport` succeeding proves only that the client sent its DTLS
parameters — it says nothing about whether packets can flow.

---

## The TURN relay

coturn carries traffic for peers that have no direct path, and for a browser told
to use nothing else. It is optional: the DERP floor over the API's own port keeps
remote desktop, tunnels, SSH and the overlay connected without it. That is also
how a broken coturn goes unnoticed. `/health` stays 200, the server keeps
advertising the TURN URLs, and the only sign is in a client's log:
`TURN allocate failed … attribute not found` when nothing answers on 3478.

### How to tell that it is healthy

Run this on the host:

```bash
bash scripts/selfhost-coturn-check.sh
```

It starts coturn if it is not running, watches it for 30 seconds, then drives the
image's own TURN client through the published port, with credentials minted the way
the server mints them. It exits non-zero and names what failed. Each run leaves 9 relay
ports held for 13 minutes, because the client does not end what it allocates over UDP.

| It says | What is wrong |
|---|---|
| `coturn is not staying up` | A crash loop. See below. |
| `coturn is running outside the roomler service's namespace` | The server's process died moments ago, and coturn has not found its way back yet. See [if the server's process dies](#if-the-servers-process-dies). |
| `no relay allocation` | Nothing answers on 3478. Check the `roomler` service's ports, and that it is running. |
| `coturn has no free relay port` | Under any line. Allocations hold all 101 ports, so no client can get a relay until one ends. See [relay ports](#relay-ports). |
| `coturn hands out … but its --external-ip is …` | The relay address is not the public address. Clients would be told the wrong place. |
| `CreatePermission to the public address … is refused` | No call can use this relay: every browser asks for this first. coturn logs `A peer IP … denied in the range`, because it rewrites the public address to its own before the deny list reads it. An edited file has lost the `--allowed-peer-ip` line. |
| `relay to relay carried nothing` | Two relayed clients cannot reach each other. |
| `does not run in the server's network namespace` | An edited file has lost `network_mode: service:roomler`. A relay to the public address lands on coturn itself, so a call signals and carries no media. |
| `NOT refused` | The deny list has a hole. See [what it refuses](#what-the-relay-refuses). |
| `could not ask about` | coturn gave no answer about that peer, so this proves nothing about the deny list. The lines under it say why. |
| `NOTE the relay address is 127.0.0.1` | Not a failure, but only this machine can use it. See [the two addresses](#the-two-addresses-to-set). |
| `NOTE coturn was not running, so this started it` | It had stopped, and the note says why. See [if the server is replaced](#if-the-server-is-replaced) and [after a reboot](#after-a-reboot-or-a-docker-restart). |
| `NOTE coturn has restarted … and is not climbing` | Not a failure. Each server crash costs coturn one restart. |

### What a crash loop looks like

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost ps -a coturn
docker inspect -f '{{.RestartCount}}' "$(docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost ps -aq coturn)"
```

The status reads `Restarting (255)`, or `running` if you catch it between two exits,
and `RestartCount` climbs. A count above 0 that holds still is not a loop: it has died
since Docker last started it, and come back. Read the reason from the whole log:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost logs --no-log-prefix coturn | grep -a 'unrecognized option'
```

⚠️ `logs --tail` hides it: coturn prints its full help text after the line that says
why it quit.

### What is pinned

`coturn/coturn:4.18.0-r0`, by its multi-arch digest, in both compose files. `latest`
moved to 4.18.0, which removed `--no-dtls`, a flag this file passed, and every fresh
install crash-looped from then on. Nothing replaces that flag: with no certificate
configured, coturn opens no TLS or DTLS listener at all.

To move the pin, change the line in both files and run the check. The
`selfhost-coturn` lane runs it on any PR that changes either file.

### Why coturn and the server share a namespace

A browser asks the relay for the SFU at the public address, and coturn rewrites that
address to its own before it sends. So the SFU has to listen on coturn's own address.
`coturn` therefore runs in the `roomler` service's network namespace, with
`network_mode: service:roomler`, and `roomler` publishes coturn's ports with its own.

The server owns the namespace because it has to come back by itself whatever happens to
the relay, which is optional. A container that joins another's namespace can lose a race
when Docker starts them. An owner cannot, on any engine. A separate small service
holding the namespace for both was the first design, and it let the relay's plumbing
keep the whole app down: on Docker 28.5.2 a member lost in 2 of 24 daemon restarts.

This costs three things, each below.

### If the server's process dies

Docker brings `roomler` back in a new namespace and leaves `coturn` in the old one, with
no address, still `running`. Nothing would ever move it: `up -d` sees nothing to do. So
coturn's healthcheck ends it as soon as it has no address, and its restart policy brings
it back into the new namespace. With `kill -9` of the server's process and Compose 2.40.3,
coturn was back within 20 s in 8 of 8 crashes on Docker 29.1.3 and within 21 s in 6 of 6 on
28.5.2, and the server answered after about 10 s. coturn's `RestartCount` goes up by one
each time.

Run the check in that gap and it says
`coturn is running outside the roomler service's namespace`, with the repair if it
persists. It leaves the server alone:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost restart coturn
```

`docker stop roomler` is different: coturn then has nothing to rejoin, and stays `exited`.
A plain `up -d` brings both back.

### If the server is replaced

A new image or a changed setting replaces the server's container, and coturn has to be
replaced with it, because it names that container by id. A plain `up -d` for the whole
file does both, in order:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d
```

⚠️ Naming the server replaces only the server: `up -d roomler`, also with
`--force-recreate` or `--no-deps`. coturn then stays `exited`, `docker inspect` says
`joining network namespace of container: No such container`, and a plain
`docker compose ps` leaves it out. The relay is dark until the `up -d` above.

A plain `up -d` and `restart coturn` behaved the same on Compose 2.17.3 and 2.20.3 with
Docker 28.5.2, and on 2.29.7 and 2.40.3 with Docker 29.1.3. Docker 29 refuses the two older
Compose releases outright: `client version 1.42 is too old`.

### After a reboot or a Docker restart

Docker 29 starts a namespace's owner before the containers that join it. Older engines
start everything at once, so coturn can lose: it stays `Exited (128)`, the daemon does not
retry it, and `docker inspect` says
`cannot join network namespace of a non running container`.

That costs the relay, not the app: the server comes back either way. `docker compose ps -a`
shows it, and the check starts it and says why it had stopped. A plain `up -d` brings it back:

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d
```

Measured with the whole stack on Compose 2.40.3. On Docker 29.1.3, through 20 daemon
restarts and 2 reboots, the server and coturn came back every time. On Docker 28.5.2, the
server came back through 20 daemon restarts and 4 reboots, and coturn stayed
`Exited (128)` after 4 of the restarts.

### The two addresses to set

`ROOMLER_PUBLIC_IP` is what coturn tells clients its relay address is.
`ROOMLER_TURN_URL` is where the server tells them to find coturn. Both default to
`127.0.0.1`, and that fails silently for anyone on another machine. With the URL left
alone, a client looks for the relay at its own loopback. With only the IP left alone,
coturn starts clean and logs no error, and a client that reaches it gets an allocation
and can send through it, but the address that client gives its peer is `127.0.0.1`,
which is the peer's own loopback. Set both to an address clients can reach, as in
[Putting it behind a real hostname](#putting-it-behind-a-real-hostname).

A LAN address works for `ROOMLER_PUBLIC_IP`, although the deny list covers it, because
coturn rewrites it to its own before the check.

⚠️ The server hands out more TURN URLs than this coturn serves: it adds
`turn:…:443?transport=udp` and three `turns:` variants, and this file opens only 3478
over UDP and TCP, with no TLS. A failed allocation that names port 443 or 5349 is
expected here and costs a client time, not the relay.

### What the relay refuses

Anyone with an account on your server can get a TURN credential, so the relay must not
be a way into the machines around it. With the shipped flags it answers 403 to a peer in:

| Range | Refused by |
|---|---|
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` | the `--denied-peer-ip` lines |
| `100.64.0.0/10`, the range overlay meshes use | the `--denied-peer-ip` lines |
| `169.254.0.0/16`, including the cloud metadata address | coturn 4.18.0 itself |
| loopback | coturn itself |
| multicast | `--no-multicast-peers` |

That includes the other containers on the stack's network and its gateway, which
reaches every listener on the host. The check tries each of them.

**One address passes:** the shared namespace's own, through `--allowed-peer-ip`. coturn
needs it to reach the SFU. On it, a relay can reach only UDP ports that are already
published to the world: coturn's, its relay ports and the SFU's.
`--no-tcp-relay` answers a TCP relay allocation with 442, so the TCP listeners on that
address, the API among them, stay out of reach. Clients still reach coturn over TCP:
browsers and the agent only ever ask for a UDP relay.

The relay still carries UDP to public addresses. That is what a TURN relay does.

### Relay ports

The file gives coturn 101 ports, 49152 to 49252. Each allocation holds one until its client
ends it or its lifetime runs out, and a client that vanishes leaves it to run out. With
all 101 held, coturn answers 508 to the next allocation, and that client gets no relay.

To widen the range, change `--min-port`, `--max-port` and the `49152-49252` line under
`roomler`'s `ports:` together. To free it now, `restart coturn` ends every allocation,
including those of calls in progress.

---

## Optional integrations

None of these are required to run the product; all are off until you supply
keys. Add them to the `roomler` service's `environment:` block, using the names
from [`.env.example`](../.env.example).

| Feature | Variables |
|---|---|
| Outbound email (invites, notifications, activation) | `ROOMLER__EMAIL__*` |
| OAuth sign-in (Google, GitHub, Microsoft, LinkedIn, Facebook) | `ROOMLER__OAUTH__*` |
| Web push | `ROOMLER__PUSH__*` |
| AI document recognition | `ROOMLER__CLAUDE__API_KEY` |
| Stripe billing | `ROOMLER__STRIPE__*` |

⚠️ `ROOMLER_AUTO_VERIFY=true` is the default **because no SMTP is configured out
of the box** — an account waiting on an activation email that can never arrive is
an account nobody can use. Set it to `false` once email works.

---

## Upgrading

```bash
git pull
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d --build
```

Schema migrations run at startup and are leader-gated, so it is safe to restart
into a newer build.

Run `up -d` for the whole file, never for `roomler` alone: that
[leaves the relay dark](#if-the-server-is-replaced).

⚠️ **Agent updates are a separate track from your server.** Enrolled agents ask
*your* server for the current release, but that endpoint proxies the upstream
[GitHub releases](https://github.com/gjovanov/roomler-ai/releases) — it does not
serve builds you produced. Your server is the download host (so corporate
allow-lists only need to trust your hostname, and `github.com` need not be
reachable from the endpoint), while the *version* your fleet converges on is the
upstream one. Rebuilding the server image does not change what agents install.

## Backups

The state that matters lives in two volumes: `mongo_data` (everything except
files) and `minio_data` (uploaded files).

```bash
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost \
  exec -T mongo mongodump --archive --gzip \
  -u "$MONGO_ROOT_USERNAME" -p "$MONGO_ROOT_PASSWORD" --authenticationDatabase admin \
  > roomler-$(date +%F).archive.gz
```

Keep `.env.selfhost` with the backup. Losing `ROOMLER_JWT_SECRET` invalidates
every session and every enrolled agent's token — the fleet would have to
re-enroll.

---

## No plan limits

A self-hosted server holds no organization to a plan limit: no device or user
count, no cap on concurrent remote sessions, no meeting length. Plans are sold only
by the hosted service, through a module the self-hosted image is built without.
Every organization still shows `free` in the database, because nobody sold it
anything; on this server that name carries no limits (`Core::plans_sold`,
[FR-115](fr/FR-115-pricing-reshape.md)).

## Known limitations, stated plainly

- **No prebuilt image yet.** First run compiles from source. Tracked in
  [FR-39](fr/FR-39-launch-readiness.md).
- **Single node.** The compose stack is one API instance. Multi-pod scale-out is
  supported by the code (Redis fan-out, leader-gated startup) but is a
  Kubernetes topology, not this file — see
  [`multi-pod-scale-out.md`](multi-pod-scale-out.md).
- **No automatic TLS.** Deliberate: terminate it in whatever you already run.
- **coturn is optional and off the critical path.** The DERP floor over the
  API's own port already guarantees connectivity when no direct path exists.
  That also means nothing fails loudly when it breaks: [check it](#how-to-tell-that-it-is-healthy).

## Getting help

[Open an issue](https://github.com/gjovanov/roomler-ai/issues). Include
`docker compose ... logs roomler | tail -100`, your OS, and whether you are
behind a reverse proxy — those three answer most of it.
