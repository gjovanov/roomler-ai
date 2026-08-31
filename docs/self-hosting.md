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

---

## Quickstart

**You need** Docker with Compose v2, ~4 GB RAM, and ~10 GB disk.

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
docker compose -f docker-compose.selfhost.yml --env-file .env.selfhost up -d --build
```

> **The first build compiles the Rust server and mediasoup from source.**
> Measured **6 minutes** on a 2024 laptop (16 cores, Docker Desktop + WSL2);
> budget 15–20 on a small VPS or 2–4 cores. Subsequent starts are seconds.
> A prebuilt image — which would make this a 60-second pull instead — is tracked
> in [FR-42](fr/FR-42-selfhost-verified-on-a-clean-box.md); until it ships,
> `--build` is the only path.

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

Mint an enrollment token in the web app (**Devices → Enroll device**), then on
the machine you want to reach:

```bash
# Linux / macOS
curl -fsSL http://<your-host>:8080/api/setup/install.sh | sh -s -- \
    --role daemon --server http://<your-host>:8080 --token <enrollment-jwt>

# Windows (PowerShell, elevated)
& ([scriptblock]::Create((irm http://<your-host>:8080/api/setup/install.ps1))) `
    -Role daemon-system -Server http://<your-host>:8080 -Token <enrollment-jwt>
```

⚠️ **`--server` / `-Server` is not optional on a self-hosted instance.** Both
installers default to `https://roomler.ai` (`scripts/install.sh`'s `SERVER=`,
`scripts/install.ps1`'s `$Server`), and they are piped from the network, so
neither can see which host you fetched it from. Omit it and the agent downloads
from you and then tries to enroll against the *hosted* service with a token only
your server can verify — failing with an authentication error that says nothing
about the real cause. Use the same URL you set as `ROOMLER_PUBLIC_URL`.

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

Terminate TLS in front of the stack and point it at `ROOMLER_HTTP_PORT`. The app
needs WebSocket upgrades on `/ws` and `/derp`, and both are long-lived — set
generous read timeouts or sessions will be cut every 60 seconds.

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

## Getting help

[Open an issue](https://github.com/gjovanov/roomler-ai/issues). Include
`docker compose ... logs roomler | tail -100`, your OS, and whether you are
behind a reverse proxy — those three answer most of it.
