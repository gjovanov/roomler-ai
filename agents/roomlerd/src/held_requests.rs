// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3439 — session requests that arrive before the capability probe's first answer.
//!
//! On a host that is slow to probe, the control WS opens before the probe has
//! answered (`signaling::run` waits `encode::caps::FIRST_ANSWER_WAIT` at most), so
//! the device is reachable while its probe children run. A session asked for then
//! waits here, in its connection, and is handled when the answer lands, as if it
//! had arrived then.
//!
//! It is not served from the interim answer. No driver code runs in the daemon
//! before a probe child has opened its cell, and a session opens Media Foundation
//! in the daemon whatever the caps say (`open_default`). A host whose driver
//! faults there would crash on every start, with a viewer that reconnects by
//! itself, before its probe could finish.
//!
//! A request's windows count from when the hub sent it, so a held one is handled
//! with what is left of them. One whose window runs out first is dropped
//! unanswered, and the hub's own consent timer ends it, as it ends any request a
//! device leaves unanswered. A held request dies with its connection: the hub
//! pushes a pending request again to the next one.

use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::signaling::ServerMsg;

/// At most this many wait at once. A viewer that keeps asking replaces its own
/// request (the same session); past this, the oldest goes.
const MOST: usize = 8;

/// The session requests one connection holds until the probe's first answer.
#[derive(Default)]
pub struct HeldRequests {
    held: Vec<(Instant, ServerMsg)>,
}

impl HeldRequests {
    /// Hold `msg` if it is a session request, and give anything else back.
    pub fn hold(&mut self, msg: ServerMsg, now: Instant) -> Result<(), ServerMsg> {
        let Some(session_id) = session_of(&msg) else {
            return Err(msg);
        };
        // The hub pushes a pending request again after a socket flap: the same
        // session, still waiting since the first push.
        let since = match self
            .held
            .iter()
            .position(|(_, m)| session_of(m) == Some(session_id))
        {
            Some(i) => self.held.remove(i).0,
            None => now,
        };
        if self.held.len() >= MOST {
            let (_, oldest) = self.held.remove(0);
            tracing::warn!(
                session_id = ?session_of(&oldest),
                "rc:request — too many held for the capability probe's first answer; the \
                 oldest is dropped, and the hub ends it"
            );
        }
        tracing::info!(
            %session_id,
            "rc:request — held until the capability probe's first answer, which picks the \
             session's encoder"
        );
        self.held.push((since, msg));
        Ok(())
    }

    /// The session ended while its request waited.
    pub fn forget(&mut self, session_id: ObjectId) {
        self.held.retain(|(_, m)| session_of(m) != Some(session_id));
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Every held request, oldest first, with what is left of its windows. One
    /// with nothing left is dropped.
    pub fn release(&mut self, now: Instant) -> Vec<ServerMsg> {
        self.held
            .drain(..)
            .filter_map(|(since, msg)| after_waiting(msg, now.saturating_duration_since(since)))
            .collect()
    }
}

fn session_of(msg: &ServerMsg) -> Option<ObjectId> {
    match msg {
        ServerMsg::Request { session_id, .. } => Some(*session_id),
        _ => None,
    }
}

/// `msg` as if it had arrived `waited` later. The consent window loses the wait,
/// in whole seconds rounded up, so this side never runs past the hub's timer.
/// `None` once that window has gone. The on-host prompt keeps its own window as
/// far as the session's allows, so a person asked late still gets the time to
/// answer it.
pub(crate) fn after_waiting(mut msg: ServerMsg, waited: Duration) -> Option<ServerMsg> {
    if let ServerMsg::Request {
        session_id,
        consent_timeout_secs,
        host_prompt_timeout_secs,
        ..
    } = &mut msg
    {
        let waited_s = waited.as_secs() + u64::from(waited.subsec_nanos() > 0);
        let waited_s = u32::try_from(waited_s).unwrap_or(u32::MAX);
        let left = consent_timeout_secs.saturating_sub(waited_s);
        if left == 0 {
            tracing::info!(
                %session_id,
                waited_s,
                "rc:request — its consent window ran out while it waited for the capability \
                 probe's first answer; the hub ends it"
            );
            return None;
        }
        tracing::info!(
            %session_id,
            waited_s,
            left_s = left,
            "rc:request — handled now that the capability probe has answered"
        );
        *consent_timeout_secs = left;
        if let Some(host) = host_prompt_timeout_secs.as_mut() {
            *host = (*host).min(left);
        }
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::permissions::Permissions;

    fn request(session_id: ObjectId, consent: u32, host: Option<u32>) -> ServerMsg {
        serde_json::from_value(serde_json::json!({
            "t": "rc:request",
            "session_id": session_id.to_hex(),
            "controller_user_id": ObjectId::new().to_hex(),
            "controller_name": "a viewer",
            "permissions": Permissions::VIEW,
            "consent_timeout_secs": consent,
            "host_prompt_timeout_secs": host,
        }))
        .expect("a request")
    }

    fn windows(msg: &ServerMsg) -> (u32, Option<u32>) {
        match msg {
            ServerMsg::Request {
                consent_timeout_secs,
                host_prompt_timeout_secs,
                ..
            } => (*consent_timeout_secs, *host_prompt_timeout_secs),
            other => panic!("not a request: {other:?}"),
        }
    }

    #[test]
    fn a_held_request_keeps_what_is_left_of_its_window() {
        let sid = ObjectId::new();
        let out = after_waiting(request(sid, 30, None), Duration::from_millis(12_300))
            .expect("time left");
        // 12.3 s waited counts as 13: never past the hub's timer.
        assert_eq!(windows(&out), (17, None));
    }

    #[test]
    fn a_request_whose_window_ran_out_is_dropped() {
        let sid = ObjectId::new();
        assert!(after_waiting(request(sid, 30, None), Duration::from_secs(30)).is_none());
        assert!(after_waiting(request(sid, 30, None), Duration::from_millis(29_001)).is_none());
        assert!(after_waiting(request(sid, 30, None), Duration::from_secs(29)).is_some());
    }

    #[test]
    fn the_host_prompt_keeps_its_own_window_while_the_session_allows() {
        // prompt_then_email: a 30 s modal inside a five-minute session window.
        let sid = ObjectId::new();
        let out =
            after_waiting(request(sid, 300, Some(30)), Duration::from_secs(40)).expect("time left");
        assert_eq!(windows(&out), (260, Some(30)));
        // A plain prompt: the modal never outlasts the session's window.
        let out =
            after_waiting(request(sid, 30, Some(30)), Duration::from_secs(10)).expect("time left");
        assert_eq!(windows(&out), (20, Some(20)));
    }

    #[test]
    fn only_a_request_is_held() {
        let mut held = HeldRequests::default();
        let other: ServerMsg = serde_json::from_value(serde_json::json!({
            "t": "rc:terminate",
            "session_id": ObjectId::new().to_hex(),
            "reason": "controller_hangup",
        }))
        .expect("a terminate");
        assert!(held.hold(other, Instant::now()).is_err());
        assert!(held.is_empty());
    }

    #[test]
    fn a_pushed_again_request_waits_since_the_first_push() {
        let mut held = HeldRequests::default();
        let sid = ObjectId::new();
        let first = Instant::now();
        held.hold(request(sid, 30, None), first).expect("held");
        // The hub's re-push after a flap, 20 s later.
        let again = first + Duration::from_secs(20);
        held.hold(request(sid, 30, None), again).expect("held");
        let out = held.release(first + Duration::from_secs(25));
        assert_eq!(out.len(), 1, "one session, one request");
        assert_eq!(windows(&out[0]), (5, None), "counted from the first push");
        assert!(held.is_empty());
    }

    #[test]
    fn a_session_that_ended_while_waiting_is_forgotten() {
        let mut held = HeldRequests::default();
        let (gone, stays) = (ObjectId::new(), ObjectId::new());
        let now = Instant::now();
        held.hold(request(gone, 30, None), now).expect("held");
        held.hold(request(stays, 30, None), now).expect("held");
        held.forget(gone);
        let out = held.release(now);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], ServerMsg::Request { session_id, .. } if *session_id == stays));
    }

    #[test]
    fn past_the_most_the_oldest_goes() {
        let mut held = HeldRequests::default();
        let now = Instant::now();
        let ids: Vec<ObjectId> = (0..=MOST).map(|_| ObjectId::new()).collect();
        for id in &ids {
            held.hold(request(*id, 30, None), now).expect("held");
        }
        let out = held.release(now);
        assert_eq!(out.len(), MOST);
        assert!(
            out.iter().all(
                |m| !matches!(m, ServerMsg::Request { session_id, .. } if *session_id == ids[0])
            )
        );
    }
}
