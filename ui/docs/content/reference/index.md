---
title: Reference
description: The exact surfaces — every roomler command, every agent configuration key, which ports are needed, and how the HTTP API is authenticated and scoped.
tags: [reference, cli, configuration, api, ports]
order: 0
---

Lookup material rather than reading material. If you are trying to *understand*
something, [Architecture](/docs/architecture/) is the better start; these pages
are for when you already know what you want and need the exact spelling.

:::cards
- **[CLI](/docs/reference/cli/)** icon:terminal — Every `roomler` command and the flags that actually exist.
- **[Configuration](/docs/reference/configuration/)** icon:blueprint — Agent `config.toml` keys, where the file lives per platform, and which settings the server can and cannot change.
- **[Ports and firewall](/docs/reference/ports-and-firewall/)** icon:network — What must be reachable, and what never needs opening inbound.
- **[HTTP API](/docs/reference/api/)** icon:book — Authentication, organization scoping, rate limits and route groups.
:::

## Two things that catch people

:::warning Device selectors are hex ids, not display names
`--agent`, and the target argument to `exec` and `ssh`, take the **hex device
id** from the dashboard. A friendly name is not resolved, and passing one fails
in a way that looks like the device is missing rather than like a bad argument.
:::

:::warning `--help` is authoritative for the version you have
These pages describe the current release. The binary on your machine may be
older or newer, and `roomler <command> --help` is the truth for it.
:::
