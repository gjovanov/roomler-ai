---
title: Users, roles and permissions
description: How membership, roles and permissions decide what someone can do in an organization — and which powerful ones are deliberately not granted by default.
tags: [security, permissions, roles, access-control, admin]
order: 2
---

Everything is scoped to an **organization**. A user is a member of one or more,
and their role in each decides what they can do there.

## Roles

Roles are collections of permissions. Every organization starts with a sensible
set, and you can define your own.

| Role | Broadly |
|---|---|
| **Owner** | Everything, including billing and deletion |
| **Administrator** | Manage members, roles, devices and settings |
| **Member** | Use rooms, calls and files; use the devices they are granted |
| **Guest** | Limited to the rooms they were invited to |

## Permissions

Permissions are individual capabilities, not a level. Some of the ones worth
knowing:

| Permission | Grants |
|---|---|
| **Manage members** | Invite, remove, assign roles |
| **Manage channels** | Create, archive and delete rooms |
| **Manage devices** | Enroll, rename, remove, configure |
| **Remote control** | Open a device's screen |
| **Run commands on a device** | Execute a command remotely |
| **SSH to a device** | Open an SSH session |
| **View command audit** | Read the record of remote commands |
| **View SSH audit** | Read the record of SSH decisions |

### The powerful ones are deliberately not in the default admin role

:::danger Running commands and opening SSH sessions are separate grants
Neither **run commands** nor **SSH** is included in the default administrator
role, and they are separate from each other. Being able to manage a fleet is a
different job from being able to execute code on every machine in it.

The audit-reading permissions are the mirror image: **view SSH audit** *is* in
the default admin role, while **SSH** is not — because reviewing who held a
session is a different job from opening one.
:::

:::warning A new permission is not retroactive
Permissions added in a release are not granted to existing custom roles — their
stored permission set is what it was. If a feature seems missing for an
administrator, check whether their role's permissions have been updated since
the feature shipped. (An owner is unaffected: the owner role bypasses individual
checks.)
:::

## Granting a power you do not have

:::warning You cannot open a door you cannot walk through
Enabling remote commands on a device requires *both* the device-management
permission **and** the command permission. Enabling SSH keys requires the SSH
permission.

Otherwise an administrator could grant a capability to others that they were
denied themselves, which is the same escalation by a longer route. Revoking, by
contrast, needs only device management — taking a permission away is not a
grant.
:::

## Invitations

Invite by link or by email. An invitation carries the role the person will get,
and an invitation cannot grant a role more powerful than the inviter's own.

## Object-level scoping

:::tip Membership is not authorisation
Being a member of an organization is not permission to touch a specific object
in it. Every request that names a room, message or device resolves that object
**within** the organization first — so a reference to something in another
organization is not found, rather than being checked afterwards.

That distinction matters because anyone can create an organization, and
therefore anyone can satisfy a bare "is a member of *some* organization" check.
:::

## Multiple organizations

A user can be in several, with a different role in each. Everything — devices,
rooms, files, audit — is scoped per organization, and there is no view that
crosses them.
