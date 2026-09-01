---
title: Consent and audit
description: Who is asked before a session starts, what is recorded afterwards, and why the record of a decision is kept separately from a machine's own account of itself.
tags: [security, consent, audit, compliance, access-control]
order: 5
---

Two questions, kept deliberately separate: **who agreed to this?** and **what
actually happened?**

## Consent

By default a machine asks the person sitting at it before handing over its
screen. Modes, surfaces and fallbacks are covered in
[consent](/docs/remote-desktop/consent/). The parts that matter here:

:::badges
- **Absent means ask** icon:check — a device with no consent setting prompts. Only an explicit "automatic" skips it.
- **The device's setting is a floor** icon:shield — a server-side policy cannot loosen it.
- **A refusal has a reason** icon:info — denied, timed out, or no prompt surface are three different facts, and the caller is told which.
:::

## What is recorded

| Record | Contains |
|---|---|
| **Remote sessions** | Who connected, to which device, when, from where, and the outcome |
| **Command execution** | Every attempt including refusals, with the command and the result |
| **SSH decisions** | Every request, granted or refused, with the reason and the account mode |
| **SSH activity** | What the machine says it did: sessions, commands, forwards |
| **Network ACL decisions** | What was allowed or denied, especially in warn mode |
| **Relay decisions** | Every grant and revocation |
| **Organization events** | Membership, role and setting changes |

Audit records are retained for **90 days** and are readable by holders of the
relevant audit permission — which is separate from the permission to perform the
action.

## Two records, never merged

:::danger An audit record and an activity report are different kinds of claim
The **audit** record is the server's own decision. It is authoritative.

The **activity** report is what a machine says it did. If that machine is
compromised, its report is whatever the attacker wants it to be.

Folding them into one table would leave a reader unable to tell which is which.
They are joined by an identifier when you want the full story, and kept apart so
that the story has attribution.
:::

:::warning An empty activity log is not evidence of inactivity
Activity reporting is a machine-side setting that defaults to off, so a machine
that never reports looks exactly like an idle one. The server's decision log is
what survives a machine that lies — read that one when it matters.
:::

## What is deliberately never recorded

:::danger No session contents, ever
No pixel stream, no keystroke log, no terminal recording, no command output
stored server-side.

Recording a terminal means shipping whatever an operator typed — a password into
`sudo`, a credential into a database client — off the machine and into a system
they do not control. That is the exact property the rest of this product exists
to avoid, so it is not offered as an option.
:::

If you need session recording for compliance, this is a real gap, and it is
better to know now than to discover it during an audit.

## Reading the audit

The dashboard has a section per record type, filterable by device, user and time.

:::tip The refusals are the interesting rows
A granted session is usually routine. A **denied** one — a permission that was
missing, a consent that timed out, a forward a machine refused — is what an
investigation is actually looking for. That is why refusals are recorded with
their reason rather than as a bare failure.
:::

## Exporting

Audit records can be exported for retention beyond 90 days or for an external
SIEM. Do it before the retention window closes — expiry is automatic.
