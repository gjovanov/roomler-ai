---
title: Install the tunnel CLI only
description: Install just the roomler command-line client on a machine you sit at, to open port forwards and a SOCKS5 proxy without running the full agent.
tags: [install, tunnels, cli, getting-started, socks5]
order: 14
---

Not every machine needs the full agent. If you only want to **open tunnels
from** a machine — your own laptop, a jump box, a CI runner — install the
**tunnel client**: the `roomler` CLI on its own, with no service, no screen
capture and no mesh membership.

## Agent or tunnel client?

| Capability | Agent (`roomlerd`) | Tunnel client (`roomler`) |
|---|---|---|
| Reach **this** machine remotely | Yes | No |
| Open tunnels **from** this machine | Yes | Yes |
| Screen sharing and input | Yes | No |
| Joins the private mesh | Yes | No |
| Runs as a service | Yes | Only while you run it |
| Install footprint | Full node stack | One binary |

:::tip The rule of thumb
If you want to **reach** the machine, install the agent. If you want to **reach
out from** it, the tunnel client is enough.
:::

## Install

Mint a token under **Tunnel clients → Enroll** — this is a different token type
from the device enrollment token, so mint it from the right place.

:::enroll tunnel
:::

## Use it

```bash
roomler status                                                    # who am I, and am I connected
roomler forward --agent <agent-id> --local 5432 --remote localhost:5432
roomler socks5 --local 1080                                       # a SOCKS5 proxy into the mesh
roomler flows                                                     # what is open right now
```

:::warning Targets are device **ids**, not display names
`--agent` takes the hex device id from the dashboard. A friendly name is not
resolved, and passing one fails in a way that looks like the device is missing.
:::

[Tunnels](/docs/network/tunnels/) and [SOCKS5](/docs/network/socks5/) cover the
whole surface, including UDP and mesh mode.

## Keeping it updated

On a tunnel-only machine the CLI updates itself:

```bash
roomler self-update
```

:::warning On a machine that also runs the agent, `self-update` refuses
That is deliberate. On an agent host the installer owns the whole node stack,
and the `roomler` command is a thin shim that re-execs the agent's own command
surface — so there is nothing separate for it to update, and letting it try
would replace a 150 KB shim with a full standalone binary.
:::

## Uninstall

Delete the binary and remove the tunnel client in the dashboard. Removing it
server-side is the part that revokes its credential.
