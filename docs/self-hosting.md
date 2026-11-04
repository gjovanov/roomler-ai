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
in `.env.selfhost`, with the `<tag>` that `docker-compose.selfhost.yml` names) or by
**`ROOMLER_PROFILE`** when you build from source. No published
`v*` image carries the hosted service's billing and newsletter module — that is what `roomler.ai`
runs, not what you run.

> The same package also holds the **`hosted-<date>-<sha7>`** tags and the moving `hosted`
> pointer: the image `roomler.ai` itself deploys, built from every merge to `master` by the
> `hosted image` workflow (FR-73). It is the `full` composition **plus** the billing module,
> holds no secret (everything is configuration), and is public because the source is — but it
> is not a release: it is whatever `master` was an hour ago, with the Stripe webhook and the
> newsletter routes mounted. Pull a `v*` tag; leave `hosted-*` to the hosted service.

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
> smoke-tested there before publication — neither is emulated. The documented path
> itself runs on both as well: the `Self-host pull` lane pulls anonymously into an
> empty cache, runs `up -d`, and makes a two-party call directly and over the relay
> (`scripts/selfhost-pull-check.sh`, on every pull request that changes a compose
> file, and weekly).

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
- the RTC port range — 32 ports are mapped by default, enough to try it out. To
  widen it, change `40000-40031` in the `roomler` service's `ports:` and
  `ROOMLER__MEDIASOUP__RTC_MAX_PORT` together. Docker runs a proxy process per
  mapped port by default, so map what you need rather than all of 40000–49999.

⚠️ Don't give the `roomler` service `network_mode: host` with this compose file.
The compose service names stop resolving on the host network, so the server loops
on `Server selection timeout … Address: mongo:27017` and the web port answers 502.

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
ports held for up to 13 minutes, because the client does not end what it allocates over UDP.

| It says | What is wrong |
|---|---|
| `coturn is not staying up` | A crash loop. See below. |
| `coturn is running outside the roomler service's namespace` | The server's process died moments ago, and coturn has not found its way back yet. See [if the server's process dies](#if-the-servers-process-dies). |
| `no relay allocation` | Nothing answers on 3478. Check the `roomler` service's ports, and that it is running. |
| `coturn has no free relay port` | Under any line. Allocations hold all 101 ports, so no client can get a relay until one ends, and coturn logs `create_relay_ioa_sockets: no available ports` as it refuses them. A call's participant holds up to 4 on Chrome and 6 on Firefox, so about 16 to 25 participants at once fill them. See [sizing the range](#sizing-the-range) and [when the range is full](#when-the-range-is-full). |
| `coturn hands out … but its --external-ip is …` | The relay address is not the public address. Clients would be told the wrong place. |
| `CreatePermission to the public address … is refused` | No call can use this relay: every browser asks for this first. coturn logs `A peer IP … denied in the range`, because it rewrites the public address to its own before the deny list reads it. An edited file has lost the `--allowed-peer-ip` line. |
| `relay to relay carried nothing` | Two relayed clients cannot reach each other. |
| `does not run in the server's network namespace` | An edited file has lost `network_mode: service:roomler`. A relay to the public address lands on coturn itself, so a call signals and carries no media. |
| `NOT refused` | The deny list has a hole. See [what it refuses](#what-the-relay-refuses). |
| `could not ask about` | coturn gave no answer about that peer, so this proves nothing about the deny list. The lines under it say why. |
| `NOTE the relay address is 127.0.0.1` | Not a failure, but only this machine can use it. See [the two addresses](#the-two-addresses-to-set). |
| `NOTE coturn was not running, so this started it` | It had stopped, and the note says why. See [if the server's process dies](#if-the-servers-process-dies), [if the server is replaced](#if-the-server-is-replaced) and [after a reboot](#after-a-reboot-or-a-docker-restart). |
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

A server that crashes within seconds of starting can cost the relay the same way. coturn
takes about 5 s to end once its healthcheck fires, and Docker tries to restart it once. If
the server is still coming back at that moment, the attempt fails with
`cannot join network namespace of container: … is restarting`, and Docker does not try
again. In 10 crashes in a row, each as soon as the server was back, coturn was lost by the
second, and it stayed `exited` after the server had recovered, until a plain `up -d`. A
server that had run for at least 12 s healed wherever its crash fell: 6 of 6, with the kill
placed from 0.7 to 8 s before coturn's next probe.

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

⚠️ The server hands out more TURN URLs than this coturn serves, and this file opens only
3478, over UDP and TCP, with no TLS. A call's browsers get four
(`VariantCaps::media`, `crates/remote_control/src/turn_url.rs:68`): `turn:…:3478`,
`turn:…:443?transport=udp`, `turn:…:3478?transport=tcp` and
`turns:…:5349?transport=tcp`. Remote control, the overlay and tunnels get six
(`VariantCaps::default`, `turn_url.rs:53`): the same, plus `turns:` on 443 over UDP and
over TCP. A failed allocation that names port 443 or 5349 is expected here and costs a
client time, not the relay: nothing answers there, so nothing is allocated and no
[relay port](#relay-ports) is held.

### What the relay refuses

Anyone with an account on your server can get a TURN credential, so the relay must not
be a way into the machines around it. With the shipped flags it answers 403 to a peer in:

| Range | Refused by |
|---|---|
| `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` | the `--denied-peer-ip` lines |
| `100.64.0.0/10`, the range overlay meshes use | the `--denied-peer-ip` lines |
| `169.254.0.0/16`, including the cloud metadata address | coturn 4.18.0 itself |
| loopback, `0.0.0.0`, `255.255.255.255` | coturn itself |
| multicast | `--no-multicast-peers` |

It has no IPv6 relay: a client that asks for one gets 440, so no IPv6 peer is reachable
through it either.

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
all 101 held, coturn answers 508 to the next allocation, and that client gets no relay:
see [when the range is full](#when-the-range-is-full).

A browser in a call opens two peer connections, one that sends and one that receives, and
each asks the relay for one allocation for every TURN URL that answers. On this file two
answer: 3478 over UDP and 3478 over TCP. Media rides a UDP allocation, and the TCP one is
held unused. What one participant held, measured on this file with coturn 4.18.0, the
browsers on a machine other than the server's:

| Participant | Relayed, measured with the relay forced | On a direct path that works |
|---|---|---|
| Chrome 151 | up to 4 for each network that reaches the server, 2 to 4 after the first minute | none |
| Firefox 153 | 6 | 6 |

| Call | Relay ports held |
|---|---|
| 2 × Chrome | 8, then 7 (6 over a 40 ms round trip) |
| 2 × Chrome, each with a second network | 12, then 9 to 10 |
| 3 × Chrome | 12, then 10 |
| 2 × Firefox | 12, relayed or not |
| 2 × Chrome and 2 × Firefox | 20 |
| 2 × Chrome, relay not forced | 0 |

Hanging up gives them all back within 3 s. The counts differ for three reasons:

- **Chrome stops asking once its first path connects.** An allocation still on its way at
  that moment is one it no longer wants, and it is freed about 45 s into the call. That is
  most often a TCP one, so a Chrome participant settles at 2 to 4. When its direct path
  works, that path connects first and Chrome holds no allocation at all: over a LAN and
  over a 40 ms round trip alike.
- **Firefox asks for everything at once and keeps it**, relayed or not: 6 for the whole
  call, two more than its two connections account for, and it reports only 4 of them.
- **Chrome allocates from every network that reaches the server.** A second one adds its
  UDP allocations again: measured with a second interface, and with Docker's bridge on a
  Linux machine that runs a container, whose traffic leaves through the first network.
  Firefox did not use the second interface in the same test.

Safari was not measured: there was no macOS machine in the test.

### Sizing the range

Add up what the calls in progress hold, and what clients that vanished still hold
([below](#what-a-vanished-client-leaves-behind)):

```
relay ports ≈ 4 × relayed Chrome participants × their networks
            + 6 × Firefox participants, relayed or not
            + 4 × Chrome participants who left without hanging up, in the last 10 minutes
            + 6 × Firefox participants whose machine dropped off the network, in the last hour
              (3 if only its browser died)
            + 9 for each run of the check in the last 13 minutes
```

The 101 ports hold about 25 relayed Chrome participants at once, or 16 on Firefox.
⚠️ Firefox users use up the range even on direct calls, so about 16 Firefox participants at
once, in all calls together, is this file's real ceiling: a meeting of 17 on Firefox fills it
while nobody's media is relayed.

⚠️ A Firefox participant whose machine drops off the network holds its 6 relay ports for up
to an hour, TBD-FFHOLD-SHORT. A Chrome one holds up to 4 for up to 10 minutes. So the range
can be full after a meeting has ended, if its network went down under it.

### When the range is full

No participant is told. Only coturn's log says so, at this file's own log level:
`create_relay_ioa_sockets: no available ports`, as it refuses them.
Measured twice with coturn narrowed to 6 ports in a lab copy of this file, the relay forced,
and three Chrome participants: the range filled as the third joined, and each connection that
asked too late got no relay at all.

| Participant | Sending | Receiving | What it had |
|---|---|---|---|
| first | relayed | no relay | it was seen, and saw nobody |
| second | no relay | relayed | it saw the first, and nobody saw it |
| third | no relay | no relay | nothing either way |

All three stayed in the call with every tile in place, and no page showed an error. Nothing
recovered: in both runs one port came free 45 s in and nobody took it, and when the first
participant left and freed 3 more, the other two were as they had been 60 s later. A
connection that gathered no relay never asks again, and the app does not restart one
(`ui/src/stores/conference.ts:272` only logs the change). Joining the call again makes new
connections, and they ask the relay again.

### What a vanished client leaves behind

Hanging up gives every port back within 3 s, and so does Firefox when its tab or the whole
browser is closed. Every other way out leaves ports held until their lifetime runs out: what
the client asked for, capped by coturn's `--max-allocate-lifetime` (3600 s by default, in
coturn 4.18.0's own `--help`; the file does not set it). A TCP allocation ends when its
connection does, so it outlives its client only when the client's machine is gone from the
network. Measured on this file in a four-party call, once on Chrome and once on Firefox, with
each participant leaving a different way, and in a three-party call where a Chrome and a
Firefox participant closed the call's tab:

| How the participant left | Chrome 151 | Firefox 153 |
|---|---|---|
| hung up | all freed within 3 s | all freed within 3 s |
| closed the tab, the browser still running | TCP freed at once, UDP held | all freed within 3 s |
| closed the browser | TCP freed at once, UDP held | all freed within 3 s |
| browser killed | TCP freed at once, UDP held: 2 for each network | TCP freed at once, its 3 UDP held |
| machine gone from the network | all held: up to 4 for each network | all 6 held |
| held for | 600 s after its last refresh, so 1 to 10 minutes: it asks for 600 s and refreshes every 540 s | 3600 s after its last refresh, so up to an hour: it asks for 3600 s |
| measured | every port left behind was freed 600 s after it was allocated | TBD-FFHOLD |

### Widening the range

Change `--min-port`, `--max-port` and the `49152-49252` line under `roomler`'s `ports:`
together, then run the check. To free the range now instead, `restart coturn` ends every
allocation, including those of calls in progress.

The file keeps 101 on purpose: every port more costs every install memory and start time.
Docker sets up every published port each time the server starts, and unless its userland
proxy is off it runs two `docker-proxy` processes for each, one for IPv4 and one for IPv6.
Measured on a fresh install with 4 vCPUs and 4 GiB, Docker 29.1.3 and Compose 2.40.3, each
size started from `down`, with the whole machine's memory in use as `free` reports it:

| Relay ports | Docker's userland proxy | `docker-proxy` processes | Memory in use | `up -d` | Server restart to `/health` | Docker restart to `/health` |
|---|---|---|---|---|---|---|
| 101, this file | on, Docker's default | 336 | 1.2 GiB | 14 s | 9 s | 10 s |
| 501 | on | 1,136 | 2.3 GiB | 22 s | 33 s | 57 s |
| 1001 | on | 2,136 | 3.6 GiB, 192 MiB left | 37 s | 73 s | 106 s |
| 101 | off | 0 | 0.7 GiB | 13 s | 10 s | 11 s |
| 1001 | off | 0 | 0.7 GiB | 35 s | 90 s | 133 s |
| 3001 | off | 0 | 0.8 GiB | 163 s | 488 s | 687 s |

The 336 are two for each of the file's 168 published ports: the relay's 101, the SFU's 32
over UDP and 32 over TCP, 3478 over UDP and over TCP, and the HTTP port. With the proxy on,
each relay port costs two processes and about 2.8 MiB, so each 100 ports more cost about
280 MiB and hold about 25 more relayed Chrome participants, or 16 on Firefox. The time
grows faster than the range: a server restart took 9 s at 101 ports, 33 s at 501, 73 to
90 s at 1001 and 8 minutes at 3001.

The userland proxy is a setting of the whole engine, for every container on the host:
`{ "userland-proxy": false }` in `/etc/docker/daemon.json`, then
`systemctl restart docker`. Docker then runs no process per port. That takes the memory
away, not the time: the restart time grows with the range either way.

⚠️ A server restart is also what a crash costs. The healing numbers in
[if the server's process dies](#if-the-servers-process-dies), coturn back within 20 s and a
server that has run for 12 s healing wherever its crash falls, were measured at 101 ports. A
wider range makes every restart slower, so measure them again after widening.

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
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost pull
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d
```

`git pull` is the step that moves the [pinned images](#what-is-pinned-and-what-that-costs-you);
`pull` alone fetches what the file you already have names. A source install replaces the last
two lines with `up -d --build`.

Schema migrations run at startup and are leader-gated, so it is safe to restart
into a newer build. They only run forward, so take a [backup](#backups) before a
large jump.

Run `up -d` for the whole file, never for `roomler` alone: that
[leaves the relay dark](#if-the-server-is-replaced).

⚠️ **Agent updates are a separate track from your server.** Enrolled agents ask
*your* server for the current release, but that endpoint proxies the upstream
[GitHub releases](https://github.com/gjovanov/roomler-ai/releases) — it does not
serve builds you produced. Your server is the download host (so corporate
allow-lists only need to trust your hostname, and `github.com` need not be
reachable from the endpoint), while the *version* your fleet converges on is the
upstream one. Rebuilding the server image does not change what agents install.

### What is pinned, and what that costs you

Every image in `docker-compose.selfhost.yml` names an exact version, so a fresh
install gets the bytes this file was tested with. A moving tag broke fresh installs
twice: `minio/minio` left Docker Hub (#2034), and a new `coturn/coturn:latest`
dropped a flag this file passed (#3287).

| Service | Image | Pinned by |
|---|---|---|
| `mongo` | `mongo:7.0.45` | digest |
| `redis` | `redis:7.4.12-alpine` | digest |
| `minio`, `minio-init` | `ghcr.io/gjovanov/minio:RELEASE.2026-09-22T19-25-18Z` | digest, on our own registry |
| `coturn` | `coturn/coturn:4.18.0-r0` | digest |
| `roomler` | `ghcr.io/gjovanov/roomler-ai:v0.4.197` | exact tag |

**The cost.** Upstream fixes, security fixes included, reach you only when this file
moves. Nothing updates on its own: `git pull` moves the pins, and `pull` then fetches
what they name. If you need a MongoDB or coturn fix before this file has it, move the
line yourself (below).

**A digest works on both architectures.** It names the multi-architecture index, so
amd64 and arm64 hosts resolve the same line to their own image.

**The Roomler image has a tag and no digest** because `up -d --build` cannot tag a
build with a digest: Compose stops with `failed to solve: build tag cannot contain a
digest`. The source build is the licence's promise, so it wins. The cost is that a tag
is only as fixed as our promise not to re-push it. `ROOMLER_IMAGE` in `.env.selfhost`
overrides the default, and accepts a digest if you never build from source. While it is
set, `git pull` no longer moves your server image: you move it.

**To move a pin yourself**, read the digest and edit the `image:` line:

```bash
docker pull mongo:7.0.46    # prints "Digest: sha256:…"
```

```yaml
image: mongo:7.0.46@sha256:…
```

A local edit to that line will conflict on your next `git pull` if we moved it too.

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
