pub mod fixtures;

#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod channel_crud_tests;
#[cfg(test)]
mod message_tests;
#[cfg(test)]
mod multi_tenancy_tests;
#[cfg(test)]
mod reaction_tests;
#[cfg(test)]
mod conference_tests;
#[cfg(test)]
mod conference_message_tests;
#[cfg(test)]
mod file_tests;
#[cfg(test)]
mod export_tests;
#[cfg(test)]
mod recording_tests;

#[cfg(test)]
mod pdf_export_tests;
#[cfg(test)]
mod invite_tests;
#[cfg(test)]
mod member_tests;
#[cfg(test)]
mod oauth_tests;
#[cfg(test)]
mod notification_tests;
#[cfg(test)]
mod rate_limit_tests;
#[cfg(test)]
mod pagination_tests;
#[cfg(test)]
mod role_tests;
#[cfg(test)]
mod cors_tests;
