# Relay PoP — regional coturn + DERP on a cheap VPS

One VPS per region runs the whole relay stack: **coturn** (TURN over
UDP/3478, TCP/3478, TLS/5349, UDP/443-via-DNAT), the **derp-relay** binary
(ticket-authenticated regional DERP), and an **SNI-splitting nginx** so
TCP/443 serves both `turns:` (corp escape) and `wss:` (DERP) on one address.
Selection is client-probed RTT (no GeoDNS): agents receive the region list
over the control WS, time a STUN binding to each, and the server derives
their `relay_home`.

Secrets on the box: the TURN shared secret (bandwidth-theft blast radius
only) and the DERP ticket **public** key. The JWT secret and the ticket
**private** key never leave the API.

## Region matrix (procurement)

| Phase | Region id | Provider / location | Plan |
|---|---|---|---|
| 1 | `us-east` | OVH VPS-1, Vint Hill VA | ~$4.5/mo, unlimited @500 Mbps |
| 1 | `us-west` | OVH VPS-1, Hillsboro OR | ~$4.5/mo |
| 1 | `ca-east` | OVH VPS-1, Beauharnois QC | ~$4.5/mo |
| 2 | `eu-central` | existing fleet coturn (Vienna) | $0 |
| 2 | `eu-west` | OVH VPS-1, Gravelines FR | ~$4.5/mo |
| 2 | `eu-north` | Hetzner CX23, Helsinki | ~€5.5/mo, 20 TB |
| 3 | `jp-tokyo` | Vultr, Tokyo | $3.5–6/mo |
| 3 | `sg` | Contabo, Singapore | ~€5.5+surcharge, ~32 TB |
| 3 | `in-mumbai` | Vultr, Mumbai | $3.5–6/mo |
| 3 | `me-dubai` | LightNode, Dubai | ~$7.7/mo |
| 3 | `hk` | LightNode, Hong Kong (best-effort China adjacency) | ~$7.7/mo |
| 4 | `au-sydney` | Binary Lane, Sydney (also serves NZ) | ~AU$4.9/mo |
| 5 | `br-saopaulo` | Vultr, São Paulo | $3.5–6/mo |
| 5 | `mx-mexicocity` | Vultr, Mexico City | $3.5–6/mo |
| 5 | `za-johannesburg` | Vultr, Johannesburg | $3.5–6/mo |

Cost guards: bump a Vultr plan at 80% of included egress; any PoP whose
monthly egress+overage exceeds ~$25 graduates to an unmetered/32 TB provider
in the same metro (Hetzner auction EU, Contabo, ReliableSite US). Prices are
mid-2026 — re-quote at signup.

## Bring-up (per PoP)

1. **VPS + DNS.** Order the VPS (Debian 12/Ubuntu 22.04+). Create A records
   `coturn-{region}.roomler.ai` and `derp-{region}.roomler.ai` → its IP.
2. **Copy this kit** to the box and stage the daemon binary (built by CI or
   the build host — `cargo build -p derp-relay --release`):
   ```bash
   scp -r scripts/relay-pop root@<pop>:/opt/relay-pop
   scp target/release/derp-relay root@<pop>:/opt/relay-pop/bin/derp-relay
   ssh root@<pop> chmod +x /opt/relay-pop/bin/derp-relay
   ```
3. **Configure**: `cp /opt/relay-pop/pop.env.example /opt/relay-pop/pop.env`
   and fill in (region id, both hostnames, public IP, the TURN shared secret,
   the DERP ticket **public** key from the API's startup log line
   `derp ticket signer loaded — set DERP_TICKET_PUBLIC_KEY to this`).
4. **Provision**: `bash /opt/relay-pop/provision.sh` (idempotent — installs
   docker, sysctls, the UDP/443 DNAT, issues the LE cert via acme.sh
   standalone :80, renders configs, starts the compose stack).
5. **Verify from anywhere** — one line per advertised transport variant
   (STUN on udp/3478 + udp/443-DNAT + tcp/3478, STUN-over-TLS on 5349 and on
   443 through the SNI split, derp healthz). With `TURN_SECRET_FILE` set it
   also performs full TURN **Allocates** over udp/443 and TLS/443 (catches
   static-auth-secret drift and relay-address misconfig) plus a **relay-echo**
   — a udp/443 and a tls/443 allocation exchanging real bytes both ways —
   which catches allocate-succeeds-data-vanishes regressions like the
   missing-`relay-ip` loopback blackhole:
   ```bash
   TURN_SECRET_FILE=~/coturn.auth python3 scripts/relay-pop/healthcheck.py \
     us-east=coturn-us-east.roomler.ai:3478,derp-us-east.roomler.ai
   ```
6. **Register the region** — append to `ROOMLER__RELAY__REGIONS` in the prod
   configmap (deploy repo) and redeploy the web:
   ```json
   {"id":"us-east",
    "turn_url":"turn:coturn-us-east.roomler.ai:3478",
    "derp_url":"wss://derp-us-east.roomler.ai/derp",
    "caps":{"tls_443_udp":false}}
   ```
   `tls_443_udp: false` because that variant is DTLS and structurally dead
   here: the UDP/443 DNAT lands on coturn's **plain** 3478 listener (webrtc-rs
   drops the URL anyway; browsers would burn an allocate timeout on it). The
   other :443 caps stay on — `udp_443` rides the DNAT and `tls_443_tcp` rides
   the nginx SNI passthrough to coturn's TLS listener; both field-verified
   allocate-level on all four phase-1 PoPs (2026-08-17).
   Requires `ROOMLER__RELAY__REGIONS_ENABLED=true` and (for DERP)
   `ROOMLER__RELAY__DERP_TICKET_PRIVATE_KEY`
   (`openssl genpkey -algorithm ed25519 -outform DER | base64 -w0`).
7. **Watch**: `relay_region_pick_total` in `/api/cluster/status` (a platform admin's token),
   `GET /api/relay/regions`, `agents.relay_home` in Mongo / the Devices UI.
   Soak ≥1 week before the next procurement wave.

## Health cron (buildhost)

`TURN_SECRET_FILE` points at the fleet truth file so every run exercises real
Allocates; the trailing-comma `eu-central` row (no PoP DERP) keeps the fleet
coturn's own :443 DNAT under the same watch:

```cron
*/15 * * * * TURN_SECRET_FILE=/home/gjovanov/coturn.auth \
  python3 /opt/roomler/relay-pop/healthcheck.py \
  us-east=coturn-us-east.roomler.ai:3478,derp-us-east.roomler.ai \
  ... \
  eu-central=coturn.roomler.ai:3478, \
  >> ~/relay-pop-health.log 2>&1 || <alert: gh issue / mail>
```

## Per-org TURN bytes (FR-115 P4b-2)

coturn publishes each allocation's totals to a loopback-only Redis on the PoP
(the compose file's `redis` on `127.0.0.1:6390`, from `redis-statsdb` in
`turnserver.conf`), and derp-relay keeps them an hour (`TURN_STATS_REDIS`).
Every API pod pulls them on its `/stats` poll from `GET /stats/turn-usage`, under
the prefix nginx already proxies, and books each leg once across pods as the
paying org's `turn_bytes`.

If that Redis stops answering, derp-relay's subscribe gives up after 3 s, at the
connect or at a `PSUBSCRIBE`, logs
`derp-relay: subscribing to coturn's statistics failed` with an error such as
`no answer within 3000 ms (connect)`, and tries again 1, 2, 4 … 30 s apart. It
does not probe a subscription that is up: with the Redis on the same host, a
connection gone deaf is a frozen Redis, which coturn cannot publish to either.

That endpoint is **not** public, unlike `/stats`, because the usernames name
orgs and principals. The API signs each request with the DERP ticket key for
this PoP's `REGION`, and derp-relay checks it with the public key it already
holds, so there is still no secret on the PoP. `GET /stats` reports
`turn_usage` (an object) only when totals are kept. That object is how the
API knows to pull.

| Switch | Turns off |
|---|---|
| `TURN_STATS=0` in `pop.env`, then re-provision | this PoP's statsdb line and derp-relay's collector; Redis idles |
| `ROOMLER__RELAY_ALLOWANCE__TURN_ATTRIBUTION=false` on the API | every pull, and the cluster's collector |

Neither touches TURN itself: a coturn whose Redis is down, unresolvable or
restarting keeps relaying, and only its totals are lost.

## A relay that serves only some networks (FR-76 P3)

The regional PoPs serve every overlay network, and they must go on doing so,
because some hosts on them have no carrier but DERP. A derp-relay that should
serve only some networks, such as a tenant's own node, names them in
`DERP_NETWORKS`: a comma-separated list of overlay network ids, each the 24
lowercase hex characters the API mints into a ticket's `net`.

| `DERP_NETWORKS` | derp-relay |
|---|---|
| unset | serves every network, exactly as before. Every regional PoP runs this way |
| `6a54…,6a55…` | serves only those networks. A valid ticket for any other is answered `403` at the upgrade, before anything is registered. The node logs `refused a valid ticket for a network this relay does not serve` with the ticket's network, its own networks and a running total, at most once per network per 10 s |
| set but empty, or anything but such ids | refuses to start, with an error that names the variable. A node that cannot read its scope serves nobody, and the operator sees it at deploy time |

The match is exact: a prefix of a served id, a longer id or the same id in
capitals is another network. A scoped relay logs
`serving only the overlay networks DERP_NETWORKS names`, with its list, at
start. An unscoped relay logs nothing new.

⚠️ `docker-compose.pop.yml` does not pass `DERP_NETWORKS`, and it must never
pass it as `${DERP_NETWORKS:-}`, the way it passes `TURN_STATS_REDIS`. Compose
would then hand every regional PoP an empty value, and an empty value refuses to
start. A node that serves some networks writes the list as a literal in its own
`derp-relay` environment.

## The admission ticket and the access log (#3656)

`derp-relay` takes a node's ticket as `Authorization: Bearer <ticket>` and, for
every daemon older than 0.4.210, from the query (`/derp?ticket=…`). When a dial
carries both, the header decides. A refusal is a `401` whose
`WWW-Authenticate: Bearer realm="roomler"` says whether a ticket arrived
(`error="invalid_token"` when one did), so a newer daemon dials again with the
query form only after a refusal that names no `error`: a relay older than the
header (it answers a header-only dial `400`, no challenge), or a header lost on
the way.

The PoP's nginx logs `$uri`, never `$request`: the path without the query, and
no request header (`log_format pop` in `nginx-pop.conf.tpl`). nginx's error log
still names the whole request line when it cannot reach the relay, so a ticket
in a URL can land there while `derp-relay` is down; a ticket lives 24 h.

**Taking a new template or binary onto a PoP.** Copy the kit and the binary as in
step 2 of the bring-up, then re-run `bash /opt/relay-pop/provision.sh`. It renders
`nginx-pop.conf` again and, when the render changed, checks it with `nginx -t` and
reloads nginx. A reload keeps the live DERP sockets and TURN-over-TLS splices on the
old workers until they close. A new `derp-relay` binary takes effect with
`docker compose -f docker-compose.pop.yml restart derp-relay`, which drops the
PoP's DERP sockets; the nodes reconnect at once.

## Generation floors (FR-155 P1e)

Since FR-155 P1e a node's ticket carries the token generation of the control
socket it was minted to (`gen`), and the API tells every PoP that takes them the
generation floors of the devices whose token rotated
([the spec](../../docs/fr/FR-155-one-row-new-credentials.md#every-regional-relay-holds-a-and-b-for-the-devices-generation-p1e)).
While the token rotation is on (`rc.agent_token_rotation` and
`rc.token_generation_check` on the API), a PoP:

- admits a registration only at the device's current generation;
- ends a registration below it before the rotation's route answers when the
  commit's push reaches it, and otherwise at the API's next poll (every 30 s);
- keeps, for each key, the registration of the higher generation, while the
  API's last set is live.

A floor is keyed by what the registry already keys by, the overlay network and
the WG public key, so a PoP learns no new identifier and logs no agent id.

| Part | Where | What it does |
|---|---|---|
| The push | `POST /stats/floors`, under the prefix nginx already proxies (`floors_push`, `crates/derp-relay/src/main.rs:996`) | the API's live floors as one EdDSA JWT signed with the ticket key (audience `derp-floors`), with the set's `ttl`, three polls. `204` when merged, `401` when it does not verify, `400` when the body is not text. A refused set changes nothing |
| The merge | `Floors::merge` (`crates/derp-relay/src/main.rs:367`) | entry by entry, by the higher generation and the later `until`, so an older, repeated or partial set lowers no floor and ends none sooner. A floor holds while `now <= until` on the PoP's clock; the API sets `until` past every ticket below the floor |
| The refusal | the upgrade (`crates/derp-relay/src/main.rs:1243-1253`) and the registration (`admit`, `:509`) | a ticket below its key's floor, or one without `gen` for a key with a floor, is answered `401` at the upgrade, as a ticket that does not verify is, and refused at the registration, under the key's registry entry. A refusal writes nothing, and the registration in place stays and keeps relaying |
| The end | `end_below` (`crates/derp-relay/src/main.rs:537`) | a set that raises a floor ends each live connection below it, through the socket's own close |
| The higher generation | `admit` (`crates/derp-relay/src/main.rs:509`) | while a set is live (one arrived within its `ttl`), a registration below the generation of the one in place for its key is refused. An equal or a higher one replaces it. With no live set, a re-registration is last-writer-wins, as before P1e |
| A restart | `DERP_FLOORS_FILE` (`crates/derp-relay/src/main.rs:955-965`): `/var/lib/derp-relay/floors.json` in the container, `/opt/relay-pop/state/floors.json` on the host | every change is written to `floors.json.tmp` and renamed over the file, which the start reads, so a PoP holds its floors before the API's first push |

A PoP that hears no API keeps admitting every ticket at or above its key's floor,
and every ticket for a key with none. With the switch off the API pushes no set,
so a PoP holds no live set and refuses no ticket for its generation; a floor it
already holds lasts to its `until`.

`GET /stats` reports `floors`, counts and a time, never a key. Its presence is
how the API knows the PoP takes floors: a PoP whose `/stats` has no `floors`
runs a relay from before P1e, and the API pushes it nothing.

| Field | What |
|---|---|
| `held` | the floors held whose `until` has not passed |
| `newest_iat` | the `iat` of the newest set merged (unix seconds), `null` before the first |
| `set_live` | whether a set arrived within its `ttl` |
| `kept` | `file` with `DERP_FLOORS_FILE` set, `memory` without |
| `refused_total` | tickets refused at the upgrade and registrations refused, since the process started |
| `ended_total` | connections ended by a floor that arrived, since the process started |

| Event | Level | Message | Fields |
|---|---|---|---|
| the start read the file | INFO | `derp-relay: holding the generation floors its file keeps` | `held`, `path` |
| the file cannot be read, or does not parse | WARN | `derp-relay: the floors file is unreadable; holding none until the API's next push`, or `… does not parse; …` | `path`, `e` |
| `DERP_FLOORS_FILE` is unset | WARN | `DERP_FLOORS_FILE is unset: the generation floors are kept in memory only, and a restart holds none until the API's next push` | |
| a write failed | WARN | `derp-relay: the floors file could not be written` | `path`, `e` |
| a set raised floors | INFO | `derp-relay: generation floors rose` | `raised` |
| a set ended connections | INFO | `derp-relay: connections below a generation floor that arrived end` | `ended` |
| each connection it ends | INFO | `derp-relay: a registration below its key's generation floor ends` | `network` |
| a registration refused | INFO | `derp-relay: registration refused` | `network`, `reason` (`below its key's generation floor`, or `below the generation of the registration in place`) |
| a ticket refused at the upgrade | DEBUG | `derp-relay: a ticket below its key's generation floor` | `network` |
| a set that does not verify | DEBUG | `derp-relay: refused a floor set` | `e` |

**The roll: every PoP first, then the API's pods.** On each PoP, stage the kit
and the binary as in step 2 of the bring-up, then re-run
`bash /opt/relay-pop/provision.sh`. It creates `/opt/relay-pop/state`, and its
`docker compose up -d` re-creates `derp-relay` with the variable and the volume,
which drops the PoP's DERP sockets once; the nodes reconnect at once. A
`restart` alone keeps the container's old environment, without the file. Then
read the PoP's `GET https://derp-{region}.roomler.ai/stats`: `floors` with
`"kept": "file"`. A relay from before P1e verifies a ticket that carries `gen`,
and a P1e relay admits a ticket without it for a key with no floor, so pods and
PoPs of either release work together while the switch is off. Going back to an
older binary needs nothing else. P1f, the switch on, comes after every regional
PoP runs this relay.

## The drop lines (FR-148 D52)

`derp-relay` drops a frame it cannot pass on, as the central relay does, and
since FR-148 D52 it counts every drop past the frame-size check and logs it,
paced, so a T3 on the mesh relay that no node's own count explains can be
looked for here ([every drop on the relay path](../../docs/overlay-communication.md#every-drop-on-the-relay-path-and-where-it-reports-fr-148-d52)).

| Drop | Level | Message | Fields | Paced |
|---|---|---|---|---|
| a bulk frame of an org past its plan's monthly relay (FR-115) | INFO | `derp-relay: bulk frame dropped by the relay allowance (the org is past its plan's monthly relay)` | `network`, `rate_bytes_per_sec`, `derp_allowance_shaped_total` | once per network per 10 s |
| a frame for a destination with no registration here: the peer is offline, or registered on another relay | INFO | `derp-relay: dropped a frame for an unregistered dst (peer offline, or registered on another relay)` | `network`, `src`, `dst`, `derp_miss_total` | once per network and destination per 30 s |
| a frame the destination's send queue refused: full (a slow or half-open consumer), or closed by a socket on its way out | WARN | `derp-relay: dropped a frame on a full destination queue (slow or half-open consumer)` | `network`, `src`, `dst`, `derp_full_total` | once per network and destination per 30 s |

`src` and `dst` are the first 8 hex characters of each key, never a whole key,
so a line here matches the nodes' lines and the central relay's. A total counts
every drop of its cause since the process started, logged or not; a line
between two others says how many fell between. The two forward drops are
counted and paced in `ForwardDrops` (`crates/derp-relay/src/main.rs:1417`,
pacing `drop_log_due`, `:1464`); each cause keeps the last line's time for at
most 4,096 network and destination pairs, and a pair past that waits for a
sweep, at most one a window. A frame shorter than 32 bytes or longer than
2,048 is still dropped without a count.

⚠️ **There is no census line.** `derp-relay` writes no periodic line, and
`GET /stats` does not carry these totals, so the drop lines are the only place
they show. Read them in the relay's log:
`docker compose -f docker-compose.pop.yml logs derp-relay | grep 'dropped a frame'`.

## Rollback

A misbehaving region: set its spec `"enabled": false` (or drop it) and
redeploy — grants fall back to the default region within the 10-min sticky
window; agents keep their probe tables and simply re-home on the next push.
Global off-switch: `ROOMLER__RELAY__REGIONS_ENABLED=false` restores the
single-region behaviour byte-for-byte.
