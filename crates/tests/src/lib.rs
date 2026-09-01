// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
pub mod fixtures;

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod channel_crud_tests;
#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod conference_message_tests;
#[cfg(test)]
mod conference_tests;
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

#[cfg(test)]
mod agent_crash_tests;
#[cfg(test)]
mod agent_e2e_tests;
#[cfg(test)]
mod agent_exec_tests;
#[cfg(test)]
mod agent_presence_tests;
#[cfg(test)]
mod agent_tests;
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
mod ephemeral_tests;
#[cfg(test)]
mod invite_tests;
#[cfg(test)]
mod key_rotation_tests;
#[cfg(test)]
mod member_tests;
#[cfg(test)]
mod notification_tests;
#[cfg(test)]
mod oauth_tests;
#[cfg(test)]
mod overlay_growth_tests;
#[cfg(test)]
mod overlay_tests;
#[cfg(test)]
mod pagination_tests;
#[cfg(test)]
mod pdf_export_tests;
#[cfg(test)]
mod peer_relay_mint_tests;
#[cfg(test)]
mod peer_relay_tests;
#[cfg(test)]
mod plan_limit_tests;
#[cfg(test)]
mod rate_limit_tests;
#[cfg(test)]
mod relay_region_tests;
#[cfg(test)]
mod remote_control_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod room_visibility_tests;
#[cfg(test)]
mod stats_tests;
#[cfg(test)]
mod subscribe_tests;
#[cfg(test)]
mod tenant_archive_tests;
#[cfg(test)]
mod tunnel_tests;
#[cfg(test)]
mod tutorial_tests;
#[cfg(test)]
mod usage_ledger_tests;
#[cfg(test)]
mod ws_auth_tests;
