---
title: Device policies
description: The per-machine gates for remote command execution, SSH and relaying — four independent layers, each owned by a different party, all default-deny.
tags: [security, device-policies, access-control, ssh, exec, admin]
order: 4
---

The most powerful things you can do to a machine — run a command on it, open a
shell, make it forward other machines' traffic — are gated at **four independent
levels**. Each is owned by a different party, and **any one of them refuses**.

## The four gates

:::steps
1. **The organization.** A switch per capability. Remote command execution and SSH are **separate switches** — enabling one does not enable the other.
2. **The person.** A specific permission, deliberately **not** included in the default administrator role, and separate per capability.
3. **The device.** A per-machine policy set by an administrator: whether this machine accepts it at all, as which account, and whether it prompts.
4. **The machine itself.** Its own configuration. The only refusal that survives a compromised server.
:::

:::danger Gate 4 is the reason the other three are worth having
Gates 1 to 3 are enforced by the server. If the control plane were compromised,
they would fall together. Gate 4 lives in the machine's own configuration file
and **the server structurally cannot write it**.

That is the property, and it has a real cost: enabling these features means
touching the machine.
:::

## Remote command execution

Run a command on a trusted device from the dashboard or the CLI.

:::warning Commands inherit the agent's identity
On Windows that is `SYSTEM`; under systemd it is `root`. This is not a
restricted shell. Grant it to the people you would give root to, because that is
what you are giving them.
:::

It runs over the agent's **control connection**, not the mesh — deliberately, so
the diagnostics you most need still work when the mesh is the broken thing.

Output is redacted for credentials before it leaves the machine, and every
attempt, including every refusal, is recorded.

## SSH

Four gates again, with their own switch and their own permission. Additionally:

- **Which account** a session runs as is device-owned. Unset means *authenticate, then run nothing* — never a silent root shell.
- **Port forwarding is default-deny.** An empty allow-list means *nowhere*. See [SSH](/docs/network/ssh/) for why that is the opposite sense from tunnels.
- **Consent** may be required, and fires when the session is redeemed so a refusal can explain itself.

## Acting as a relay

A machine can forward other machines' encrypted traffic, keeping relayed pairs
on hardware you own. Gated by an organization mode (**off** by default), an
access rule making the relay visible to each party, a per-device approval
requiring **two** permissions, and the machine's own opt-in.

:::warning A relay never becomes server-configurable
The device's own consent to relay is not something the server can push, for the
same reason as everything else on this page.
:::

## Remote configuration — the deliberate loophole, and its limit

Requiring physical access to every machine is a real obstacle, and it is why
these features are off nearly everywhere. So a machine **may** opt in to being
configured from the dashboard.

:::danger The opt-in itself is not remotely settable
`remote_config_enabled` is device-owned and structurally absent from anything
the server can push. A machine that sets it has knowingly delegated its last
gate to its control plane; a machine that has not, has not — and no server-side
action can change that.
:::

Two consequences worth knowing:

- **The machine reports back.** Applied, applied-but-needs-a-restart, refused-because-not-opted-in, refused-because-secondary-organization, and never-arrived are five different states with five different fixes. Without a report they would look identical on screen.
- **Compare revisions, not just outcomes.** A report about revision 3 says nothing about revision 4.

## Reviewing what is enabled

The device list shows which capabilities each machine has switched on. It is
worth reading occasionally — these are the settings people turn on for one
debugging session and never turn off.
