---
title: CLI reference
description: Every roomler command — status, peers, why, netcheck, forwards, routes, exec, ssh and diagnostics — with the flags that actually exist.
tags: [reference, cli, commands, tunnels, diagnostics]
order: 1
---

`roomler` is the command-line tool. On a machine that also runs the agent it is
a thin shim onto the agent's own command surface, so the two can never disagree
about their version.

:::warning Device selectors are hex ids, not display names
`--agent`, and the target argument to `exec`, take the **hex device id** from
the dashboard. A friendly name is not resolved, and passing one fails in a way
that looks like the device is missing rather than like a bad argument.
:::

## Status and inspection

| Command | Does |
|---|---|
| `roomler status` | This machine: id, version, mode, mesh address, server connection — **and** the mesh state per organization |
| `roomler peers` | Every peer the local agent sees, with its live connection type |
| `roomler why <peer>` | Why **one** peer rides the path it does: the ladder, each tier's eligibility, and any hold-down overriding the ranking |
| `roomler netcheck` | This machine's measured network capability: reachability, relay verdict, floor health, NAT class |
| `roomler flows` | Flows the local agent is currently running |
| `roomler logs --tail <n>` | Tail the agent's log, resolved **by** the agent — the path differs per process and platform |
| `roomler ping <peer>` | Reachability over the mesh |

:::tip `why` is the command to reach for on a relay question
`peers` tells you a pair is relayed. `why` tells you which tier was eligible,
what it scored, and whether something is holding the decision down — which is
the difference between knowing and guessing.
:::

:::warning `roomler logs --grep` reads a bounded tail
It searches a slice of the end of the log, not all of it. **A negative result is
not proof of absence.**
:::

## Tunnels

```bash
roomler forward --agent <id> --local 5432 --remote localhost:5432
roomler forward --agent <id> --local 5432 --remote db.internal:5432 --daemon
roomler socks5  --agent <id> --local 1080
roomler socks5  --local 1080                  # mesh mode: omit --agent
roomler kill <flow-id>
```

| Flag | Means |
|---|---|
| `--agent` | Hex device id of the far end. On `socks5`, **omitting** it selects mesh mode |
| `--local` | Local port to listen on, bound to loopback |
| `--remote` | `host:port` the far end dials |
| `--daemon` | Hand the flow to the local agent so it outlives this command |

Both commands stay in the foreground without `--daemon`; `Ctrl-C` tears down.

## Declared routes

Forwards the agent re-establishes on every start:

```bash
roomler route add --agent <id> --local 5432 --remote localhost:5432
roomler route ls
roomler route enable <id>
roomler route disable <id>
roomler route rm <id>
```

## Remote access

```bash
roomler exec <agent-id> -- <command>
roomler ssh <agent-id>
roomler proxy <host> <port>        # for OpenSSH ProxyCommand
```

:::warning `exec` and `ssh` do not resolve display names either
Use the hex device id. This is the most common cause of "the CLI says my device
does not exist" when it is plainly in the dashboard.
:::

:::danger On Windows, quote-containing arguments to `exec` can be lost
An argument with spaces can arrive at the far end empty. The tell is a blank
line or a zero-byte file rather than an error. Prefer `roomler ssh` when the
command has arguments that need quoting.
:::

`roomler proxy` is for OpenSSH's `ProxyCommand` — transport and name resolution
only. It cannot supply an identity or a host key, so it uses keys **you**
manage. For a session where the account is resolved by policy and the host key
is verified for you, use `roomler ssh`.

## Diagnostics

```bash
roomler diag host                  # evidence bundle from one device
roomler diag pair <other-agent-id> # both ends, side by side
roomler diagnose --agent <id>      # probe MTU, candidates and relay status from HERE
```

:::tip `diag` and `diagnose` are different tools
`diagnose` probes **from this machine**. `diag` runs a canned, OS-appropriate
evidence set **on the target devices** — adapters, routes, firewall posture,
carrier state, recent warnings — which is what a "why is this pair relayed?"
question actually needs.
:::

## Configuration and identity

```bash
roomler config ls
roomler config set <key> <value>
roomler config clear <key>
roomler rename <new-name>
roomler enroll --server <url> --token <token> --name <name>
roomler self-update
```

:::warning `self-update` refuses on a machine that runs the agent
There the installer owns the whole node stack and `roomler` is a shim with
nothing of its own to update. It is the real updater only on tunnel-only
machines.
:::

## Organizations

```bash
roomler org ls
roomler org overlay <org> tun
roomler org set-primary <org>
roomler org rm <org>
```

## Getting help

Every command takes `--help`, and that is authoritative for the version you have
installed:

```bash
roomler --help
roomler forward --help
```
