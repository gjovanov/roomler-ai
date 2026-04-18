//! One-shot enrollment exchange.
//!
//! Flow: admin issues an enrollment token in the Roomler UI and hands it to
//! the machine operator. `roomler-agent enroll --token <t>` posts it to
//! `POST /api/agent/enroll` with machine metadata, gets back a long-lived
//! agent token, and persists everything to the config file.

use anyhow::{Context, Result, bail};
use roomler_ai_remote_control::models::OsKind;
use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;

#[derive(Debug, Serialize)]
struct EnrollRequest<'a> {
    enrollment_token: &'a str,
    machine_id: &'a str,
    machine_name: &'a str,
    os: OsKind,
    agent_version: &'a str,
}

#[derive(Debug, Deserialize)]
struct EnrollResponse {
    agent_id: String,
    tenant_id: String,
    agent_token: String,
}

pub struct EnrollInputs<'a> {
    pub server_url: &'a str,
    pub enrollment_token: &'a str,
    pub machine_id: &'a str,
    pub machine_name: &'a str,
}

pub async fn enroll(inputs: EnrollInputs<'_>) -> Result<AgentConfig> {
    let url = format!("{}/api/agent/enroll", inputs.server_url.trim_end_matches('/'));
    let os = detect_os();
    let agent_version = env!("CARGO_PKG_VERSION");

    tracing::info!(%url, os = ?os, "posting enrollment");

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&EnrollRequest {
            enrollment_token: inputs.enrollment_token,
            machine_id: inputs.machine_id,
            machine_name: inputs.machine_name,
            os,
            agent_version,
        })
        .send()
        .await
        .context("POST /api/agent/enroll")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("enrollment rejected (status {status}): {body}");
    }

    let body: EnrollResponse = resp.json().await.context("parsing enroll response")?;

    Ok(AgentConfig {
        server_url: inputs.server_url.trim_end_matches('/').to_string(),
        ws_url: None,
        agent_token: body.agent_token,
        agent_id: body.agent_id,
        tenant_id: body.tenant_id,
        machine_id: inputs.machine_id.to_string(),
        machine_name: inputs.machine_name.to_string(),
        encoder_preference: crate::config::EncoderPreferenceChoice::default(),
    })
}

fn detect_os() -> OsKind {
    match std::env::consts::OS {
        "linux" => OsKind::Linux,
        "macos" => OsKind::Macos,
        "windows" => OsKind::Windows,
        other => {
            tracing::warn!(%other, "unknown OS, defaulting to Linux");
            OsKind::Linux
        }
    }
}
