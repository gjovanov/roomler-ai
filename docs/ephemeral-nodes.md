# Ephemeral nodes — devices that remove themselves

**FR-51** ([#1095](https://github.com/gjovanov/roomler-ai/issues/1095), spec:
[`docs/fr/FR-51-ephemeral-nodes.md`](fr/FR-51-ephemeral-nodes.md)). The roomler
answer to Tailscale's ephemeral nodes: a device that joins an org **knowing in
advance that it is temporary**, and then removes itself — device row, overlay
lease, address, MagicDNS name — shortly after it stops. For CI runners,
containers, autoscaled workers, preview environments: anywhere the device set
turns over faster than a human can curate it.

## The lifecycle

```
admin mints an EPHEMERAL ENROLLMENT KEY  (reusable, capped, revocable, audited)
        │
        ▼
roomlerd enroll --server … --token <key> --name runner-1 --ephemeral
        │   random machine fingerprint → a NEW device row, ephemeral: true
        ▼
roomlerd run          … the device works like any other …
        │
        ├── clean stop (SIGTERM/SIGINT, e.g. `docker stop`)
        │       → the daemon calls POST /api/agent/self/unenroll
        │       → removed within seconds
        │
        └── unclean stop (SIGKILL, power loss, network partition)
                → the server-side reaper removes it once it has been
                  silent past its TTL (default 15 min, 60 s floor)
```

Removal is **final and complete**: the device row is hard-deleted (not
tombstoned), the overlay address returns to the org's pool, the MagicDNS name
frees, and peers receive a netmap delta. **A restart is a NEW device** — a
fresh random fingerprint, a fresh address, a fresh name. Nothing persists
across a cycle; that is the contract, not a limitation.

## Switches — everything defaults OFF

| switch | where | default | what it gates |
|---|---|---|---|
| `ephemeral_keys_enabled` | org settings (`PUT /api/tenant/{tid}/ephemeral-key-settings`, or Settings → "Ephemeral enrollment keys") | **off** | whether keys can be minted — and whether any existing key still works (the gate is re-checked on every use, so flipping it off is an org-wide revocation that burns nothing) |
| `rc.ephemeral_reaper_enabled` | server config (`ROOMLER__RC__EPHEMERAL_REAPER_ENABLED`) | **false** | the reaper task — off means zero queries, zero deletes, deployment-wide |
| `rc.ephemeral_default_ttl_secs` | server config | 900 | the inactivity deadline for devices whose key set no override |
| `rc.ephemeral_reap_interval_secs` | server config | 60 | reap cycle cadence (cluster-singleton per cycle) |

Even with the reaper on, the predicate (`ephemeral: true`) structurally cannot
match a device that did not enroll ephemeral — every pre-FR-51 row deserialises
permanent, and nothing after enrollment can flip a device in either direction.

## Ephemeral enrollment keys

The single-use enrollment token (10 minutes, once, minted by a human) is
deliberately unusable for autoscaling, so ephemeral devices enroll with a
**second credential kind**: a reusable key. A reusable key is a standing
credential that mints device identities inside your org — treat it like any
other CI secret. Four controls make it acceptable, and they are all structural:

1. **Use ceiling** — `max_uses` (default 100, cap 10 000), claimed atomically:
   racing replicas cannot overshoot it.
2. **Absolute expiry** — default 30 days, cap 90; enforced on the server row
   as well as inside the credential.
3. **Revocability** — `DELETE /api/tenant/{tid}/agent/enroll-key/{id}` (or the
   Devices page). Takes effect on the very next use. Expiry alone would mean a
   leaked key can only be waited out; revocation means it can be *stopped*.
4. **Per-use audit** — every enrollment writes an `enrollment_key_uses` row
   (90-day TTL) that also records the device's *removal* (when, and by which
   path), so "which key created this device, and where did it go" stays
   answerable **after** the device row is gone.

Mint: `POST /api/tenant/{tid}/agent/enroll-key` (needs `MANAGE_AGENTS`; the
org switch needs `MANAGE_TENANT` — deciding the credential class exists is an
org-owner decision, minting within it is fleet administration). The response's
`key` is shown **exactly once** and is not stored; the list can never return it.

The ephemeral property **rides the credential, never the request body**: a
device cannot declare itself permanent (and evade the reaper), and a permanent
device cannot be flipped ephemeral (and be scheduled for silent deletion) by
anything the host says.

## The CI / container recipe

One key in the secret store; each job or replica enrolls itself and cleans up
after itself:

```bash
# entrypoint.sh — each replica is its own device
roomlerd enroll \
  --server https://roomler.ai \
  --token  "$ROOMLER_EPHEMERAL_KEY" \
  --name   "runner-${HOSTNAME}" \
  --ephemeral
exec roomlerd run
```

- `--ephemeral` mints a **random** machine fingerprint. Without it, N replicas
  of one image share hostname+OS+arch+path, derive the SAME fingerprint, and
  the server refuses the collision (an ephemeral enrollment never revives or
  takes over an existing row — a key holder posting a real device's
  machine_id gets a final 409).
- `--ephemeral` **refuses if a config already exists** at the target path.
  Containers get a fresh filesystem for free; on a real host, point `--config`
  at an empty path.
- With a **standard** token, `--ephemeral` warns loudly and the device enrolls
  PERMANENT (under a random fingerprint) — the credential decides, and the
  config records the server's answer.
- `docker stop` sends SIGTERM → the daemon de-enrolls within seconds. The
  auto-updater's internal restart does **not** de-enroll (that would delete
  the device on every update).
- `roomler status` on an ephemeral device says so
  (`ephemeral   yes — removes itself after inactivity, or on clean stop`).

## Quota

An ephemeral device consumes a `max_devices` slot **while it exists** — the
reaper is what gives slots back. Until it runs, yesterday's runners are
today's quota, which is why the reap deadline defaults short (15 min) and why
the TTL floor is 60 s (a network blip or a pod roll must not read as "the
device left").

## What this is deliberately NOT

- **Not a stateless daemon.** `roomlerd` persists its config (WG key, SSH host
  key); Tailscale's `--state=mem:` has no roomler equivalent. The
  refuse-if-config-exists guard plus a container's fresh filesystem covers the
  motivating case without one.
- **Not tunnel clients (yet).** `tunnel_clients` rows stay permanent: the
  ephemeral credential is its own JWT audience, and the tunnel enrollment
  deliberately refuses foreign audiences — extending ephemerality there means
  a second credential kind, built when a consumer exists (FR-51 §7 decision 5,
  recorded on the issue).
- **Not a ban mechanism.** Removal frees the identity; the binary can enroll
  again (as a new device) with any valid credential, exactly like an evict.
- **Not exempt from ACLs, consent, or any gate.** An ephemeral device is an
  ordinary device until it vanishes.

## Operational notes

- The reaper is a **cluster singleton** per cycle (Redis `NX` claim, the
  presence sweep's shape) and **aborts the cycle if the presence directory is
  unreachable** — an unreadable directory must not let one pod reap agents
  that are alive on another.
- A reaped-but-still-running device (unclean-stop survivor, network partition)
  hears **401** on its next control-plane call — its credential died with the
  row — and its self-unenroll treats 401/404 as "already gone".
- Every reap logs
  `ephemeral device reaped` with tenant, device, silence and TTL; every
  self-removal logs `ephemeral device unenrolled itself`. The lifecycle also
  lands on the key's use-row (`removed_at` + `removal`:
  `ephemeral_expired` / `ephemeral_self_unenroll` / `agent_delete`).
- The device grid badges ephemeral devices; the vanishing must never be a
  surprise.
