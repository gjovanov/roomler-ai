// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 B1 — the daemon install on Linux and macOS (`cmd_install` with a
//! daemon [`Role`] off Windows): a GUI front end to the embedded
//! `install.sh` halves, under the desktop's own administrator prompt, with
//! the enrollment in this process. The mechanics are in
//! [`crate::unix_daemon`]; this is the order.
//!
//! **The order** (B0's finding, recorded in the spec):
//!
//! 1. preflight — the role's scope on this OS, what is installed, and the
//!    chosen server's `wizard_unix_daemon` switch, re-read now because the
//!    server is picked after the role;
//! 2. **system scope only:** enroll in-process, into a private 0600 file in
//!    the scratch directory. The person cannot read the root daemon's own
//!    config, so the fold into an existing one (#2338) happens in the next
//!    step, as root, through `roomlerd adopt-enrollment`;
//! 3. the **privileged half** — `--privileged-steps --role daemon --server …
//!    --name … --desktop`, plus `--system --config-from <file>` for the
//!    system scope — under ONE native prompt: `pkexec` on Linux, `osascript`
//!    with administrator privileges on macOS. The half's stdout and stderr
//!    stream into the progress channel. A dismissed prompt ends the install
//!    as *cancelled*; a prompt that cannot be shown turns into the terminal
//!    command (no secret on it) and a wait for the person to say it ran;
//! 4. **per-user scope:** enroll in-process now, as the person, folded into
//!    the config already at the path (#2338) — how Windows does it;
//! 5. `--user-steps --role daemon` (`--system` answers "no user half"), as
//!    the person: the systemd user unit, the LaunchAgent kickstart.
//!
//! **Secrets.** The token stays in this process: never in argv, never in a
//! URL, never in a line. The system-scope file holds the agent JWT and the
//! WireGuard key; it is shredded the moment the privileged half has placed
//! it, kept for a Retry when that half failed (the enrollment already
//! happened and the single-use token is spent — the B0 property), and
//! shredded at the wizard's exit at the latest.
//!
//! **Cancel.** Before the prompt is answered the prompt client is killed
//! and the dialog goes away. After it, the half runs as root and cannot be
//! stopped from here: the cancel is reported, the half is waited out, and
//! the pipeline then stops — a system-scope install may already be enrolled
//! and running at that point, and a Retry folds into it.

use std::ffi::OsString;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use roomler_ai_remote_control::setup_diagnostics::{ErrorCode, Step};
use tauri::ipc::Channel;
use wizard_shared::progress::{ProgressEvent, replay_log};

use crate::commands::DoneReport;
use crate::diagnostics::{Diagnostics, cancel_requested, enroll_error_code, failed};
use crate::orchestrator_agent::AdvancedOptions;
use crate::role::{HostOs, Role, Scope, scope_for};
use crate::unix_daemon::{self as ud, Half, HalfOutcome, KeptEnrollment, Scratch, Tool};

/// Drive a full daemon install on Linux or macOS. Streams ProgressEvent
/// over the channel; mirrors every emit into the replay log.
pub async fn run_install(
    role: Role,
    server: String,
    token: String,
    device_name: String,
    advanced: AdvancedOptions,
    on_event: Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    crate::INSTALL_IN_PROGRESS.store(true, Ordering::SeqCst);
    crate::CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    crate::ACTIVE_MSI_PID.store(0, Ordering::SeqCst);
    replay_log().reset();

    let diag = Diagnostics::global();
    diag.server_known(&crate::orchestrator_agent::diagnostics_server(&server));
    diag.pipeline_started();

    let outcome = run_install_inner(role, server, token, device_name, advanced, &on_event).await;

    crate::INSTALL_IN_PROGRESS.store(false, Ordering::SeqCst);

    match outcome {
        Ok(r) => Ok(r),
        Err(e) => {
            diag.abandon(ErrorCode::Other, cancel_requested());
            emit(
                &on_event,
                ProgressEvent::Error {
                    step: "install".to_string(),
                    message: e.clone(),
                },
            );
            Err(e)
        }
    }
}

/// The overlay box's default for a role on this OS when the person never
/// touched it: ON for a Linux system daemon (root, the TUN) and for a Linux
/// per-user daemon (the userspace netstack, which needs no privilege);
/// untouched — the agent's own default, off — on macOS, where the mesh
/// needs the root half (B2).
pub fn overlay_default(os: HostOs, scope: Scope) -> Option<bool> {
    match (os, scope) {
        (HostOs::Linux, _) => Some(true),
        _ => None,
    }
}

async fn run_install_inner(
    role: Role,
    server: String,
    token: String,
    device_name: String,
    advanced: AdvancedOptions,
    on_event: &Channel<ProgressEvent>,
) -> Result<DoneReport, String> {
    emit(on_event, ProgressEvent::Started);
    let diag = Diagnostics::global();
    let os = HostOs::current();

    // --- Step 1: preflight -----------------------------------------------
    diag.begin(Step::Preflight);
    check_cancel()?;
    advanced
        .validate()
        .map_err(|e| failed(ErrorCode::AdvancedOptions, e))?;
    let scope = scope_for(os, role).ok_or_else(|| {
        failed(
            ErrorCode::UnsupportedPlatform,
            format!("role {role:?} installs no daemon on {}", os.wire()),
        )
    })?;
    let tool = ud::elevation_tool(os).ok_or_else(|| {
        failed(
            ErrorCode::UnsupportedPlatform,
            format!("no administrator prompt is known for {}", os.wire()),
        )
    })?;
    emit(on_event, ProgressEvent::PreflightStarted);
    let detected = ud::detect(os);
    emit(
        on_event,
        ProgressEvent::PreflightOk {
            existing: label_existing(&detected),
        },
    );
    if let Some(message) = other_scope_warning(&detected, scope) {
        emit(on_event, ProgressEvent::PreflightWarning { message });
    }
    // The server's word, for the server the person chose (after the role).
    let features = crate::features::fetch(&server).await;
    if !features.wizard_unix_daemon {
        return Err(failed(
            ErrorCode::UnsupportedPlatform,
            format!(
                "{server} does not offer the daemon install from this wizard on {} (its setup \
                 features say so, or it is an older server). Install from a terminal with \
                 `curl -fsSL {}/api/setup/install.sh | sh -s -- --role daemon …`, pick the \
                 tunnel client, or choose another server.",
                os.wire(),
                server.trim_end_matches('/')
            ),
        ));
    }
    diag.ok();

    let config_path = match scope {
        Scope::System => ud::system_config_path(),
        Scope::PerUser => {
            ud::per_user_config_path().map_err(|e| failed(ErrorCode::ConfigWrite, e))?
        }
    };
    let install_info = || {
        let mut install = roomlerd::enrollment::InstallInfo::new(
            roomlerd::enrollment::InstallMethod::Wizard,
            match scope {
                Scope::System => roomlerd::enrollment::InstallScope::System,
                Scope::PerUser => roomlerd::enrollment::InstallScope::PerUser,
            },
        );
        install.run = diag.run_id();
        install
    };
    let mut kept = ud::take_kept(scope, &server, &device_name);

    // --- Step 2 (system): enroll in-process, into the scratch file -------
    let mut scratch: Option<Scratch> = None;
    let mut ids: Option<(String, String)> = None;
    let mut take: Vec<String> = Vec::new();
    if scope == Scope::System {
        diag.begin(Step::Enroll);
        check_cancel()?;
        emit(on_event, ProgressEvent::EnrollStarted);
        match kept.take() {
            Some(k)
                if k.scratch
                    .as_ref()
                    .is_some_and(|s| s.config_from().is_file()) =>
            {
                emit(
                    on_event,
                    ProgressEvent::PreflightWarning {
                        message: "reusing the enrollment from the previous attempt (its token was already spent)".to_string(),
                    },
                );
                emit_enrolled(on_event, &k.agent_id, &k.tenant_id);
                ids = Some((k.agent_id.clone(), k.tenant_id.clone()));
                take = advanced.touched_keys();
                scratch = k.scratch;
            }
            _ => {
                let s = Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?;
                // `existing: None` on purpose — root's config is unreadable
                // here; the privileged half folds into it (#2338).
                let crate::enroll_step::Enrolled { mut cfg, outcome } =
                    crate::enroll_step::enroll_folded(
                        &config_path,
                        None,
                        &server,
                        &token,
                        &device_name,
                        install_info(),
                    )
                    .await
                    .map_err(|e| failed(enroll_error_code(&e), format!("enrollment: {e}")))?;
                crate::enroll_step::finish(
                    &mut cfg,
                    &outcome,
                    false,
                    &advanced,
                    overlay_default(os, scope),
                )
                .map_err(|e| failed(ErrorCode::AdvancedOptions, e))?;
                roomlerd::config::save(&s.config_from(), &cfg).map_err(|e| {
                    failed(
                        ErrorCode::ConfigWrite,
                        format!("write the enrolled config: {e}"),
                    )
                })?;
                emit_enrolled(on_event, &cfg.agent_id, &cfg.tenant_id);
                ids = Some((cfg.agent_id.clone(), cfg.tenant_id.clone()));
                take = advanced.touched_keys();
                scratch = Some(s);
            }
        }
        diag.ok();
    }
    let scratch = match scratch {
        Some(s) => s,
        None => Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?,
    };
    let keep_system = |scratch: Scratch, ids: &Option<(String, String)>| {
        if let Some((agent_id, tenant_id)) = ids {
            ud::keep(KeptEnrollment {
                scope,
                server: server.clone(),
                device_name: device_name.clone(),
                agent_id: agent_id.clone(),
                tenant_id: tenant_id.clone(),
                config_path: config_path.clone(),
                scratch: Some(scratch),
            });
        }
    };

    // --- Step 3: the privileged half, under ONE native prompt ------------
    diag.begin(Step::Msi);
    if let Err(e) = check_cancel() {
        keep_system(scratch, &ids);
        return Err(e);
    }
    let config_from = (scope == Scope::System).then(|| scratch.config_from());
    let args = ud::privileged_args(&ud::PrivilegedSpec {
        script: &scratch.script,
        server: &server,
        device_name: &device_name,
        scope,
        config_from: config_from.as_deref(),
        take: &take,
    });
    if let Err(e) = run_privileged(tool, &args, os, scope, on_event).await {
        keep_system(scratch, &ids);
        return Err(e);
    }
    diag.ok();
    // Placed: the file has done its job.
    scratch.shred_config();

    // --- Step 4 (per-user): enroll in-process, as the person -------------
    if scope == Scope::PerUser {
        diag.begin(Step::Enroll);
        check_cancel()?;
        emit(on_event, ProgressEvent::EnrollStarted);
        let reusable = kept.take().filter(|k| {
            roomlerd::config::read_if_present(&config_path)
                .is_some_and(|cfg| cfg.agent_id == k.agent_id)
        });
        match reusable {
            Some(k) => {
                emit(
                    on_event,
                    ProgressEvent::PreflightWarning {
                        message: "reusing the enrollment from the previous attempt (its token was already spent)".to_string(),
                    },
                );
                emit_enrolled(on_event, &k.agent_id, &k.tenant_id);
                ids = Some((k.agent_id, k.tenant_id));
            }
            None => {
                let existing = roomlerd::config::read_if_present(&config_path);
                let crate::enroll_step::Enrolled { mut cfg, outcome } =
                    crate::enroll_step::enroll_folded(
                        &config_path,
                        existing,
                        &server,
                        &token,
                        &device_name,
                        install_info(),
                    )
                    .await
                    .map_err(|e| failed(enroll_error_code(&e), format!("enrollment: {e}")))?;
                let note = crate::enroll_step::finish(
                    &mut cfg,
                    &outcome,
                    false,
                    &advanced,
                    overlay_default(os, scope),
                )
                .map_err(|e| failed(ErrorCode::AdvancedOptions, e))?;
                if let Some(message) = note {
                    emit(on_event, ProgressEvent::PreflightWarning { message });
                }
                if let Some(message) = give_netstack_port(os, &mut cfg) {
                    emit(on_event, ProgressEvent::PreflightWarning { message });
                }
                roomlerd::config::save(&config_path, &cfg).map_err(|e| {
                    failed(ErrorCode::ConfigWrite, format!("write config.toml: {e}"))
                })?;
                emit_enrolled(on_event, &cfg.agent_id, &cfg.tenant_id);
                ids = Some((cfg.agent_id.clone(), cfg.tenant_id.clone()));
            }
        }
        diag.ok();
    }
    let (agent_id, tenant_id) = ids.ok_or_else(|| {
        failed(
            ErrorCode::Other,
            "no enrollment was made (a bug: the pipeline reached the user half without one)"
                .to_string(),
        )
    })?;
    let keep_placed = || {
        ud::keep(KeptEnrollment {
            scope,
            server: server.clone(),
            device_name: device_name.clone(),
            agent_id: agent_id.clone(),
            tenant_id: tenant_id.clone(),
            config_path: config_path.clone(),
            scratch: None,
        })
    };

    // --- Step 5: the user half, as the person ----------------------------
    if let Err(e) = check_cancel() {
        keep_placed();
        return Err(e);
    }
    emit(
        on_event,
        ProgressEvent::ScriptHalfStarted {
            half: Half::User.wire().to_string(),
            tool: Tool::Sh.wire().to_string(),
        },
    );
    let user_args = ud::user_args(&scratch.script, scope);
    let run = match ud::run_half(
        Tool::Sh.program(),
        &user_args,
        Half::User,
        on_event,
        ud::USER_BUDGET,
    )
    .await
    {
        Ok(run) => run,
        Err(e) => {
            keep_placed();
            return Err(failed(
                ErrorCode::Other,
                format!(
                    "the user steps could not start: {e}. The daemon is installed and enrolled; run `sh {} --user-steps --role daemon{}` yourself.",
                    scratch.script.display(),
                    if scope == Scope::System {
                        " --system"
                    } else {
                        ""
                    }
                ),
            ));
        }
    };
    match ud::classify(Tool::Sh, run.status, &run.stderr_tail, run.killed) {
        HalfOutcome::Exited(0) => emit(
            on_event,
            ProgressEvent::ScriptHalfDone {
                half: Half::User.wire().to_string(),
                code: 0,
            },
        ),
        HalfOutcome::Exited(code) => {
            emit(
                on_event,
                ProgressEvent::ScriptHalfDone {
                    half: Half::User.wire().to_string(),
                    code,
                },
            );
            keep_placed();
            check_cancel()?;
            return Err(failed(
                ErrorCode::Other,
                format!(
                    "the user steps failed (exit {code}): {}. The daemon is installed and enrolled; Retry runs the steps again without enrolling twice.",
                    last_line(&run.stderr_tail)
                ),
            ));
        }
        HalfOutcome::Cancelled | HalfOutcome::Unavailable(_) => {
            keep_placed();
            return Err(check_cancel().err().unwrap_or_else(|| {
                failed(ErrorCode::Other, "the user steps did not run".to_string())
            }));
        }
    }

    // --- Done ------------------------------------------------------------
    emit(on_event, ProgressEvent::Done);
    diag.done();
    ud::clear_kept();
    let binary = ud::daemon_binary(os);
    let tag = ud::installed_version(&binary)
        .map(|v| format!("agent-v{v}"))
        .unwrap_or_else(|| "(unknown)".to_string());
    let cli = ud::cli_binary(os).filter(|p| p.is_file());
    let cli_included = match os {
        HostOs::Linux => Some(cli.is_some()),
        _ => None,
    };
    drop(scratch);

    Ok(DoneReport {
        principal_kind: "agent".to_string(),
        principal_id: agent_id,
        tenant_id,
        tag,
        role,
        flavour: Some(scope.wire().to_string()),
        binary_path: cli.map(|p| p.display().to_string()),
        config_path: Some(config_path.display().to_string()),
        path_updated: None,
        shortcut_created: None,
        cli_included,
        desktop_installed: Some(ud::companion_present(os)),
        anchored: None,
    })
}

/// The privileged half under the prompt tool, with the terminal fallback.
async fn run_privileged(
    tool: Tool,
    args: &[OsString],
    os: HostOs,
    scope: Scope,
    on_event: &Channel<ProgressEvent>,
) -> Result<(), String> {
    let argv: Vec<OsString> = match tool {
        Tool::Osascript => ud::osascript_args(args),
        _ => args.to_vec(),
    };
    emit(
        on_event,
        ProgressEvent::ScriptHalfStarted {
            half: Half::Privileged.wire().to_string(),
            tool: tool.wire().to_string(),
        },
    );
    let outcome = match ud::run_half(
        tool.program(),
        &argv,
        Half::Privileged,
        on_event,
        ud::PRIVILEGED_BUDGET,
    )
    .await
    {
        Ok(run) => ud::classify(tool, run.status, &run.stderr_tail, run.killed),
        Err(e) => HalfOutcome::Unavailable(ud::spawn_failure(tool, &e)),
    };
    match outcome {
        HalfOutcome::Exited(0) => {
            emit(
                on_event,
                ProgressEvent::ScriptHalfDone {
                    half: Half::Privileged.wire().to_string(),
                    code: 0,
                },
            );
            Ok(())
        }
        HalfOutcome::Exited(code) => {
            emit(
                on_event,
                ProgressEvent::ScriptHalfDone {
                    half: Half::Privileged.wire().to_string(),
                    code,
                },
            );
            // A cancel that landed after the password: the half ran on to
            // its end, and the person's intent is what to report.
            check_cancel()?;
            Err(failed(
                ErrorCode::MsiFatal,
                format!(
                    "the privileged install steps failed (exit {code}) — see the lines above. Nothing \
                     was enrolled on this side that a Retry cannot reuse."
                ),
            ))
        }
        HalfOutcome::Cancelled => {
            emit(
                on_event,
                ProgressEvent::ElevationCancelled {
                    half: Half::Privileged.wire().to_string(),
                },
            );
            Diagnostics::global().cancelled();
            Err(
                "cancelled at the administrator prompt — nothing was installed. Try again and \
                 enter your password, or pick the tunnel client, which needs none."
                    .to_string(),
            )
        }
        HalfOutcome::Unavailable(reason) => {
            terminal_fallback(args, reason, os, scope, on_event).await
        }
    }
}

/// No prompt could be shown: the same half as a command the person runs in
/// a terminal (no secret on it), then a wait for their word, then a check
/// that the daemon is there.
async fn terminal_fallback(
    args: &[OsString],
    reason: String,
    os: HostOs,
    scope: Scope,
    on_event: &Channel<ProgressEvent>,
) -> Result<(), String> {
    let command = ud::terminal_command(args);
    tracing::warn!(%reason, "no administrator prompt; showing the terminal command");
    emit(
        on_event,
        ProgressEvent::ElevationUnavailable { command, reason },
    );
    emit(
        on_event,
        ProgressEvent::ScriptHalfStarted {
            half: Half::Privileged.wire().to_string(),
            tool: Tool::Terminal.wire().to_string(),
        },
    );
    let mut confirmed = ud::confirm_arm();
    let started = Instant::now();
    loop {
        tokio::select! {
            res = &mut confirmed => {
                if res.is_err() {
                    return Err(failed(ErrorCode::Other, "the terminal step was abandoned".to_string()));
                }
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                check_cancel()?;
                if started.elapsed() > ud::TERMINAL_BUDGET {
                    return Err(failed(
                        ErrorCode::Other,
                        format!(
                            "no confirmation within {} minutes that the terminal command ran",
                            ud::TERMINAL_BUDGET.as_secs() / 60
                        ),
                    ));
                }
            }
        }
    }
    // Their word, checked.
    let binary = ud::daemon_binary(os);
    if !binary.is_file() {
        return Err(failed(
            ErrorCode::Other,
            format!(
                "the daemon is not installed yet ({} is missing): run the command shown, then click Continue",
                binary.display()
            ),
        ));
    }
    if scope == Scope::System && !ud::system_config_path().is_file() {
        return Err(failed(
            ErrorCode::Other,
            format!(
                "the enrolled config was not placed ({} is missing): run the command shown, then click Continue",
                ud::SYSTEM_CONFIG
            ),
        ));
    }
    emit(
        on_event,
        ProgressEvent::ScriptHalfDone {
            half: Half::Privileged.wire().to_string(),
            code: 0,
        },
    );
    Ok(())
}

/// A Linux per-user daemon that joins the overlay needs the userspace
/// netstack's loopback port: it has no CAP_NET_ADMIN under its unit, and
/// without the port it joins nothing (#2493's lesson for secondaries,
/// applied to the primary). Picked as the secondaries' are; a band with no
/// free port is said, never silently skipped. A note for the person when
/// something is worth saying.
fn give_netstack_port(os: HostOs, cfg: &mut roomlerd::config::AgentConfig) -> Option<String> {
    if os != HostOs::Linux || !cfg.overlay_enabled || cfg.netstack_socks_port.is_some() {
        return None;
    }
    match roomlerd::org_join::pick_primary_netstack_port(cfg) {
        Some(port) => {
            if let Err(e) =
                roomlerd::config_surface::apply(cfg, "netstack_socks_port", Some(&port.to_string()))
            {
                return Some(format!(
                    "could not set the overlay's loopback port ({e}); the mesh will not come up until `netstack_socks_port` is set"
                ));
            }
            tracing::info!(port, "per-user Linux daemon: overlay through the netstack on 127.0.0.1");
            None
        }
        None => Some(
            "no free loopback port for the overlay's SOCKS5 front (10800–10899 all taken); the mesh will not come up until `netstack_socks_port` is set"
                .to_string(),
        ),
    }
}

/// The Welcome page's words for what is here.
pub fn label_existing(d: &ud::UnixDetect) -> String {
    let version = d.daemon_version.as_deref().unwrap_or("unknown version");
    match (d.daemon_installed, d.per_user_config, d.system_config) {
        (false, false, false) => "clean".to_string(),
        (false, _, _) => "an enrollment without the daemon (the package was removed)".to_string(),
        (true, true, true) => format!("daemon {version}, enrolled per-user AND as a system daemon"),
        (true, true, false) => format!("daemon {version}, enrolled per-user"),
        (true, false, true) => format!("daemon {version}, enrolled as a system daemon"),
        (true, false, false) => format!("daemon {version}, not enrolled"),
    }
}

/// Installing one scope beside an enrollment of the other: said, not
/// refused — the person may want exactly that, or may be about to run two
/// daemons on one machine without knowing it.
pub fn other_scope_warning(d: &ud::UnixDetect, scope: Scope) -> Option<String> {
    match scope {
        Scope::System if d.per_user_config => Some(
            "A per-user daemon is already enrolled on this machine. The system daemon installs beside it, as a second device; stop or uninstall the per-user one if you want only one."
                .to_string(),
        ),
        Scope::PerUser if d.system_config => Some(
            "A system (root) daemon is already enrolled on this machine. The per-user daemon installs beside it, as a second device; stop or uninstall the system one if you want only one."
                .to_string(),
        ),
        _ => None,
    }
}

fn emit_enrolled(on_event: &Channel<ProgressEvent>, agent_id: &str, tenant_id: &str) {
    emit(
        on_event,
        ProgressEvent::EnrollOk {
            principal_kind: "agent".into(),
            principal_id: agent_id.to_string(),
            tenant_id: tenant_id.to_string(),
        },
    );
}

fn last_line(text: &str) -> &str {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no output)")
}

fn emit(channel: &Channel<ProgressEvent>, event: ProgressEvent) {
    replay_log().push(event.clone());
    let _ = channel.send(event);
}

fn check_cancel() -> Result<(), String> {
    if crate::CANCEL_REQUESTED.load(Ordering::SeqCst) {
        Diagnostics::global().cancelled();
        Err("install cancelled by operator".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay box per OS and scope: on for both Linux scopes, untouched
    /// on macOS.
    #[test]
    fn overlay_default_per_os_and_scope() {
        assert_eq!(overlay_default(HostOs::Linux, Scope::System), Some(true));
        assert_eq!(overlay_default(HostOs::Linux, Scope::PerUser), Some(true));
        assert_eq!(overlay_default(HostOs::Macos, Scope::PerUser), None);
    }

    /// The preflight's words, and the warning for the other scope.
    #[test]
    fn existing_label_and_other_scope_warning() {
        let clean = ud::UnixDetect::default();
        assert_eq!(label_existing(&clean), "clean");
        assert!(other_scope_warning(&clean, Scope::System).is_none());
        let per_user = ud::UnixDetect {
            daemon_installed: true,
            daemon_version: Some("0.4.181".into()),
            per_user_config: true,
            system_config: false,
        };
        assert_eq!(
            label_existing(&per_user),
            "daemon 0.4.181, enrolled per-user"
        );
        assert!(
            other_scope_warning(&per_user, Scope::System)
                .unwrap()
                .contains("per-user")
        );
        assert!(other_scope_warning(&per_user, Scope::PerUser).is_none());
        let system = ud::UnixDetect {
            daemon_installed: true,
            daemon_version: None,
            per_user_config: false,
            system_config: true,
        };
        assert_eq!(
            label_existing(&system),
            "daemon unknown version, enrolled as a system daemon"
        );
        assert!(
            other_scope_warning(&system, Scope::PerUser)
                .unwrap()
                .contains("system")
        );
        assert!(other_scope_warning(&system, Scope::System).is_none());
    }

    /// A Linux per-user daemon with the overlay on gets a netstack port; one
    /// with the overlay off, or on macOS, or with a port already, is left
    /// alone.
    #[test]
    fn a_linux_per_user_overlay_gets_a_netstack_port() {
        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = true;
        cfg.netstack_socks_port = None;
        assert!(give_netstack_port(HostOs::Linux, &mut cfg).is_none());
        let port = cfg.netstack_socks_port.expect("a port");
        assert!((10800..=10899).contains(&port), "{port}");

        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = true;
        cfg.netstack_socks_port = Some(10850);
        give_netstack_port(HostOs::Linux, &mut cfg);
        assert_eq!(
            cfg.netstack_socks_port,
            Some(10850),
            "an existing port is kept"
        );

        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = false;
        cfg.netstack_socks_port = None;
        give_netstack_port(HostOs::Linux, &mut cfg);
        assert_eq!(cfg.netstack_socks_port, None, "no overlay, no port");

        let mut cfg = roomlerd::config::test_fixture();
        cfg.overlay_enabled = true;
        cfg.netstack_socks_port = None;
        give_netstack_port(HostOs::Macos, &mut cfg);
        assert_eq!(
            cfg.netstack_socks_port, None,
            "macOS: the mesh is the root half's"
        );
    }
}
