// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
pub mod fixtures;

/// #3199 — the way back from an expired activation link: one answer for every
/// address, both rate limits, and who can ever be mailed.
#[cfg(test)]
mod activation_resend_tests;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod channel_crud_tests;
#[cfg(test)]
mod channel_tests;
/// The `media:*` socket namespace admits only a live participant of the
/// room's call: org, room visibility and call session for `media:join`, the
/// connection's seat for every other command, on both sides of the socket.
#[cfg(test)]
mod conference_gate_tests;
#[cfg(test)]
mod conference_message_tests;
#[cfg(test)]
mod conference_tests;
/// FR-127 P1 — receive what is visible: `caps: ["view"]`, `media:view`, the
/// viewer's pause against the owner's (AC2), the kill switch, the other pod.
#[cfg(test)]
mod conference_view_tests;
#[cfg(test)]
mod export_tests;
#[cfg(test)]
mod file_tests;
#[cfg(test)]
mod message_tests;
#[cfg(test)]
mod multi_tenancy_tests;
#[cfg(test)]
mod reaction_tests;
#[cfg(test)]
mod recording_tests;

/// Tests of the fixture itself, not of the product — see the module docs.
#[cfg(test)]
mod harness_tests;

/// FR-69 — the composition baseline (routes, index plan, wire names) asserted
/// identical after every module move. See docs/fr/FR-69-modular-monolith.md.
#[cfg(test)]
mod composition_tests;

#[cfg(test)]
mod agent_crash_tests;
#[cfg(test)]
mod agent_e2e_tests;
#[cfg(test)]
mod agent_exec_tests;
#[cfg(test)]
mod agent_presence_tests;
/// FR-28 — the control socket's redial on the load balancer's 502, through the
/// real signaling loop and a scripted front.
#[cfg(test)]
mod agent_redial_tests;
/// #2267 — a removal (or an archive) tells a connected device at once with
/// `rc:goodbye AgentDeleted`, instead of leaving it its socket.
#[cfg(test)]
mod agent_removal_tests;
/// FR-84 D5a — `/api/agent/self/{devices,mesh}`: a device's own view of its
/// org is exactly its netmap ∪ itself (the negative control lives here).
#[cfg(test)]
mod agent_self_tests;
/// #3517 — the agent socket's disconnect line says how the socket ended.
#[cfg(test)]
mod agent_socket_end_tests;
#[cfg(test)]
mod agent_tests;
/// FR-88 P1a — sign-up attribution: stored once at creation, absent from
/// every user-facing response, parked across OAuth, counted for the admin.
#[cfg(test)]
mod attribution_tests;
#[cfg(test)]
mod billing_tests;
#[cfg(test)]
mod cluster_tests;
#[cfg(test)]
mod cors_tests;
#[cfg(test)]
mod device_list_tests;
#[cfg(test)]
mod device_naming_tests;
#[cfg(test)]
mod enroll_key_tests;
#[cfg(test)]
mod ephemeral_tests;
#[cfg(test)]
mod exec_service_tests;
// FR-52 P3c — the loopback proof: an outsider logs into the REAL agent through
// the server. ⚠️ A test file that is not registered here compiles green and
// runs nothing (P1 shipped one that way for a day).
#[cfg(test)]
mod extauth_tests;
#[cfg(test)]
mod external_access_tests;
/// FR-102 P6 — the purestat puller (seven closed days frozen once, a frozen
/// day never rewritten, a failed pull recorded rather than read as 0) and
/// the Campaigns table (both planes joined by the normalised key; a revoked
/// key reads `not_measured`, AC16's control; unmounted modules named).
#[cfg(test)]
mod growth_campaigns_tests;
/// FR-102 P8 — the ingest key and routes (AC18: a snapshot pushed twice is
/// stored once; an unknown platform and post text are refused; a wrong key
/// is a 404), and the Channels table's join with the reach column it fills.
#[cfg(test)]
mod growth_channels_tests;
/// FR-102 P1d — the daily digest: every section's exact numbers on a seeded
/// day, the once-only claim under two concurrent runs (AC23), the routes,
/// the no-address rule, and the purestat client against a fake server.
#[cfg(test)]
mod growth_digest_tests;
/// FR-102 P2 — the account funnel with pending/settled (AC6),
/// `growth_org_daily` surviving the audit TTL and dying with the org
/// (AC8), the weekly cohorts and the org views.
#[cfg(test)]
mod growth_funnel_tests;
/// FR-102 P7 — the GitHub puller against a fake GitHub (the closed day's
/// snapshot read once in its window, releases paged, drafts out, traffic
/// only with a token, a refused token recorded rather than zeroed) and its
/// readers (snapshots differenced, a gap read as null, never 0).
#[cfg(test)]
mod growth_github_tests;
/// FR-102 P5 — the newsletter's delivery side: SendGrid's signed Event
/// Webhook refusing what it cannot verify before parsing it, recording only
/// the four events, suppressing a hard bounce at the next send (AC14's test
/// half); the per-issue unsubscribe; the per-issue table and the digest.
#[cfg(test)]
mod growth_newsletter_tests;
/// FR-102 P10 — the read key (AC20: the admin's numbers on every GET view,
/// the empty 404 on every write and on the admin surface), the weekly
/// roll-up's once-only claim and routes, and the alert rules firing once,
/// clearing, and mailing at most once a day.
#[cfg(test)]
mod growth_p10_tests;
/// FR-102 P9a — remote-desktop quality from what is already recorded: the
/// nightly freeze of `growth_quality_daily` from the four sources as they
/// are really written (the camelCase crash payload, the browser's
/// `rc.connect` records, the agents' own batches), both scopes, the 7-day
/// log horizon, the Quality view and the digest's and weekly's sections.
#[cfg(test)]
mod growth_quality_tests;
/// FR-102 P4 — billed revenue through Stripe's own signature scheme (AC13's
/// test half: the lifecycle's invoice amounts, MRR month by month, the
/// coupon control), the frozen months surviving an org's purge, the
/// backfill against a fake Stripe, and the quota-collision counter.
#[cfg(test)]
mod growth_revenue_tests;
/// FR-102 P7 — the search pullers against fakes of Google, Bing and the
/// blog's feed: only final Search Console days, the service account's
/// token exchange, the weighted position, URL Inspection into
/// `growth_content`, Bing at its own grain, every refusal recorded.
#[cfg(test)]
mod growth_search_tests;
/// FR-102 P3b — the setup wizard's opt-in diagnostics (AC12's server half):
/// a record stored with exactly its fields and nothing about the request,
/// every invalid field refused with nothing stored, the kill switch's empty
/// 404, the per-run cap, the nightly freeze into `growth_daily`, and the
/// digest's `wizard_steps` over opted-in runs.
#[cfg(test)]
mod growth_setup_diagnostics_tests;
/// FR-102 — growth observability: the campaign taxonomy at ingress (AC5),
/// the exclusion list and the set every growth number applies (AC4).
#[cfg(test)]
mod growth_tests;
/// FR-102 P3 — the install trace: the token ledger (AC11), the fetch
/// counters by client class with the updater never counted (AC10), and the
/// enroll's `install` claim as the row records it.
#[cfg(test)]
mod install_trace_tests;
#[cfg(test)]
mod invite_tests;
#[cfg(test)]
mod keep_busy_tests;
#[cfg(test)]
mod key_rotation_tests;
/// FR-115 P5 — the meeting-length cap: warned at 55, ended at 60, once.
#[cfg(test)]
mod meeting_cap_tests;
#[cfg(test)]
mod member_tests;
#[cfg(test)]
mod newsletter_tests;
#[cfg(test)]
mod notification_tests;
#[cfg(test)]
mod oauth_tests;
/// FR-115 P3 — an organization's concurrent remote-session cap: refused at
/// start with the holder named, per seat on Pro, carried across pods.
#[cfg(test)]
mod org_session_cap_tests;
#[cfg(test)]
mod overlay_growth_tests;
#[cfg(test)]
mod overlay_route_offer_tests;
#[cfg(test)]
mod overlay_tests;
#[cfg(test)]
mod pagination_tests;
/// FR-129 A1 — device pairing, the server half: the daemon and tunnel
/// flows end to end, two halves, two pods, the codes (a user code or a
/// forged device code retrieves nothing; the device code in no response
/// but start's), slow-down, expiry, denial, the gate re-checked at delivery,
/// the device cap and an archived org refusing a paired token like a pasted
/// one, the address and account ceilings, the kill switch, the audit trail.
#[cfg(test)]
mod pairing_tests;
/// FR-137 P2 — FR-90's two parse-only wire fields are refused, never served as
/// the owner's, and the audit rows Hive left still list.
#[cfg(test)]
mod parked_wire_tests;
#[cfg(test)]
mod pdf_export_tests;
/// FR-100 P1a — a device's identity key as the fleet stores and shows it, and
/// the real agent's hello carrying one it keeps beside its config.
#[cfg(test)]
mod peer_auth_key_tests;
#[cfg(test)]
mod peer_relay_mint_tests;
#[cfg(test)]
mod peer_relay_tests;
#[cfg(test)]
mod plan_limit_tests;
#[cfg(test)]
mod privacy_activity_tests;
#[cfg(test)]
mod privacy_gate_tests;
#[cfg(test)]
mod rate_limit_tests;
#[cfg(test)]
mod rc_idle_limit_tests;
#[cfg(test)]
mod rc_no_common_codec_tests;
#[cfg(test)]
mod rc_request_ceiling_tests;
#[cfg(test)]
mod rc_session_limits_tests;
#[cfg(test)]
mod rc_session_policy_tests;
#[cfg(test)]
mod relay_allowance_tests;
#[cfg(test)]
mod relay_region_tests;
#[cfg(test)]
mod remote_control_tests;
#[cfg(test)]
mod remote_recording_tests;
/// FR-40 P2 — a key a device rotated away from cannot be used to rejoin:
/// refused `key_retired` at join and DERP under the org's `enforce`, the
/// device with the verb ordered to rotate (once), `warn` and `off` admit.
#[cfg(test)]
mod retired_key_tests;
#[cfg(test)]
mod role_reconcile_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod room_visibility_tests;
/// FR-102 P9c — the carrier a session rides: the agent records it on
/// `rc:session.stats` once Active, and the server stores it on
/// `remote_sessions.carrier` — a code only, absent read as unknown, never
/// overwritten by a later sample with none.
#[cfg(test)]
mod session_carrier_tests;
/// FR-132 P1 (§3b) — the viewer's decode errors: a real viewer peer reports
/// one on the control channel, the real agent counts it and sends a sample at
/// once, and the server stores `remote_sessions.stats.decode_errors`; a
/// counted zero is stored as zero, and a sample that did not count never
/// overwrites a stored count.
#[cfg(test)]
mod session_decode_errors_tests;
/// FR-102 P9b — the post-session rating's server half: a party stores one
/// rating with codes only, everyone else is refused with a code, the kill
/// switch's empty 404, `purge_user`, and the digest's `quality.ratings`.
#[cfg(test)]
mod session_feedback_tests;
/// FR-135 R2 — `rc:media-plan` and `rc:viewer-hz` through a real agent and a real
/// viewer peer: the verb, the plan on the control channel, and the viewer's
/// refresh reaching the agent.
#[cfg(test)]
mod session_media_plan_tests;
/// FR-129 B1-s — `GET /api/setup/features`: the Phase B kill switch (one per
/// OS since B1-s2, the legacy `wizard_unix_daemon` for both; default off) and
/// `pairing`, each following its setting, read by a guest and a member alike.
#[cfg(test)]
mod setup_features_tests;
/// FR-55 P4 — a device that says it is going to sleep reads "asleep" until its
/// next hello, and only the connection that said hello can say so.
#[cfg(test)]
mod sleep_announce_tests;
#[cfg(test)]
mod ssh_grant_ack_tests;
#[cfg(test)]
mod ssh_service_tests;
#[cfg(test)]
mod stats_tests;
#[cfg(test)]
mod subscribe_tests;
#[cfg(test)]
mod tenant_archive_tests;
/// FR-103 — device credentials renew themselves, and a removal ends every
/// earlier token (agents and tunnel clients alike).
#[cfg(test)]
mod token_renewal_tests;
/// #1821, #1829 — what a removal leaves behind answers nothing: External
/// access on a removed device, and an overlay join on the socket it still holds.
#[cfg(test)]
mod tombstone_tests;
/// FR-111 P4a — the org's on-screen control presets: any member reads them,
/// `MANAGE_TENANT` writes them, a foreign id is a 404, a bad body is refused
/// with a code, and an org keeps at most fifty.
#[cfg(test)]
mod touch_preset_tests;
#[cfg(test)]
mod transfer_activity_tests;
#[cfg(test)]
mod tunnel_tests;
/// FR-115 P4b — coturn's per-allocation totals into `turn_bytes`, for the
/// org a charged TURN username names, once per allocation leg.
#[cfg(test)]
mod turn_usage_tests;
#[cfg(test)]
mod tutorial_tests;
#[cfg(test)]
mod usage_ledger_tests;
#[cfg(test)]
mod ws_auth_tests;
