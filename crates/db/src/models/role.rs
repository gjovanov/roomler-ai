// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    pub description: Option<String>,
    pub color: Option<u32>,
    #[serde(default)]
    pub position: u32,
    #[serde(default)]
    pub permissions: u64,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub is_managed: bool,
    #[serde(default)]
    pub is_mentionable: bool,
    #[serde(default)]
    pub is_hoisted: bool,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

/// Permission bits (u64 bitfield)
#[allow(dead_code)]
pub mod permissions {
    pub const VIEW_CHANNELS: u64 = 1 << 0;
    pub const MANAGE_CHANNELS: u64 = 1 << 1;
    pub const MANAGE_ROLES: u64 = 1 << 2;
    pub const MANAGE_TENANT: u64 = 1 << 3;
    pub const KICK_MEMBERS: u64 = 1 << 4;
    pub const BAN_MEMBERS: u64 = 1 << 5;
    pub const INVITE_MEMBERS: u64 = 1 << 6;
    pub const SEND_MESSAGES: u64 = 1 << 7;
    pub const SEND_THREADS: u64 = 1 << 8;
    pub const EMBED_LINKS: u64 = 1 << 9;
    pub const ATTACH_FILES: u64 = 1 << 10;
    pub const READ_HISTORY: u64 = 1 << 11;
    pub const MENTION_EVERYONE: u64 = 1 << 12;
    pub const MANAGE_MESSAGES: u64 = 1 << 13;
    pub const ADD_REACTIONS: u64 = 1 << 14;
    pub const CONNECT_VOICE: u64 = 1 << 15;
    pub const SPEAK: u64 = 1 << 16;
    pub const STREAM_VIDEO: u64 = 1 << 17;
    pub const MUTE_MEMBERS: u64 = 1 << 18;
    pub const DEAFEN_MEMBERS: u64 = 1 << 19;
    pub const MOVE_MEMBERS: u64 = 1 << 20;
    pub const MANAGE_MEETINGS: u64 = 1 << 21;
    pub const MANAGE_DOCUMENTS: u64 = 1 << 22;
    pub const ADMINISTRATOR: u64 = 1 << 23;
    /// Enroll / rename / delete / assign-owner / set-policy for remote-control
    /// agents (devices).
    pub const MANAGE_AGENTS: u64 = 1 << 24;
    /// Initiate a remote-control session against a device you do NOT own.
    /// (Controlling your OWN device — `controller == owner_user_id` — never
    /// needs this; it's gated only by the device's consent mode.)
    pub const REMOTE_CONTROL: u64 = 1 << 25;
    /// View the remote-control audit log (`remote_audit`).
    pub const VIEW_REMOTE_AUDIT: u64 = 1 << 26;
    /// Run a command on a device via Fleet RPC (`roomler exec`, the device
    /// console). Gate 2 of four — the org kill-switch, the device's own
    /// `ExecPolicy`, and the agent-local `exec_enabled` key each deny
    /// independently. Deliberately NOT implied by `MANAGE_AGENTS`: managing a
    /// device's metadata and running a root shell on it are different powers.
    pub const EXEC_DEVICE: u64 = 1 << 27;
    /// View the Fleet-RPC audit log (`exec_audit`) — who ran what, where.
    pub const VIEW_EXEC_AUDIT: u64 = 1 << 28;
    /// Open a roomler SSH session to a device (`roomler ssh`). Gate 2 of four,
    /// mirroring [`EXEC_DEVICE`].
    ///
    /// Deliberately a SEPARATE bit rather than a reuse of `EXEC_DEVICE`, for
    /// the same reason `EXEC_DEVICE` is separate from `MANAGE_AGENTS`: an SSH
    /// session is strictly more than a bounded command. It is interactive, it
    /// lasts, and it grows file transfer and port forwarding as later slices
    /// land — so "may run one clamped diagnostic" and "may hold a live session"
    /// have to be grantable independently.
    pub const SSH_DEVICE: u64 = 1 << 29;
    /// View the roomler-SSH audit log (`ssh_audit`) — who was granted, or
    /// refused, a session on which device.
    ///
    /// Separate from [`VIEW_EXEC_AUDIT`] for the same reason [`SSH_DEVICE`] is
    /// separate from [`EXEC_DEVICE`]: the two logs answer different questions
    /// and an org may well want one reviewer for bounded commands and another
    /// for interactive sessions.
    pub const VIEW_SSH_AUDIT: u64 = 1 << 30;
    // ⚠️ Bit 52 is the ceiling, and the reason is the JSON number rather than
    // anything here: a mask crosses the wire as a JSON integer, which is exact
    // only below 2^53. The UI mirror (`ui/src/utils/permissions.ts`) does its
    // mask arithmetic WITHOUT bitwise operators for exactly this reason — JS
    // coerces those to signed int32, which is what capped the catalog at bit 30
    // until #888. Adding a bit at 31..52 now needs a catalog entry there and
    // nothing else; going above 52 needs BigInt or string masks end to end.
    //
    // ⚠️ Still true, and the thing to check when adding one: a bit defined HERE
    // that the UI does not list is a permission nobody can grant from the
    // product. The mirror is hand-maintained and its spec locks the count.
    //
    // FR-19's relay approval rides MANAGE_AGENTS + EXEC_DEVICE (an EXEC_DEVICE
    // holder can already enable a relay on any exec-enabled device as root, so
    // the coupling grants nothing new). That was a workaround for the ceiling;
    // it can now become a dedicated bit whenever FR-19 wants one.

    /// Default member permissions
    pub const DEFAULT_MEMBER: u64 = VIEW_CHANNELS
        | SEND_MESSAGES
        | SEND_THREADS
        | EMBED_LINKS
        | ATTACH_FILES
        | READ_HISTORY
        | ADD_REACTIONS
        | CONNECT_VOICE
        | SPEAK
        | STREAM_VIDEO;

    /// Admin permissions (all except ADMINISTRATOR)
    pub const DEFAULT_ADMIN: u64 = DEFAULT_MEMBER
        | MANAGE_CHANNELS
        | MANAGE_ROLES
        | KICK_MEMBERS
        | BAN_MEMBERS
        | INVITE_MEMBERS
        | MENTION_EVERYONE
        | MANAGE_MESSAGES
        | MUTE_MEMBERS
        | DEAFEN_MEMBERS
        | MOVE_MEMBERS
        | MANAGE_MEETINGS
        | MANAGE_DOCUMENTS
        | MANAGE_AGENTS
        | REMOTE_CONTROL
        | VIEW_REMOTE_AUDIT
        // VIEW_EXEC_AUDIT but deliberately NOT EXEC_DEVICE, and for the same
        // reason not SSH_DEVICE: an admin should see every command the fleet
        // ran without silently gaining the power to run one. REMOTE_CONTROL is
        // not the same power — it is consent-gated, visible to whoever is at
        // the machine, and runs as the interactive user; exec and ssh run as
        // SYSTEM/root with nobody watching. Both stay explicit grants.
        | VIEW_EXEC_AUDIT
        // Same split for SSH: see the audit without gaining the session.
        | VIEW_SSH_AUDIT;

    /// Owner permissions (everything). Bump the mask whenever a new bit is
    /// added above so `ALL` literally contains every defined permission (owner
    /// also passes via the `ADMINISTRATOR` bypass in `has`, but keep this exact).
    pub const ALL: u64 = (1 << 31) - 1;

    /// Every named bit, with its wire name. Lives here rather than in the test
    /// module because two callers need it: `all_contains_every_named_permission`
    /// (so a new bit cannot be added without bumping `ALL`), and the escalation
    /// guard's 403, which has to be able to SAY which permission it refused —
    /// "you may not grant 0x8000000" is not an actionable error.
    pub const NAMED: &[(&str, u64)] = &[
        ("VIEW_CHANNELS", VIEW_CHANNELS),
        ("MANAGE_CHANNELS", MANAGE_CHANNELS),
        ("MANAGE_ROLES", MANAGE_ROLES),
        ("MANAGE_TENANT", MANAGE_TENANT),
        ("KICK_MEMBERS", KICK_MEMBERS),
        ("BAN_MEMBERS", BAN_MEMBERS),
        ("INVITE_MEMBERS", INVITE_MEMBERS),
        ("SEND_MESSAGES", SEND_MESSAGES),
        ("SEND_THREADS", SEND_THREADS),
        ("EMBED_LINKS", EMBED_LINKS),
        ("ATTACH_FILES", ATTACH_FILES),
        ("READ_HISTORY", READ_HISTORY),
        ("MENTION_EVERYONE", MENTION_EVERYONE),
        ("MANAGE_MESSAGES", MANAGE_MESSAGES),
        ("ADD_REACTIONS", ADD_REACTIONS),
        ("CONNECT_VOICE", CONNECT_VOICE),
        ("SPEAK", SPEAK),
        ("STREAM_VIDEO", STREAM_VIDEO),
        ("MUTE_MEMBERS", MUTE_MEMBERS),
        ("DEAFEN_MEMBERS", DEAFEN_MEMBERS),
        ("MOVE_MEMBERS", MOVE_MEMBERS),
        ("MANAGE_MEETINGS", MANAGE_MEETINGS),
        ("MANAGE_DOCUMENTS", MANAGE_DOCUMENTS),
        ("ADMINISTRATOR", ADMINISTRATOR),
        ("MANAGE_AGENTS", MANAGE_AGENTS),
        ("REMOTE_CONTROL", REMOTE_CONTROL),
        ("VIEW_REMOTE_AUDIT", VIEW_REMOTE_AUDIT),
        ("EXEC_DEVICE", EXEC_DEVICE),
        ("VIEW_EXEC_AUDIT", VIEW_EXEC_AUDIT),
        ("SSH_DEVICE", SSH_DEVICE),
        ("VIEW_SSH_AUDIT", VIEW_SSH_AUDIT),
    ];

    /// Names of every named bit set in `mask`, for error messages. An
    /// unnamed bit is rendered as its hex value rather than dropped — a mask
    /// that refuses something the table forgot must still say so.
    pub fn names(mask: u64) -> Vec<String> {
        let mut out: Vec<String> = NAMED
            .iter()
            .filter(|(_, bit)| mask & bit == *bit)
            .map(|(name, _)| (*name).to_string())
            .collect();
        let named_mask = NAMED.iter().fold(0u64, |a, (_, b)| a | b);
        let unnamed = mask & !named_mask;
        if unnamed != 0 {
            out.push(format!("{unnamed:#x}"));
        }
        out
    }

    pub fn has(permissions: u64, flag: u64) -> bool {
        permissions & ADMINISTRATOR != 0 || permissions & flag == flag
    }
}

impl Role {
    pub const COLLECTION: &'static str = "roles";
}

#[cfg(test)]
mod tests {
    use super::permissions::*;

    #[test]
    fn all_contains_every_named_permission() {
        // `ALL` is a hand-maintained `(1 << N) - 1` and it is what the Owner
        // role is created with. Adding a bit above without bumping it leaves
        // Owner missing that permission — invisible, because Owner also
        // carries ADMINISTRATOR and passes `has` by the bypass. This makes the
        // "bump the mask" comment executable instead of advisory.
        for (name, bit) in NAMED {
            assert!(
                ALL & bit == *bit,
                "{name} ({bit:#x}) is not in ALL ({ALL:#x}) — bump the mask"
            );
        }
    }

    #[test]
    fn every_named_permission_is_a_distinct_single_bit() {
        for (name, bit) in NAMED {
            assert_eq!(bit.count_ones(), 1, "{name} is not a single bit");
        }
        for (i, (an, ab)) in NAMED.iter().enumerate() {
            for (bn, bb) in &NAMED[i + 1..] {
                assert_ne!(ab, bb, "{an} and {bn} are the same bit");
            }
        }
    }

    #[test]
    fn admins_can_read_the_audits_without_gaining_the_powers() {
        // The deliberate asymmetry: an admin sees every session and command
        // the fleet served without silently acquiring the ability to open one.
        assert_ne!(
            DEFAULT_ADMIN & VIEW_EXEC_AUDIT,
            0,
            "admins must be able to read the exec audit"
        );
        assert_ne!(
            DEFAULT_ADMIN & VIEW_SSH_AUDIT,
            0,
            "admins must be able to read the ssh audit"
        );
        assert_eq!(
            DEFAULT_ADMIN & EXEC_DEVICE,
            0,
            "EXEC_DEVICE must stay an explicit grant"
        );
        assert_eq!(
            DEFAULT_ADMIN & SSH_DEVICE,
            0,
            "SSH_DEVICE must stay an explicit grant"
        );
    }

    #[test]
    fn ssh_and_exec_are_independently_grantable() {
        // Four separate bits on purpose: "may run one clamped command",
        // "may hold an interactive session", and the two audit views are
        // different powers and different jobs.
        for (a, b) in [
            (EXEC_DEVICE, SSH_DEVICE),
            (VIEW_EXEC_AUDIT, VIEW_SSH_AUDIT),
            (EXEC_DEVICE, VIEW_EXEC_AUDIT),
            (SSH_DEVICE, VIEW_SSH_AUDIT),
        ] {
            assert_eq!(a & b, 0, "{a:#x} and {b:#x} overlap");
        }
    }
}
