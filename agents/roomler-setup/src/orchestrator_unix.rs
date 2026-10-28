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
//!    step, as root, through `roomlerd adopt-enrollment`. The running root
//!    daemon is asked first which device it is ([`crate::root_identity`]),
//!    so a new device row (a renamed host, a cloned VM) is said here too;
//! 3. the **privileged half** — `--privileged-steps --role daemon --server …
//!    --name … --desktop`, plus `--system --config-from <file>` for the
//!    system scope — under ONE native prompt: `pkexec` on Linux, `osascript`
//!    with administrator privileges on macOS. The half's output streams into
//!    the progress channel. A dismissed prompt ends the install as
//!    *cancelled*; a prompt that cannot be shown turns into the terminal
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
//! **Cancel** (F1). Before the prompt is answered the prompt client is
//! killed, the dialog goes away, and *cancelled* means nothing ran. After it
//! the half runs as root and cannot be stopped from here: the cancel is
//! reported, the half waited out, and the install stops after it, saying
//! what the half did. A Retry then picks up where it stopped (F5): a half
//! that ran to its end is not run again — no second password, no second
//! enrollment with a token already spent.

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

/// F5 — what an earlier attempt with the same server, name and scope left,
/// and what this attempt can therefore skip.
#[derive(Debug)]
struct Resume {
    /// The device the server holds for this machine, `(agent, tenant)`.
    ids: Option<(String, String)>,
    /// System scope: the enrolled config the privileged half never placed.
    scratch: Option<Scratch>,
    /// The privileged half ran to its end and its work is still in place:
    /// it is not run again.
    privileged_done: bool,
}

/// Is a finished privileged half's work still in place? The daemon binary,
/// and for a system scope the placed config.
fn privileged_work_in_place(os: HostOs, scope: Scope) -> bool {
    ud::daemon_binary(os).is_file()
        && (scope == Scope::PerUser || ud::system_config_path().is_file())
}

/// F5 — read what was kept, against what is on disk now (`in_place`:
/// [`privileged_work_in_place`]).
fn resume_from(
    kept: Option<KeptEnrollment>,
    scope: Scope,
    in_place: bool,
) -> (Resume, Option<String>) {
    let Some(k) = kept else {
        return (
            Resume {
                ids: None,
                scratch: None,
                privileged_done: false,
            },
            None,
        );
    };
    if k.privileged_done && !in_place {
        // Removed in between: run it all again. A token the earlier attempt
        // spent cannot enroll again — say so before the server does.
        let note = (scope == Scope::System && k.ids.is_some()).then(|| {
            "The previous attempt's install is no longer in place, so it runs again from the start. Its enrollment token was already used: if the enroll is refused, get a new token or pair again.".to_string()
        });
        return (
            Resume {
                ids: None,
                scratch: None,
                privileged_done: false,
            },
            note,
        );
    }
    (
        Resume {
            ids: k.ids,
            scratch: k.scratch,
            privileged_done: k.privileged_done,
        },
        None,
    )
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
        warn(on_event, message);
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
    // F5 — an earlier attempt's work, for the same server, name and scope.
    let (resume, resume_note) = resume_from(
        ud::take_kept(scope, &server, &device_name),
        scope,
        privileged_work_in_place(os, scope),
    );
    if let Some(message) = resume_note {
        warn(on_event, message);
    }
    let skip_privileged = resume.privileged_done;
    if skip_privileged {
        warn(
            on_event,
            match scope {
                Scope::System => "Picking up the previous attempt: its administrator steps already ran to the end (the package is installed and this device enrolled, its service running), so neither the password nor the token is asked for again.",
                Scope::PerUser => "Picking up the previous attempt: its administrator steps already ran to the end (the package is installed), so the password is not asked for again.",
            }
            .to_string(),
        );
    }
    let mut ids = resume.ids.clone();
    let kept_ids = resume.ids;
    let kept_scratch = resume.scratch;

    // --- Step 2 (system): enroll in-process, into the scratch file -------
    let mut scratch: Option<Scratch> = None;
    let mut take: Vec<String> = Vec::new();
    if scope == Scope::System && !skip_privileged {
        diag.begin(Step::Enroll);
        check_cancel()?;
        emit(on_event, ProgressEvent::EnrollStarted);
        match (kept_scratch, kept_ids.clone()) {
            (Some(s), Some((agent_id, tenant_id))) if s.config_from().is_file() => {
                warn(
                    on_event,
                    "reusing the enrollment from the previous attempt (its token was already spent)".to_string(),
                );
                emit_enrolled(on_event, &agent_id, &tenant_id);
                ids = Some((agent_id, tenant_id));
                take = advanced.touched_keys();
                scratch = Some(s);
            }
            _ => {
                let s = Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?;
                // F3 — which device the root daemon here is, before the
                // enroll makes one (it may make ANOTHER: see below).
                let root = if ud::system_config_path().is_file() {
                    crate::root_identity::query().await
                } else {
                    None
                };
                // `existing: None` on purpose — root's config is unreadable
                // here; the privileged half folds into it (#2338).
                let crate::enroll_step::Enrolled {
                    mut cfg, outcome, ..
                } = crate::enroll_step::enroll_folded(
                    &config_path,
                    None,
                    &server,
                    &token,
                    &device_name,
                    install_info(),
                )
                .await
                .map_err(|e| failed(enroll_error_code(&e), enroll_refusal(&e)))?;
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
                if let Some(message) = root
                    .as_ref()
                    .and_then(|r| r.new_row_warning(&cfg.agent_id, &cfg.tenant_id))
                {
                    // The privileged half retires the key the previous row
                    // still claims (`adopt-enrollment`); this says so first.
                    warn(on_event, message);
                }
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
    // What a Retry finds when this attempt stops before the end.
    let keep_unplaced = |scratch: Scratch, ids: &Option<(String, String)>| {
        if ids.is_some() {
            ud::keep(KeptEnrollment {
                scope,
                server: server.clone(),
                device_name: device_name.clone(),
                ids: ids.clone(),
                config_path: config_path.clone(),
                scratch: Some(scratch),
                privileged_done: false,
            });
        }
    };
    let keep_after_privileged = |ids: &Option<(String, String)>| {
        ud::keep(KeptEnrollment {
            scope,
            server: server.clone(),
            device_name: device_name.clone(),
            ids: ids.clone(),
            config_path: config_path.clone(),
            scratch: None,
            privileged_done: true,
        });
    };

    // --- Step 3: the privileged half, under ONE native prompt ------------
    if !skip_privileged {
        diag.begin(Step::Msi);
        if let Err(e) = check_cancel() {
            keep_unplaced(scratch, &ids);
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
        let done = {
            // The window refuses to close while this runs: the wizard's exit
            // would remove the scratch directory from under it.
            let _running = ud::PrivilegedRunning::begin();
            run_privileged(tool, &args, &scratch, os, scope, on_event).await
        };
        let done = match done {
            Ok(done) => done,
            Err(e) => {
                keep_unplaced(scratch, &ids);
                return Err(e);
            }
        };
        diag.ok();
        // Placed: the file has done its job.
        scratch.shred_config();
        // F1 — a cancel that came while the half ran (or the moment after):
        // it ran to its end, and the install stops here, saying so.
        if done.cancelled_late || cancel_requested() {
            keep_after_privileged(&ids);
            Diagnostics::global().cancelled();
            return Err(cancelled_after_privileged(scope));
        }
    }

    // --- Step 4 (per-user): enroll in-process, as the person -------------
    if scope == Scope::PerUser {
        diag.begin(Step::Enroll);
        if let Err(e) = check_cancel() {
            keep_after_privileged(&ids);
            return Err(e);
        }
        emit(on_event, ProgressEvent::EnrollStarted);
        // F4 — a config there that does not load is said, recovered from its
        // last good copy when it can be, and kept aside.
        let existing = crate::enroll_step::read_existing(&config_path);
        if let Some(message) = existing.warning.clone() {
            warn(on_event, message);
        }
        let reusable = kept_ids.filter(|(agent_id, _)| {
            existing
                .cfg
                .as_ref()
                .is_some_and(|cfg| &cfg.agent_id == agent_id)
        });
        match reusable {
            Some((agent_id, tenant_id)) => {
                warn(
                    on_event,
                    "reusing the enrollment from the previous attempt (its token was already spent)".to_string(),
                );
                emit_enrolled(on_event, &agent_id, &tenant_id);
                ids = Some((agent_id, tenant_id));
            }
            None => {
                let enrolled = crate::enroll_step::enroll_folded(
                    &config_path,
                    existing.cfg,
                    &server,
                    &token,
                    &device_name,
                    install_info(),
                )
                .await;
                let crate::enroll_step::Enrolled {
                    mut cfg,
                    outcome,
                    previous_row,
                } = match enrolled {
                    Ok(e) => e,
                    Err(e) => {
                        keep_after_privileged(&ids);
                        return Err(failed(enroll_error_code(&e), enroll_refusal(&e)));
                    }
                };
                let note = crate::enroll_step::finish(
                    &mut cfg,
                    &outcome,
                    false,
                    &advanced,
                    overlay_default(os, scope),
                )
                .map_err(|e| failed(ErrorCode::AdvancedOptions, e))?;
                if let Some(message) = note {
                    warn(on_event, message);
                }
                if let Some(row) = &previous_row {
                    warn(
                        on_event,
                        roomlerd::enrollment::describe_previous_row(row, &cfg.agent_id),
                    );
                }
                if let Some(message) = give_netstack_port(os, &mut cfg) {
                    warn(on_event, message);
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
    let (agent_id, tenant_id) = ids.clone().ok_or_else(|| {
        failed(
            ErrorCode::Other,
            "no enrollment was made (a bug: the pipeline reached the user half without one)"
                .to_string(),
        )
    })?;

    // --- Step 5: the user half, as the person ----------------------------
    if let Err(e) = check_cancel() {
        keep_after_privileged(&ids);
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
        &crate::CANCEL_REQUESTED,
    )
    .await
    {
        Ok(run) => run,
        Err(e) => {
            keep_after_privileged(&ids);
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
    match ud::classify(Tool::Sh, &run) {
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
            keep_after_privileged(&ids);
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
            keep_after_privileged(&ids);
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

/// F1 — the install stopped by a cancel that came after the administrator
/// steps had started: they ran to the end, and this says what that left.
pub fn cancelled_after_privileged(scope: Scope) -> String {
    match scope {
        Scope::System => "Cancelled after the administrator steps had started. A step running as root cannot be stopped from here, so they ran to the end: the package is installed and this device is enrolled, its service running. Nothing else was done. Retry finishes the install without asking for the password or a token again.".to_string(),
        Scope::PerUser => "Cancelled after the administrator steps had started. A step running as root cannot be stopped from here, so they ran to the end: the package is installed, but this device is not enrolled yet. Retry finishes the install without asking for the password again.".to_string(),
    }
}

/// The enroll's refusal, for the person: a token the server already took
/// (a Retry of an attempt that spent it, a token used twice) needs a new one.
fn enroll_refusal(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    if text.contains("already been used") {
        format!(
            "enrollment: {text}. An enrollment token works once: get a new one (or pair again) and Retry."
        )
    } else {
        format!("enrollment: {text}")
    }
}

/// What the privileged half did, when it did not fail.
#[derive(Debug)]
struct PrivilegedDone {
    /// A cancel came after the half had started: it ran to its end anyway.
    cancelled_late: bool,
}

/// The privileged half under the prompt tool, with the terminal fallback.
async fn run_privileged(
    tool: Tool,
    args: &[OsString],
    scratch: &Scratch,
    os: HostOs,
    scope: Scope,
    on_event: &Channel<ProgressEvent>,
) -> Result<PrivilegedDone, String> {
    let files = scratch.half_files();
    let cmd = ud::prompt_command(tool, &files, args);
    emit(
        on_event,
        ProgressEvent::ScriptHalfStarted {
            half: Half::Privileged.wire().to_string(),
            tool: tool.wire().to_string(),
        },
    );
    let (outcome, cancelled_late) = match ud::run_privileged_half(
        &cmd,
        &files,
        on_event,
        ud::PRIVILEGED_BUDGET,
        &crate::CANCEL_REQUESTED,
    )
    .await
    {
        Ok(run) => (ud::classify(tool, &run), run.cancelled_late),
        Err(e) => (HalfOutcome::Unavailable(ud::spawn_failure(tool, &e)), false),
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
            Ok(PrivilegedDone { cancelled_late })
        }
        HalfOutcome::Exited(code) => {
            emit(
                on_event,
                ProgressEvent::ScriptHalfDone {
                    half: Half::Privileged.wire().to_string(),
                    code,
                },
            );
            // `failed` records a cancel when the flag is set; the message
            // says both, since the half ran on and failed regardless.
            let message = if cancelled_late || cancel_requested() {
                format!(
                    "Cancelled while the administrator steps were running — a step running as root cannot be stopped from here — and they failed (exit {code}): see the lines above. Retry runs them again{}.",
                    if scope == Scope::System {
                        " with the same enrollment"
                    } else {
                        ""
                    }
                )
            } else {
                format!(
                    "the privileged install steps failed (exit {code}) — see the lines above. Nothing was enrolled on this side that a Retry cannot reuse."
                )
            };
            Err(failed(ErrorCode::MsiFatal, message))
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
        HalfOutcome::Unavailable(reason) => terminal_fallback(args, reason, os, scope, on_event)
            .await
            .map(|()| PrivilegedDone {
                cancelled_late: false,
            }),
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

fn warn(on_event: &Channel<ProgressEvent>, message: String) {
    emit(on_event, ProgressEvent::PreflightWarning { message });
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

    fn kept(scope: Scope, privileged_done: bool, scratch: Option<Scratch>) -> KeptEnrollment {
        KeptEnrollment {
            scope,
            server: "https://example.invalid".into(),
            device_name: "lab".into(),
            ids: Some(("aid".into(), "tid".into())),
            config_path: std::path::PathBuf::from(ud::SYSTEM_CONFIG),
            scratch,
            privileged_done,
        }
    }

    /// F5 — a system-scope attempt cancelled after its privileged half ran
    /// to the end kept the placed enrollment with no scratch file. Before
    /// the fix the Retry fell through to "enroll again" and posted a token
    /// already spent; now it reads as a half that need not run again — as
    /// long as its work is still in place — and anything else is said.
    #[test]
    fn a_retry_picks_up_after_a_finished_privileged_half() {
        // The work still in place: the half is not run again, and its
        // enrollment carries the run.
        let (resume, note) =
            resume_from(Some(kept(Scope::System, true, None)), Scope::System, true);
        assert!(resume.privileged_done, "not run again");
        assert_eq!(resume.ids, Some(("aid".into(), "tid".into())));
        assert!(note.is_none());

        // The work gone since (removed in between): it all runs again, and the
        // person is told the spent token may be refused.
        let (resume, note) =
            resume_from(Some(kept(Scope::System, true, None)), Scope::System, false);
        assert!(!resume.privileged_done);
        assert!(resume.ids.is_none());
        let note = note.expect("said");
        assert!(
            note.contains("already used") && note.contains("new token"),
            "{note}"
        );

        // An attempt whose half never finished keeps its unplaced config for
        // the half to place.
        let scratch = Scratch::create().unwrap();
        std::fs::write(scratch.config_from(), "agent_id = \"aid\"").unwrap();
        let (resume, note) = resume_from(
            Some(kept(Scope::System, false, Some(scratch))),
            Scope::System,
            false,
        );
        assert!(!resume.privileged_done);
        assert!(note.is_none());
        assert!(resume.scratch.expect("kept").config_from().is_file());
        assert_eq!(resume.ids, Some(("aid".into(), "tid".into())));

        // Nothing kept: nothing to skip.
        let (resume, note) = resume_from(None, Scope::System, true);
        assert!(!resume.privileged_done && resume.ids.is_none() && note.is_none());
    }

    /// F1 — what a cancel after the start reports: never "nothing was
    /// installed"; the state the half left, per scope, and that a Retry does
    /// not ask again.
    #[test]
    fn a_late_cancel_says_what_the_half_left() {
        let system = cancelled_after_privileged(Scope::System);
        assert!(system.contains("ran to the end"), "{system}");
        assert!(system.contains("enrolled"), "{system}");
        assert!(!system.contains("nothing was installed"), "{system}");
        assert!(system.contains("password or a token"), "{system}");
        let per_user = cancelled_after_privileged(Scope::PerUser);
        assert!(per_user.contains("not enrolled yet"), "{per_user}");
        assert!(!per_user.contains("nothing was installed"), "{per_user}");
    }

    /// F5 — a spent token's refusal names the remedy.
    #[test]
    fn a_spent_token_names_the_remedy() {
        let e = anyhow::anyhow!(
            "enrollment rejected (status 401 Unauthorized): This enrollment token has already been used"
        );
        let said = enroll_refusal(&e);
        assert!(
            said.contains("works once") && said.contains("pair again"),
            "{said}"
        );
        let other = enroll_refusal(&anyhow::anyhow!("connection refused"));
        assert_eq!(other, "enrollment: connection refused");
    }
}
