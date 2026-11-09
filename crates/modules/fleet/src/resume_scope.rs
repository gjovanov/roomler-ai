// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-138 — `GET` / `PUT /api/admin/rc-resume-scope`: the orgs and devices a
//! remote session may outlive its device's control socket on (spec §4.1, D4).
//!
//! A platform admin's list. Anyone else gets the 404 every `/api/admin/*`
//! route gives (`require_platform_admin`), and gets it before the body is
//! read, so a malformed body does not tell them the route exists either. The
//! GET answers the list beside `rc.session_resume_secs`, the window it is
//! announced under, so the operator reads both halves at once; the PUT
//! replaces the list and answers the GET's shape.
//!
//! The store lives here, in `fleet`, because the agent hub that reads it when
//! a device registers is fleet's: `remote` and `network` reach fleet, fleet
//! reaches neither, so no hook is needed.
//!
//! P1a keeps the list and nothing reads it: with the window at 0, the
//! default, the server behaves as it did whatever the list holds. From P1b
//! the hub reads it at registration
//! ([`RcResumeScopeDao::covers`](roomler_ai_services::dao::rc_resume_scope::RcResumeScopeDao::covers))
//! and re-announces the window when it changes ([`scope_changed`]).

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
};
use bson::{DateTime, oid::ObjectId};
use roomler_ai_services::dao::rc_resume_scope::{ResumeScopeEntry, ResumeScopeKind};
use roomler_core::{ApiError, extractors::auth::AuthUser, guards::require_platform_admin};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::FleetState;

/// The longest note an entry keeps.
pub const MAX_NOTE_LEN: usize = 200;
/// The most entries the list holds: a ceiling, not a page size.
pub const MAX_ENTRIES: usize = 1_000;

/// One entry as the route answers it: ids as hex, the time as RFC 3339.
#[derive(Debug, Serialize, PartialEq)]
pub struct ScopeRow {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: String,
    pub at: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ScopeResponse {
    /// `rc.session_resume_secs`. 0 is off, whatever the list holds.
    pub window_secs: u64,
    pub entries: Vec<ScopeRow>,
}

impl ScopeResponse {
    fn of(window_secs: u64, entries: &[ResumeScopeEntry]) -> Self {
        Self {
            window_secs,
            entries: entries
                .iter()
                .map(|e| ScopeRow {
                    kind: e.kind.as_str(),
                    id: e.id.map(|id| id.to_hex()),
                    note: e.note.clone(),
                    by: e.by.to_hex(),
                    at: e.at.try_to_rfc3339_string().unwrap_or_default(),
                })
                .collect(),
        }
    }
}

/// `GET /api/admin/rc-resume-scope`
pub async fn get_scope(
    State(state): State<FleetState>,
    auth: AuthUser,
) -> Result<Json<ScopeResponse>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let entries = state.rc_resume_scope.list().await?;
    Ok(Json(ScopeResponse::of(
        state.settings.rc.session_resume_secs,
        &entries,
    )))
}

#[derive(Debug, Deserialize)]
pub struct PutEntry {
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PutBody {
    pub entries: Vec<PutEntry>,
}

/// `PUT /api/admin/rc-resume-scope` — replace the list:
/// `{"entries": [{"kind": "all" | "org" | "device", "id": "<hex>", "note": "…"}]}`.
pub async fn put_scope(
    State(state): State<FleetState>,
    auth: AuthUser,
    body: Result<Json<PutBody>, JsonRejection>,
) -> Result<Json<ScopeResponse>, ApiError> {
    require_platform_admin(&state, &auth)?;
    let Json(body) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    let entries = validate_entries(body.entries, auth.user_id, DateTime::now())?;
    let before = state.rc_resume_scope.list().await?;
    state.rc_resume_scope.replace(&entries).await?;
    let after = state.rc_resume_scope.list().await?;
    scope_changed(auth.user_id, &before, &after);
    Ok(Json(ScopeResponse::of(
        state.settings.rc.session_resume_secs,
        &after,
    )))
}

/// The one place a change to the scope goes out from. P1a only records it:
/// nothing reads the scope yet. FR-138 P1b re-announces the window from here,
/// to this pod's connected devices whose cover changed and, over the rc ctrl
/// lane ([`crate::ctrl::publish_rc_ctrl`]), to every other pod's, as FR-92's
/// keep-busy deny reaches them. `0` withdraws it, so a change needs no roll
/// and a device taken out of the scope loses its window at once.
fn scope_changed(admin: ObjectId, before: &[ResumeScopeEntry], after: &[ResumeScopeEntry]) {
    let added = after
        .iter()
        .filter(|e| !before.iter().any(|b| b.same_target(e)))
        .count();
    let removed = before
        .iter()
        .filter(|b| !after.iter().any(|e| e.same_target(b)))
        .count();
    info!(
        %admin,
        added,
        removed,
        entries = after.len(),
        "rc resume scope saved"
    );
}

/// The PUT's contract, pure so it is testable without a server: at most
/// [`MAX_ENTRIES`]; a known kind; a 24-hex id for `org` and `device` and none
/// for `all`; a note of at most [`MAX_NOTE_LEN`] printable characters, or
/// none; a target named twice is folded (the first wins) rather than refused.
/// Whether an id names a live org or device is not checked: one that names
/// nothing matches nothing.
pub fn validate_entries(
    rows: Vec<PutEntry>,
    by: ObjectId,
    at: DateTime,
) -> Result<Vec<ResumeScopeEntry>, ApiError> {
    if rows.len() > MAX_ENTRIES {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_ENTRIES} entries"
        )));
    }
    let mut out: Vec<ResumeScopeEntry> = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let kind = ResumeScopeKind::parse(row.kind.trim()).ok_or_else(|| {
            ApiError::BadRequest(format!(
                "entries[{i}].kind must be all, org or device, got {:?}",
                row.kind
            ))
        })?;
        let raw_id = row.id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let id = match (kind, raw_id) {
            (ResumeScopeKind::All, None) => None,
            (ResumeScopeKind::All, Some(_)) => {
                return Err(ApiError::BadRequest(format!(
                    "entries[{i}]: `all` names no id"
                )));
            }
            (_, None) => {
                return Err(ApiError::BadRequest(format!(
                    "entries[{i}].id is required for {}",
                    kind.as_str()
                )));
            }
            (_, Some(raw)) => Some(ObjectId::parse_str(raw).map_err(|_| {
                ApiError::BadRequest(format!(
                    "entries[{i}].id must be a 24-hex ObjectId, got {raw:?}"
                ))
            })?),
        };
        let note = match row.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(n) if n.chars().count() > MAX_NOTE_LEN => {
                return Err(ApiError::BadRequest(format!(
                    "entries[{i}].note is longer than {MAX_NOTE_LEN} characters"
                )));
            }
            Some(n) if n.chars().any(char::is_control) => {
                return Err(ApiError::BadRequest(format!(
                    "entries[{i}].note must be printable text"
                )));
            }
            Some(n) => Some(n.to_string()),
        };
        let entry = ResumeScopeEntry {
            kind,
            id,
            note,
            by,
            at,
        };
        if out.iter().any(|e| e.same_target(&entry)) {
            continue;
        }
        out.push(entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG: &str = "6a975e17971e2d55ddcdedb2";
    const DEVICE: &str = "6a975e17971e2d55ddcdedb3";

    fn row(kind: &str, id: Option<&str>, note: Option<&str>) -> PutEntry {
        PutEntry {
            kind: kind.into(),
            id: id.map(str::to_string),
            note: note.map(str::to_string),
        }
    }

    fn check(rows: Vec<PutEntry>) -> Result<Vec<ResumeScopeEntry>, ApiError> {
        validate_entries(rows, ObjectId::new(), DateTime::now())
    }

    fn refused(rows: Vec<PutEntry>) -> String {
        match check(rows) {
            Err(ApiError::BadRequest(msg)) => msg,
            other => panic!("expected a 400, got {other:?}"),
        }
    }

    #[test]
    fn the_three_kinds_are_taken_as_they_name_their_targets() {
        let got = check(vec![
            row(" all ", None, Some("  P4: the whole fleet ")),
            row("org", Some(ORG), None),
            row("device", Some(&format!(" {DEVICE} ")), Some("   ")),
        ])
        .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].kind, got[0].id), (ResumeScopeKind::All, None));
        assert_eq!(got[0].note.as_deref(), Some("P4: the whole fleet"));
        assert_eq!(got[1].id, Some(ObjectId::parse_str(ORG).unwrap()));
        assert_eq!(got[2].kind, ResumeScopeKind::Device);
        assert_eq!(got[2].id, Some(ObjectId::parse_str(DEVICE).unwrap()));
        assert_eq!(got[2].note, None, "a blank note is no note");
        assert!(check(vec![]).unwrap().is_empty(), "an empty list is valid");
    }

    #[test]
    fn a_wrong_kind_a_missing_or_stray_id_and_a_bad_note_are_refused() {
        assert!(refused(vec![row("tenant", Some(ORG), None)]).contains("entries[0].kind"));
        assert!(refused(vec![row("Org", Some(ORG), None)]).contains("entries[0].kind"));
        assert!(refused(vec![row("all", Some(ORG), None)]).contains("`all` names no id"));
        assert!(refused(vec![row("org", None, None)]).contains("required for org"));
        assert!(refused(vec![row("device", Some(" "), None)]).contains("required for device"));
        assert!(refused(vec![row("device", Some("not-hex"), None)]).contains("24-hex"));
        let long = "x".repeat(MAX_NOTE_LEN + 1);
        assert!(refused(vec![row("all", None, Some(&long))]).contains("longer than"));
        assert!(refused(vec![row("all", None, Some("a\nb"))]).contains("printable"));
        let many = (0..=MAX_ENTRIES).map(|_| row("all", None, None)).collect();
        assert!(refused(many).contains("at most"));
    }

    #[test]
    fn a_target_named_twice_is_kept_once_with_its_first_note() {
        let got = check(vec![
            row("device", Some(DEVICE), Some("first")),
            row("org", Some(DEVICE), None),
            row("device", Some(DEVICE), Some("second")),
            row("all", None, None),
            row("all", None, Some("again")),
        ])
        .unwrap();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].note.as_deref(), Some("first"));
        assert_eq!(
            got[1].kind,
            ResumeScopeKind::Org,
            "the same id under another kind is another target"
        );
        assert_eq!(got[2].note, None);
    }

    #[test]
    fn the_answer_names_ids_as_hex_and_leaves_absent_parts_out() {
        let by = ObjectId::parse_str(ORG).unwrap();
        let entries = check(vec![
            row("all", None, None),
            row("device", Some(DEVICE), Some("n")),
        ])
        .unwrap()
        .into_iter()
        .map(|e| ResumeScopeEntry { by, ..e })
        .collect::<Vec<_>>();
        let v = serde_json::to_value(ScopeResponse::of(60, &entries)).unwrap();
        assert_eq!(v["window_secs"], 60);
        assert_eq!(v["entries"][0]["kind"], "all");
        assert!(v["entries"][0].get("id").is_none() && v["entries"][0].get("note").is_none());
        assert_eq!(v["entries"][1]["id"], DEVICE);
        assert_eq!(v["entries"][1]["by"], ORG);
        assert!(v["entries"][1]["at"].as_str().unwrap().contains('T'));
    }
}
