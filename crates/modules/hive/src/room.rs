// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P0d — a session's room: where the org sees it happen.
//!
//! A session is a `Secret` room bound to `{module: "hive", ref: <session id>}`
//! — a non-member gets 404, not proof the session exists. The session itself
//! authors what appears in it: a note when it starts, is refused or ends, and
//! one STUB per turn — who asked, how it stands, how many steps, how long, what
//! it cost — updated in place as the turn finishes. Never what was asked or
//! answered: that streams to a viewer from a replica (P0d-2), and the stub is
//! built from `rc:hive.turn`, whose field set cannot carry it.
//!
//! Everything here is best-effort past the room's creation: a note that fails
//! to post is logged, never a reason to refuse a start or drop a report.

use bson::oid::ObjectId;
use roomler_ai_db::models::Binding;
use roomler_ai_remote_control::hive::HiveTurnStatus;
use tracing::{debug, warn};

use crate::HiveState;
use crate::model::{AgentSession, TurnStub};

/// The module id rooms and stubs are bound to.
const MODULE: &str = "hive";

/// The room's path: unique by construction (the session id), so a session
/// named like an existing room cannot collide with it.
pub(crate) fn room_path(session: ObjectId) -> String {
    format!("hive-{}", session.to_hex())
}

/// Open the session's room, with its owner as the only member.
pub(crate) async fn open(
    state: &HiveState,
    tenant_id: ObjectId,
    session: ObjectId,
    title: &str,
    owner: ObjectId,
) -> Result<ObjectId, roomler_ai_services::dao::base::DaoError> {
    let room = state
        .chat
        .create_bound_room(
            tenant_id,
            title.to_string(),
            room_path(session),
            owner,
            Binding::new(MODULE, session.to_hex()),
        )
        .await?;
    Ok(room.id.expect("a stored room has an id"))
}

/// Post a note the session authors into its room. Best-effort.
pub(crate) async fn note(state: &HiveState, s: &AgentSession, text: String) {
    let (Some(sid), Some(room)) = (s.id, s.room_id) else {
        return;
    };
    if let Err(e) = state
        .chat
        .post_agent_message(
            s.tenant_id,
            room,
            sid,
            s.author_display(),
            Binding::new(MODULE, sid.to_hex()),
            text,
        )
        .await
    {
        warn!(session = %sid, %e, "hive: a note was not posted to the session's room");
    }
}

/// Post the ended note for a session the CALLER just ended on the server's
/// side. Call it only when the caller's compare-and-set applied, so one end
/// gets one note.
pub(crate) async fn note_ended(state: &HiveState, sid: ObjectId) {
    match state.sessions.find(sid).await {
        Ok(Some(s)) => note(state, &s, ended_note(&s)).await,
        Ok(None) => {}
        Err(e) => warn!(session = %sid, %e, "hive: the ended session was not read for its note"),
    }
}

fn device_label(s: &AgentSession) -> String {
    if s.location.device_name.is_empty() {
        "the device".to_string()
    } else {
        format!("**{}**", md_escape(&s.location.device_name))
    }
}

/// "Started" — where, and as whom the device says it runs.
pub(crate) fn started_note(s: &AgentSession) -> String {
    let mut t = format!(
        "▶️ Started on {} in {}",
        device_label(s),
        md_escape(&s.location.folder)
    );
    if let Some(account) = &s.location.account {
        t.push_str(&format!(" as **{}**", md_escape(account)));
    }
    t
}

/// "Refused" — which of the device's gates said no, in plain words.
pub(crate) fn refused_note(s: &AgentSession) -> String {
    let word = s.refusal.as_deref().unwrap_or("other");
    let mut t = format!(
        "⛔ {} refused the session: {}",
        device_label(s),
        crate::routes::device_refusal_message(word)
    );
    if let Some(detail) = &s.detail {
        t.push_str(&format!(" — {}", md_escape(detail)));
    }
    t
}

/// "Ended" — and why, by the record's `end_reason`.
pub(crate) fn ended_note(s: &AgentSession) -> String {
    let why = match s.end_reason.as_deref() {
        Some("stopped") => "stopped",
        Some("exited") => "the agent exited",
        Some("member_removed") => "its starter left the organization",
        Some("device_removed") => "its device was removed",
        Some("tenant_archived") => "the organization was archived",
        Some("never_answered") => "the device never answered",
        _ => "ended",
    };
    let mut t = format!("⏹ Session ended — {why}");
    if let Some(detail) = &s.detail {
        t.push_str(&format!(": {}", md_escape(detail)));
    }
    t
}

/// What a turn report says, as the stub renders it.
pub(crate) struct TurnReport {
    pub turn: u32,
    pub status: Option<HiveTurnStatus>,
    pub prompted_by: Option<ObjectId>,
    pub steps: u32,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
}

/// Create the turn's stub, or update it when this turn already has one.
/// Reports for a turn OLDER than the newest stubbed one change nothing.
pub(crate) async fn turn_stub(state: &HiveState, s: &AgentSession, r: TurnReport) {
    let (Some(sid), Some(room)) = (s.id, s.room_id) else {
        return;
    };
    let asked_by = match r.prompted_by {
        Some(uid) => state
            .users
            .find_display_names(&[uid])
            .await
            .ok()
            .and_then(|m| m.get(&uid).cloned()),
        None => None,
    };
    let text = stub_text(&r, asked_by.as_deref());
    match s.last_turn {
        Some(t) if t.turn == r.turn => {
            match state
                .chat
                .update_agent_message(s.tenant_id, room, t.message_id, text)
                .await
            {
                Ok(true) => {}
                Ok(false) => debug!(session = %sid, turn = r.turn, "hive: the turn's stub is gone"),
                Err(e) => warn!(session = %sid, %e, "hive: a turn stub was not updated"),
            }
        }
        Some(t) if t.turn > r.turn => {
            debug!(session = %sid, turn = r.turn, newest = t.turn, "hive: a report for an older turn");
        }
        _ => {
            let posted = state
                .chat
                .post_agent_message(
                    s.tenant_id,
                    room,
                    sid,
                    s.author_display(),
                    Binding::new(MODULE, format!("{}#{}", sid.to_hex(), r.turn)),
                    text,
                )
                .await;
            match posted {
                Ok(message_id) => {
                    let stub = TurnStub {
                        turn: r.turn,
                        message_id,
                    };
                    if let Err(e) = state.sessions.set_last_turn(sid, stub).await {
                        warn!(session = %sid, %e, "hive: the newest turn was not recorded");
                    }
                }
                Err(e) => warn!(session = %sid, %e, "hive: a turn stub was not posted"),
            }
        }
    }
}

/// The stub as markdown — what any chat client renders, whether or not it
/// knows Hive. Only numbers and an escaped display name go in.
pub(crate) fn stub_text(r: &TurnReport, asked_by: Option<&str>) -> String {
    let (mark, how) = match r.status {
        Some(HiveTurnStatus::Running) | None => ("⏳", "working…"),
        Some(HiveTurnStatus::Ok) => ("✅", "done"),
        Some(HiveTurnStatus::Error) => ("❌", "ended with an error"),
        Some(HiveTurnStatus::Interrupted) => ("⏹", "interrupted"),
    };
    let mut parts = vec![format!("{mark} **Turn {}** — {how}", r.turn)];
    if r.steps > 0 {
        parts.push(format!(
            "{} step{}",
            r.steps,
            if r.steps == 1 { "" } else { "s" }
        ));
    }
    if let Some(ms) = r.duration_ms {
        parts.push(format_duration(ms));
    }
    if let Some(usd) = r.cost_usd.filter(|c| c.is_finite() && *c >= 0.0) {
        parts.push(format!("${usd:.2}"));
    }
    if let Some(name) = asked_by.filter(|n| !n.trim().is_empty()) {
        parts.push(format!("asked by **{}**", md_escape(name)));
    }
    parts.join(" · ")
}

fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min {} s", secs / 60, secs % 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

/// Escape what markdown would read as syntax, so a display name or a
/// device's words render as the text they are — never as a link, an image or
/// emphasis that changes what the note says. (The renderer's sanitiser is the
/// XSS boundary; this is about the note saying what it means.)
pub(crate) fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.push(' ');
            continue;
        }
        if matches!(
            c,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
                | '<'
                | '>'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(status: Option<HiveTurnStatus>) -> TurnReport {
        TurnReport {
            turn: 3,
            status,
            prompted_by: None,
            steps: 5,
            duration_ms: Some(42_000),
            cost_usd: Some(0.123),
        }
    }

    #[test]
    fn a_stub_says_how_the_turn_stands_and_nothing_it_said() {
        assert_eq!(
            stub_text(&report(Some(HiveTurnStatus::Ok)), Some("Alice")),
            "✅ **Turn 3** — done · 5 steps · 42 s · $0.12 · asked by **Alice**"
        );
        assert_eq!(
            stub_text(&report(Some(HiveTurnStatus::Running)), None),
            "⏳ **Turn 3** — working… · 5 steps · 42 s · $0.12"
        );
        let mut r = report(Some(HiveTurnStatus::Interrupted));
        r.steps = 1;
        r.duration_ms = Some(3_725_000);
        r.cost_usd = Some(f64::NAN);
        assert_eq!(
            stub_text(&r, None),
            "⏹ **Turn 3** — interrupted · 1 step · 1 h 2 min"
        );
    }

    /// A display name is text, not markdown: no link, image or emphasis
    /// survives into the note.
    #[test]
    fn a_name_cannot_inject_markdown() {
        let text = stub_text(
            &report(Some(HiveTurnStatus::Ok)),
            Some("x](javascript:alert(1)) ![img](http://e/x.png) **bold**\nnext"),
        );
        assert!(!text.contains("](java"), "{text}");
        assert!(!text.contains("![img]("), "{text}");
        assert!(!text.contains('\n'), "{text}");
        assert!(text.contains(r"\*\*bold\*\*"), "{text}");
    }

    #[test]
    fn the_room_path_is_the_session() {
        let sid = ObjectId::new();
        assert_eq!(room_path(sid), format!("hive-{}", sid.to_hex()));
    }
}
