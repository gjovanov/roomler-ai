// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P2c-2b — placement: which devices hold a session's copies (spec §3b,
//! "Membership and placement").
//!
//! [`place`] is a pure function over facts the server already holds, so every
//! rule below is a unit test; [`place_new`] reads those facts once, for a
//! session that is starting. A placement is a plan: the joins (P2c-3) say
//! which members took their copy, and the device's own `hive_replica` decides
//! that, whatever this chose.
//!
//! | Rule | Why |
//! |---|---|
//! | a candidate advertises `hive-replica` | its owner's `hive_replica` is on: the gate that survives a compromised server. A device advertises it only on its primary org's connection (P2c-3) |
//! | never an ephemeral device | FR-51 hard-deletes it once it goes quiet: it could keep nothing for the retention, and never acknowledge a purge |
//! | the owner's own devices are those whose `owner_user_id` AND `enrolled_by` both name the session's owner | `owner_user_id` is reassignable with `MANAGE_AGENTS` alone: a device manager who hands someone a device must not receive that person's sessions on it |
//! | an archive replica is designated by an `ADMINISTRATOR`, offers itself (`hive-archive`), and joins only while `archive` is on | the administrator's half and the device owner's half, either first |
//! | a restricted tag only ever takes a device out, and is compared without case | a guard that assumes a casing is no guard: a primary tagged `Prod` is restricted by `prod` |
//! | the primary, then archive replicas in the policy's order, then the owner's devices last seen first until `min`; never more than `max` | decision 2 |
//! | an adopted session is placed nowhere but where it was adopted | decision 11: only its owner sees it, and whoever runs an archive replica could read that disk |

use bson::{DateTime, oid::ObjectId};
use roomler_ai_remote_control::models::{Agent, RpcCap};
use roomler_ai_services::dao::base::DaoResult;

use crate::HiveState;
use crate::model::{ReplicaMember, Replicaset, SessionOrigin};
use crate::policy::{HivePolicy, ReplicasetRules};

/// The one word `prefer` may name today: the session owner's own devices.
const OWNER_DEVICES: &str = "owner_devices";

/// What placement knows about one device.
#[derive(Debug, Clone)]
pub struct DeviceFacts {
    pub id: ObjectId,
    pub owner_user_id: ObjectId,
    pub enrolled_by: Option<ObjectId>,
    pub tags: Vec<String>,
    pub ephemeral: bool,
    /// It advertises `hive-replica`: its owner lets it hold copies.
    pub replica: bool,
    /// It advertises `hive-archive`: its owner offers it as an archive.
    pub archive: bool,
    pub last_seen_at: DateTime,
}

impl DeviceFacts {
    /// From the stored row: its last hello's capabilities, so a device that
    /// is offline now is still placed, and joins when it connects.
    pub fn of(a: &Agent) -> Option<Self> {
        Some(Self {
            id: a.id?,
            owner_user_id: a.owner_user_id,
            enrolled_by: a.enrolled_by,
            tags: a.tags.clone(),
            ephemeral: a.ephemeral,
            replica: a.capabilities.has_rpc(RpcCap::HiveReplica),
            archive: a.capabilities.has_rpc(RpcCap::HiveArchive),
            last_seen_at: a.last_seen_at,
        })
    }
}

/// What placement knows about the session.
#[derive(Debug, Clone)]
pub struct SessionFacts<'a> {
    pub owner: ObjectId,
    pub primary: ObjectId,
    /// The primary device's tags.
    pub primary_tags: &'a [String],
    pub origin: SessionOrigin,
    /// The restricted tags it already carries, which a placement only adds to.
    pub restricted: &'a [String],
}

/// Why a member holds a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// It runs the session.
    Primary,
    /// An archive replica the policy designates.
    Archive,
    /// One of the session owner's own devices.
    Owner,
}

impl MemberRole {
    /// The spelling in the database and the API. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Archive => "archive",
            Self::Owner => "owner",
        }
    }
}

/// Why a session holds fewer copies than its `min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Short {
    /// An adopted session is placed nowhere but where it was adopted.
    Adopted,
    /// A device that would hold a copy lacks a tag the session is restricted
    /// by.
    Restricted,
    /// No other device offers to hold one.
    NoDevice,
}

impl Short {
    /// The spelling in the database, the API and the audit. Locked by test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::Restricted => "restricted",
            Self::NoDevice => "no_device",
        }
    }
}

/// What [`place`] chose.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    /// The primary first, then the rest in the order they were chosen.
    pub members: Vec<(ObjectId, MemberRole)>,
    pub restricted_tags: Vec<String>,
    pub short: Option<Short>,
}

fn same_tag(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

fn carries(tags: &[String], tag: &str) -> bool {
    tags.iter().any(|t| same_tag(t, tag))
}

/// The members of a session's replicaset, by the rules (module docs).
pub fn place(
    rules: &ReplicasetRules,
    archive_devices: &[ObjectId],
    session: &SessionFacts<'_>,
    devices: &[DeviceFacts],
) -> Placement {
    // What it already carries, then every policy tag its primary carries.
    let mut restricted: Vec<String> = Vec::new();
    let from_policy = rules
        .restricted_tags
        .iter()
        .filter(|t| carries(session.primary_tags, t));
    for t in session.restricted.iter().chain(from_policy) {
        if !carries(&restricted, t) {
            restricted.push(t.clone());
        }
    }
    let min = rules.min as usize;
    let max = rules.max as usize;
    let mut members = vec![(session.primary, MemberRole::Primary)];
    if session.origin == SessionOrigin::Adopted {
        let short = (members.len() < min).then_some(Short::Adopted);
        return Placement {
            members,
            restricted_tags: restricted,
            short,
        };
    }

    let offers = |d: &DeviceFacts| d.id != session.primary && d.replica && !d.ephemeral;
    let allowed = |d: &DeviceFacts| restricted.iter().all(|t| carries(&d.tags, t));
    let placed = |members: &[(ObjectId, MemberRole)], d: &DeviceFacts| {
        members.iter().any(|(m, _)| *m == d.id)
    };
    // A device that would hold a copy but for a restricted tag.
    let mut held_back = false;

    if rules.archive {
        for id in archive_devices {
            if members.len() >= max {
                break;
            }
            let Some(d) = devices.iter().find(|d| d.id == *id) else {
                continue;
            };
            if !offers(d) || !d.archive || placed(&members, d) {
                continue;
            }
            if !allowed(d) {
                held_back = true;
                continue;
            }
            members.push((d.id, MemberRole::Archive));
        }
    }

    if rules.prefer.iter().any(|p| p == OWNER_DEVICES) {
        let mut owners: Vec<&DeviceFacts> = devices
            .iter()
            .filter(|d| {
                offers(d)
                    && d.owner_user_id == session.owner
                    && d.enrolled_by == Some(session.owner)
                    && !placed(&members, d)
            })
            .collect();
        owners.sort_by(|a, b| {
            b.last_seen_at
                .cmp(&a.last_seen_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        owners.dedup_by_key(|d| d.id);
        for d in owners {
            if members.len() >= min.min(max) {
                break;
            }
            if !allowed(d) {
                held_back = true;
                continue;
            }
            members.push((d.id, MemberRole::Owner));
        }
    }

    let short = (members.len() < min).then_some(if held_back {
        Short::Restricted
    } else {
        Short::NoDevice
    });
    Placement {
        members,
        restricted_tags: restricted,
        short,
    }
}

/// The replicaset of a session `owner` is starting on `primary`, under the
/// organization's policy (or decision 2's defaults), from the devices'
/// stored rows; and why it is short of `min`, when it is.
pub async fn place_new(
    state: &HiveState,
    tid: ObjectId,
    owner: ObjectId,
    primary_id: ObjectId,
    primary: &Agent,
) -> DaoResult<(Replicaset, Option<Short>)> {
    let policy = state
        .policies
        .get(tid)
        .await?
        .unwrap_or_else(|| HivePolicy::defaults(tid));
    let rows = state
        .fleet
        .agents
        .list_replica_candidates(tid, owner, &policy.archive_devices)
        .await?;
    let devices: Vec<DeviceFacts> = rows.iter().filter_map(DeviceFacts::of).collect();
    let p = place(
        &policy.replicaset,
        &policy.archive_devices,
        &SessionFacts {
            owner,
            primary: primary_id,
            primary_tags: &primary.tags,
            origin: SessionOrigin::Started,
            restricted: &[],
        },
        &devices,
    );
    let replicaset = Replicaset {
        policy_revision: policy.revision,
        min: policy.replicaset.min,
        max: policy.replicaset.max,
        restricted_tags: p.restricted_tags,
        members: p
            .members
            .into_iter()
            .map(|(device_id, role)| ReplicaMember {
                device_id,
                role: role.as_str().to_string(),
            })
            .collect(),
        short: p.short.map(|s| s.as_str().to_string()),
        placed_at: DateTime::now(),
    };
    Ok((replicaset, p.short))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 2's defaults, with `min` and `max` as given.
    fn rules(min: u32, max: u32) -> ReplicasetRules {
        ReplicasetRules {
            min,
            max,
            ..HivePolicy::defaults(ObjectId::new()).replicaset
        }
    }

    struct Fleet {
        owner: ObjectId,
        primary: ObjectId,
        devices: Vec<DeviceFacts>,
    }

    impl Fleet {
        fn new() -> Self {
            Self {
                owner: ObjectId::new(),
                primary: ObjectId::new(),
                devices: Vec::new(),
            }
        }

        /// One of the owner's devices that offers to hold copies, last seen
        /// `ago` seconds back.
        fn owners(&mut self, ago: i64) -> ObjectId {
            let id = ObjectId::new();
            self.devices.push(DeviceFacts {
                id,
                owner_user_id: self.owner,
                enrolled_by: Some(self.owner),
                tags: Vec::new(),
                ephemeral: false,
                replica: true,
                archive: false,
                last_seen_at: DateTime::from_millis(1_000_000_000_000 - ago * 1000),
            });
            id
        }

        fn device(&mut self, id: ObjectId) -> &mut DeviceFacts {
            self.devices.iter_mut().find(|d| d.id == id).unwrap()
        }

        fn session(&self) -> SessionFacts<'static> {
            SessionFacts {
                owner: self.owner,
                primary: self.primary,
                primary_tags: &[],
                origin: SessionOrigin::Started,
                restricted: &[],
            }
        }
    }

    fn ids(p: &Placement) -> Vec<(ObjectId, &'static str)> {
        p.members.iter().map(|(i, r)| (*i, r.as_str())).collect()
    }

    #[test]
    fn the_words_are_locked() {
        assert_eq!(
            [MemberRole::Primary, MemberRole::Archive, MemberRole::Owner].map(MemberRole::as_str),
            ["primary", "archive", "owner"]
        );
        assert_eq!(
            [Short::Adopted, Short::Restricted, Short::NoDevice].map(Short::as_str),
            ["adopted", "restricted", "no_device"]
        );
    }

    /// With no other device, the session is on its primary alone, and short.
    #[test]
    fn alone_it_is_short_of_min() {
        let f = Fleet::new();
        let p = place(&rules(2, 4), &[], &f.session(), &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")]);
        assert_eq!(p.short, Some(Short::NoDevice));
        assert!(p.restricted_tags.is_empty());
        let p = place(&rules(1, 4), &[], &f.session(), &f.devices);
        assert_eq!(p.short, None, "one copy is all min 1 asks");
    }

    /// The owner's devices fill to `min`, last seen first, and never past it.
    #[test]
    fn the_owners_devices_fill_to_min_last_seen_first() {
        let mut f = Fleet::new();
        let old = f.owners(3600);
        let new = f.owners(10);
        let mid = f.owners(600);
        let p = place(&rules(3, 4), &[], &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [(f.primary, "primary"), (new, "owner"), (mid, "owner")]
        );
        assert_eq!(p.short, None);
        assert!(!p.members.iter().any(|(i, _)| *i == old), "min is met");
    }

    /// A device the owner holds but did not enroll, one enrolled by them but
    /// handed to someone else, and someone else's: none is the owner's own.
    #[test]
    fn the_owners_own_means_owner_and_enroller_both() {
        let mut f = Fleet::new();
        let handed_to_owner = f.owners(1);
        f.device(handed_to_owner).enrolled_by = Some(ObjectId::new());
        let handed_away = f.owners(2);
        f.device(handed_away).owner_user_id = ObjectId::new();
        let unknown_enroller = f.owners(3);
        f.device(unknown_enroller).enrolled_by = None;
        let p = place(&rules(2, 4), &[], &f.session(), &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")]);
        assert_eq!(p.short, Some(Short::NoDevice));
    }

    /// A device whose owner did not turn `hive_replica` on, an ephemeral
    /// one, and the primary itself are never members.
    #[test]
    fn only_a_permanent_device_that_offers_is_a_member() {
        let mut f = Fleet::new();
        let silent = f.owners(1);
        f.device(silent).replica = false;
        let ephemeral = f.owners(2);
        f.device(ephemeral).ephemeral = true;
        f.devices.push(DeviceFacts {
            id: f.primary,
            ..f.devices[0].clone()
        });
        f.device(f.primary).replica = true;
        let p = place(&rules(2, 4), &[], &f.session(), &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")], "{p:?}");
    }

    /// Archive replicas come before the owner's devices, in the policy's
    /// order, only while `archive` is on and only those that offer
    /// themselves; one of them that is also the owner's is placed once.
    #[test]
    fn designated_archives_that_offer_come_first() {
        let mut f = Fleet::new();
        let laptop = f.owners(1);
        let nas = f.owners(5000);
        f.device(nas).archive = true;
        let other = ObjectId::new();
        f.devices.push(DeviceFacts {
            id: other,
            owner_user_id: ObjectId::new(),
            enrolled_by: Some(ObjectId::new()),
            tags: Vec::new(),
            ephemeral: false,
            replica: true,
            archive: true,
            last_seen_at: DateTime::from_millis(0),
        });
        let declined = f.owners(9);
        f.device(declined).enrolled_by = Some(ObjectId::new());
        let archives = [other, declined, nas];
        let p = place(&rules(4, 4), &archives, &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [
                (f.primary, "primary"),
                (other, "archive"),
                (nas, "archive"),
                (laptop, "owner"),
            ],
            "`declined` is designated but does not offer itself"
        );
        let p = place(&rules(3, 4), &archives, &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [(f.primary, "primary"), (other, "archive"), (nas, "archive")],
            "every archive the rules allow; the owner's devices only until min"
        );
        let mut off = rules(3, 4);
        off.archive = false;
        let p = place(&off, &archives, &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [(f.primary, "primary"), (laptop, "owner"), (nas, "owner")],
            "archive off: only the owner's own, the NAS among them"
        );
        let p = place(&rules(2, 2), &archives, &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [(f.primary, "primary"), (other, "archive")],
            "never more than max"
        );
    }

    /// With nothing to prefer, only archives join.
    #[test]
    fn an_empty_prefer_takes_none_of_the_owners_devices() {
        let mut f = Fleet::new();
        f.owners(1);
        let mut r = rules(2, 4);
        r.prefer.clear();
        let p = place(&r, &[], &f.session(), &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")]);
    }

    /// A primary tagged with a restricted tag, in any case, restricts the
    /// session; then only devices carrying it too hold copies, and a session
    /// left short says why.
    #[test]
    fn a_restricted_tag_only_takes_devices_out() {
        let mut f = Fleet::new();
        let tagged = f.owners(100);
        f.device(tagged).tags = vec!["PROD".into()];
        let untagged = f.owners(1);
        let primary_tags = vec!["Prod".to_string(), "linux".to_string()];
        let s = SessionFacts {
            primary_tags: &primary_tags,
            ..f.session()
        };
        let p = place(&rules(2, 4), &[], &s, &f.devices);
        assert_eq!(p.restricted_tags, ["prod"], "the policy's spelling");
        assert_eq!(ids(&p), [(f.primary, "primary"), (tagged, "owner")]);
        assert!(!p.members.iter().any(|(i, _)| *i == untagged));
        f.device(tagged).tags.clear();
        let p = place(&rules(2, 4), &[], &s, &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")]);
        assert_eq!(p.short, Some(Short::Restricted));
        let p = place(&rules(2, 4), &[], &f.session(), &f.devices);
        assert_eq!(
            ids(&p),
            [(f.primary, "primary"), (untagged, "owner")],
            "an untagged primary restricts nothing"
        );
    }

    /// A tag the session already carries stays, though its primary no longer
    /// carries it and the policy no longer names it.
    #[test]
    fn a_session_once_restricted_stays_restricted() {
        let mut f = Fleet::new();
        let untagged = f.owners(1);
        let pci = f.owners(2);
        f.device(pci).tags = vec!["pci".into()];
        let carried = vec!["pci".to_string()];
        let s = SessionFacts {
            restricted: &carried,
            ..f.session()
        };
        let p = place(&rules(2, 4), &[], &s, &f.devices);
        assert_eq!(p.restricted_tags, ["pci"]);
        assert_eq!(ids(&p), [(f.primary, "primary"), (pci, "owner")]);
        assert!(!p.members.iter().any(|(i, _)| *i == untagged));
    }

    /// An adopted session is placed where it was adopted and nowhere else.
    #[test]
    fn an_adopted_session_stays_where_it_was_adopted() {
        let mut f = Fleet::new();
        f.owners(1);
        let s = SessionFacts {
            origin: SessionOrigin::Adopted,
            ..f.session()
        };
        let p = place(&rules(2, 4), &[], &s, &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary")]);
        assert_eq!(p.short, Some(Short::Adopted));
    }

    /// Two devices seen at the same moment are taken in id order, so a
    /// placement is the same whenever it is computed.
    #[test]
    fn a_tie_is_broken_by_id() {
        let mut f = Fleet::new();
        let a = f.owners(5);
        let b = f.owners(5);
        let first = a.min(b);
        let p = place(&rules(2, 4), &[], &f.session(), &f.devices);
        assert_eq!(ids(&p), [(f.primary, "primary"), (first, "owner")]);
    }
}
