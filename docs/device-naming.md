# Device naming — fleet names, display names, tags, and the MagicDNS label

Two name namespaces exist per device, denormalized once at overlay join:

| Namespace | Field | Written by | Consumed by |
|---|---|---|---|
| **Fleet name** | `Agent.name` / `TunnelClient.name` | enrollment (machine-reported) + the admin rename routes | devices grid, `roomler exec/ssh <name>` resolution, SOCKS mesh roster, audit rows, presence events |
| **Overlay/DNS label** | `OverlayNode.name` | overlay join (`dns_label` + in-network de-dup) and, since the rename feature, `propagate_node_rename` | MagicDNS (`<label>.<tenant domain>`), `NetmapPeer.name`, exit-node selection by name |

On top of those, two **display-only** admin fields never propagate anywhere:
`display_name` (friendly label; empty clears) and `tags` (free-form, trimmed +
de-duped, ≤16 × ≤40 chars).

## Renaming a device

`PUT /api/tenant/{tid}/agent/{id} {"name": …}` (agents) and
`PUT /api/tenant/{tid}/tunnel-client/{id}` (tunnel clients — the ONLY in-place
rename there is: a client-side rename derives a new machine_id and enrolls a
brand-new row). Both `MANAGE_AGENTS`. What happens:

1. The fleet row is renamed and marked `name_admin_set` — from then on a
   re-enroll refreshes os/version but **no longer overwrites the name** with
   the machine-reported one (it used to, silently reverting every rename).
   A never-renamed device keeps following its machine-reported hostname.
2. If the device has a **live overlay node**, the new MagicDNS label is derived
   (`dns_label`, de-duped within the network excluding the node itself, so a
   no-op rename keeps its label) and written; the unique
   `(tenant, network, name)` index arbitrates races, with one epoch-suffix
   retry. The response reports `dns_renamed` + `dns_name`.
3. Peers get an upsert **delta re-fan** — their netmaps and MagicDNS resolve
   the new label immediately.

## Caveats

- **The renamed device itself** keeps answering its OLD self-name until its
  next reconnect: `self_name` rides only the join-time full netmap, and the
  client's mid-session full-netmap arm is deliberately not exercised
  (field-untested). Peers are correct immediately; the device converges on
  reconnect.
- **Exit-node selection by name goes stale**: a peer whose config says
  `overlay_exit_node = "<old-name>"` fails to re-resolve after ITS next
  restart. Pin by **node-id hex** instead (`route_guard` resolves both) to be
  rename-proof; the rename dialog warns about this.
- `roomler exec/ssh <name>` and the SOCKS mesh roster resolve server-side per
  call / lazily — they self-heal onto the new name.
- Tunnel/overlay ACLs reference devices by ObjectId only — rename-safe.
