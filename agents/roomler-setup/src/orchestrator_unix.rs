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
//!    chosen server's daemon switch for this OS (`wizard_linux_daemon` or
//!    `wizard_macos_daemon`, or an older server's `wizard_unix_daemon`;
//!    [`crate::features::SetupFeatures::daemon_offered`]), re-read now
//!    because the server is picked after the role;
//! 2. **a root-owned config, when there is one:** enroll in-process, into a
//!    private 0600 file in the scratch directory. That is the Linux system
//!    scope's config, and — FR-129 B2 — the macOS **root half**'s, the root
//!    LaunchDaemon that owns the overlay's utun: a device of its own (FR-43:
//!    the hub keys sessions on `agent_id`), enrolled with the second token of
//!    the same approval under a machine id taken from the hardware
//!    (`machine::root_half_machine_id`). The person cannot read root's
//!    config, so the fold into an existing one (#2338) happens in the next
//!    step, as root, through `roomlerd adopt-enrollment`. The running root
//!    daemon is asked first which device it is ([`crate::root_identity`]),
//!    so a new device row (a renamed host, a cloned VM) is said here too;
//! 3. the **privileged half** — `--privileged-steps --role daemon --server …
//!    --name … --desktop`, plus `--config-from <file>` for that root-owned
//!    config (and `--system` for the Linux system scope; on macOS the flag
//!    also requests the root half) — under ONE native prompt: `pkexec` on
//!    Linux, `osascript` with administrator privileges on macOS. The half's
//!    output streams into the progress channel. A dismissed prompt ends the
//!    install as *cancelled*; a prompt that cannot be shown turns into the
//!    terminal command (no secret on it) and a wait for the person to say it
//!    ran;
//! 4. **per-user scope:** enroll in-process now, as the person, folded into
//!    the config already at the path (#2338) — how Windows does it;
//! 5. `--user-steps --role daemon` (`--system` answers "no user half"), as
//!    the person: the systemd user unit, the LaunchAgent kickstart.
//!
//! **Secrets.** The tokens stay in this process: never in argv, never in a
//! URL, never in a line. The root-owned config file holds the agent JWT and
//! the WireGuard key; it is shredded the moment the privileged half has
//! placed it, kept for a Retry when that half failed (the enrollment already
//! happened and the single-use token is spent — the B0 property), and
//! shredded at the wizard's exit at the latest.
//!
//! **Cancel** (F1). Before the prompt is answered the prompt client is
//! killed, the dialog goes away, and *cancelled* means nothing ran. After it
//! the half runs as root and cannot be stopped from here: the cancel is
//! reported, the half waited out, and the install stops after it, saying
//! what the half did. A Retry then picks up where it stopped (F5): a half
//! that ran to its end is not run again — no second password, no second
//! enrollment with a token already spent, for either of a Mac's two halves.

use std::ffi::OsString;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use roomler_ai_remote_control::setup_diagnostics::{ErrorCode, Step};
use tauri::ipc::Channel;
use wizard_shared::progress::{ProgressEvent, replay_log};

use crate::commands::{DoneReport, RootHalfReport};
use crate::diagnostics::{Diagnostics, cancel_requested, enroll_error_code, failed};
use crate::orchestrator_agent::AdvancedOptions;
use crate::role::{HostOs, Role, Scope, scope_for};
use crate::unix_daemon::{self as ud, Half, HalfOutcome, KeptEnrollment, Scratch, Tool};

/// The `EnrollOk` kind of the macOS root half's enrollment: a device of its
/// own beside the person's (`agent`), shown on its own row.
pub const ROOT_HALF_PRINCIPAL_KIND: &str = "agent_root_half";

/// Drive a full daemon install on Linux or macOS. Streams ProgressEvent
/// over the channel; mirrors every emit into the replay log.
///
/// `root_token` (FR-129 B2): the macOS root half's own enrollment token —
/// the pairing's second, or the second one pasted — when this install adds
/// the root half beside the person's; `None` otherwise.
pub async fn run_install(
    role: Role,
    server: String,
    token: String,
    root_token: Option<String>,
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

    let outcome = run_install_inner(
        role,
        server,
        token,
        root_token,
        device_name,
        advanced,
        &on_event,
    )
    .await;

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
/// untouched — the agent's own default, off — for the macOS per-user half,
/// whose mesh is the root half's (FR-129 B2).
pub fn overlay_default(os: HostOs, scope: Scope) -> Option<bool> {
    match (os, scope) {
        (HostOs::Linux, _) => Some(true),
        _ => None,
    }
}

/// FR-129 B2 — the Advanced keys that belong to the overlay, and so on macOS
/// to the root half: the mesh switch and the overlay's own subnet routes.
/// The rest (the tunnel mesh's subnets and routes, session consent) stay the
/// person's device's, as B1 left them.
pub const ROOT_HALF_KEYS: [&str; 2] = ["overlay_enabled", "overlay_advertised_routes"];

/// FR-129 B2 — the Advanced options for the device the role installs. On
/// macOS that is the per-user half, whose mesh is the root half's: the
/// overlay's keys never land on it (its `overlay_enabled` stays as B1 left
/// it), whether or not this install adds the root half. Elsewhere they are
/// all its own.
pub fn device_options(os: HostOs, advanced: &AdvancedOptions) -> AdvancedOptions {
    if os != HostOs::Macos {
        return advanced.clone();
    }
    AdvancedOptions {
        overlay_enabled: None,
        overlay_advertised_routes: None,
        touched: advanced
            .touched
            .iter()
            .filter(|k| !ROOT_HALF_KEYS.contains(&k.trim()))
            .cloned()
            .collect(),
        ..advanced.clone()
    }
}

/// FR-129 B2 — the Advanced options for the macOS root half: the overlay ON
/// (the one reason the half exists — what `--overlay` writes on a terminal)
/// and the overlay's subnet routes as the person gave them; nothing of the
/// person's device's, which keep the agent's defaults there, as the
/// terminal's root half does. A touched key is taken onto an existing root
/// config too (`--config-take`), an untouched one only onto a fresh one.
pub fn root_half_options(advanced: &AdvancedOptions) -> AdvancedOptions {
    AdvancedOptions {
        overlay_enabled: Some(true),
        overlay_advertised_routes: advanced.overlay_advertised_routes.clone(),
        advertise_local_subnets: None,
        auto_grant_session: None,
        advertise_routes: None,
        touched: advanced
            .touched
            .iter()
            .filter(|k| ROOT_HALF_KEYS.contains(&k.trim()))
            .cloned()
            .collect(),
    }
}

/// F5 — what an earlier attempt with the same server, name and scope left,
/// and what this attempt can therefore skip.
#[derive(Debug, Default)]
struct Resume {
    /// The device the server holds for this machine, `(agent, tenant)`: the
    /// system scope's, or the person's.
    ids: Option<(String, String)>,
    /// FR-129 B2: the macOS root half's device.
    root_half_ids: Option<(String, String)>,
    /// The root-owned config (system scope, root half) the privileged half
    /// never placed.
    scratch: Option<Scratch>,
    /// The privileged half ran to its end and its work is still in place:
    /// it is not run again.
    privileged_done: bool,
}

/// Is a finished privileged half's work still in place? The daemon binary,
/// and for a system scope or a root half the placed config.
fn privileged_work_in_place(os: HostOs, scope: Scope, root_half: bool) -> bool {
    ud::daemon_binary(os).is_file()
        && ((scope == Scope::PerUser && !root_half) || ud::system_config_path().is_file())
}

/// F5 — read what was kept, against what is on disk now (`in_place`:
/// [`privileged_work_in_place`]).
fn resume_from(
    kept: Option<KeptEnrollment>,
    scope: Scope,
    in_place: bool,
) -> (Resume, Option<String>) {
    let Some(k) = kept else {
        return (Resume::default(), None);
    };
    if k.privileged_done && !in_place {
        // Removed in between: run it all again. A token the earlier attempt
        // spent on a root-owned config cannot enroll again — say so before
        // the server does. The person's own enrollment is another matter: it
        // lives in the person's config, and the per-user step reuses it
        // whenever that config still holds its device.
        let spent = (scope == Scope::System && k.ids.is_some()) || k.root_half_ids.is_some();
        let note = spent.then(|| {
            "The previous attempt's install is no longer in place, so it runs again from the start. Its root service's enrollment token was already used: if that enroll is refused, get a new token or pair again.".to_string()
        });
        return (
            Resume {
                ids: if scope == Scope::PerUser { k.ids } else { None },
                ..Resume::default()
            },
            note,
        );
    }
    (
        Resume {
            ids: k.ids,
            root_half_ids: k.root_half_ids,
            scratch: k.scratch,
            privileged_done: k.privileged_done,
        },
        None,
    )
}

/// F5 — is the device a kept enrollment names still in this config? It is the
/// primary's or, for an org the attempt added beside the primary, that org
/// entry's ([`roomlerd::enrollment::enrolled_identity`]).
fn enrollment_in(cfg: Option<&roomlerd::config::AgentConfig>, agent_id: &str) -> bool {
    cfg.is_some_and(|cfg| {
        cfg.agent_id == agent_id || cfg.orgs.iter().any(|o| o.agent_id == agent_id)
    })
}

/// What this attempt has enrolled so far, for what a Retry finds.
struct Made {
    /// The system scope's device, or the person's.
    ids: Option<(String, String)>,
    /// FR-129 B2: the macOS root half's.
    root_half_ids: Option<(String, String)>,
}

#[allow(clippy::too_many_arguments)]
async fn run_install_inner(
    role: Role,
    server: String,
    token: String,
    root_token: Option<String>,
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
    let root_half = root_token.is_some();
    if root_half && !(os == HostOs::Macos && scope == Scope::PerUser) {
        return Err(failed(
            ErrorCode::UnsupportedPlatform,
            format!(
                "the root half is a macOS daemon install's; this is {} ({})",
                os.wire(),
                scope.wire()
            ),
        ));
    }
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
            existing: label_existing(&detected, os),
        },
    );
    if let Some(message) = other_scope_warning(&detected, scope, os) {
        warn(on_event, message);
    }
    if root_half {
        warn(on_event, two_devices_note(&device_name));
    }
    // The server's word for this OS, for the server the person chose (after
    // the role).
    let features = crate::features::fetch(&server).await;
    if !features.daemon_offered(os) {
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
    let install_info = |scope: Scope| {
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
        ud::take_kept(scope, &server, &device_name, root_half),
        scope,
        privileged_work_in_place(os, scope, root_half),
    );
    if let Some(message) = resume_note {
        warn(on_event, message);
    }
    let skip_privileged = resume.privileged_done;
    if skip_privileged {
        warn(
            on_event,
            match (scope, root_half) {
                (Scope::System, _) => "Picking up the previous attempt: its administrator steps already ran to the end (the package is installed and this device enrolled, its service running), so neither the password nor the token is asked for again.",
                (Scope::PerUser, true) => "Picking up the previous attempt: its administrator steps already ran to the end (the package is installed and the private network's root half enrolled, its service running), so neither the password nor that half's token is asked for again.",
                (Scope::PerUser, false) => "Picking up the previous attempt: its administrator steps already ran to the end (the package is installed), so the password is not asked for again.",
            }
            .to_string(),
        );
    }
    let mut made = Made {
        ids: resume.ids.clone(),
        root_half_ids: resume.root_half_ids.clone(),
    };
    let kept_ids = resume.ids;
    let kept_scratch = resume.scratch;
    // What a Retry finds when this attempt stops before the end.
    let keep_unplaced = |scratch: Scratch, made: &Made| {
        if made.ids.is_some() || made.root_half_ids.is_some() {
            ud::keep(KeptEnrollment {
                scope,
                server: server.clone(),
                device_name: device_name.clone(),
                root_half,
                ids: made.ids.clone(),
                root_half_ids: made.root_half_ids.clone(),
                config_path: config_path.clone(),
                scratch: Some(scratch),
                privileged_done: false,
            });
        }
    };
    let keep_after_privileged = |made: &Made| {
        ud::keep(KeptEnrollment {
            scope,
            server: server.clone(),
            device_name: device_name.clone(),
            root_half,
            ids: made.ids.clone(),
            root_half_ids: made.root_half_ids.clone(),
            config_path: config_path.clone(),
            scratch: None,
            privileged_done: true,
        });
    };

    // --- Step 2: a root-owned config — enroll in-process, into the scratch
    //     file: the Linux system scope's, or the macOS root half's ---------
    let mut scratch: Option<Scratch> = None;
    let mut take: Vec<String> = Vec::new();
    if (scope == Scope::System || root_half) && !skip_privileged {
        diag.begin(Step::Enroll);
        if let Err(e) = check_cancel() {
            // A kept enrollment goes back for the next attempt: dropping its
            // scratch would shred a config whose token is already spent.
            if let Some(s) = kept_scratch {
                keep_unplaced(s, &made);
            }
            return Err(e);
        }
        emit(on_event, ProgressEvent::EnrollStarted);
        // The kept enrollment for THIS config: the system device's, or the
        // root half's.
        let kept_root_ids = if root_half {
            made.root_half_ids.clone()
        } else {
            kept_ids.clone()
        };
        let root_options = if root_half {
            root_half_options(&advanced)
        } else {
            advanced.clone()
        };
        let enrolled = match (kept_scratch, kept_root_ids) {
            (Some(s), Some((agent_id, tenant_id))) if s.config_from().is_file() => {
                warn(
                    on_event,
                    if root_half {
                        "reusing the private network's enrollment from the previous attempt (its token was already spent)"
                    } else {
                        "reusing the enrollment from the previous attempt (its token was already spent)"
                    }
                    .to_string(),
                );
                (s, (agent_id, tenant_id))
            }
            _ => {
                let (s, ids) = if root_half {
                    enroll_root_half(
                        &server,
                        root_token.as_deref().unwrap_or_default(),
                        &device_name,
                        install_info(Scope::System),
                        &root_options,
                        on_event,
                    )
                    .await?
                } else {
                    enroll_system(
                        &server,
                        &token,
                        &device_name,
                        &config_path,
                        install_info(Scope::System),
                        &root_options,
                        overlay_default(os, scope),
                        on_event,
                    )
                    .await?
                };
                (s, ids)
            }
        };
        let (s, ids) = enrolled;
        if root_half {
            emit_enrolled(on_event, ROOT_HALF_PRINCIPAL_KIND, &ids.0, &ids.1);
            made.root_half_ids = Some(ids);
        } else {
            emit_enrolled(on_event, "agent", &ids.0, &ids.1);
            made.ids = Some(ids);
        }
        take = root_options.touched_keys();
        scratch = Some(s);
        diag.ok();
    }
    let has_root_config = scratch.is_some();
    let scratch = match scratch {
        Some(s) => s,
        None => Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?,
    };

    // --- Step 3: the privileged half, under ONE native prompt ------------
    if !skip_privileged {
        diag.begin(Step::Msi);
        if let Err(e) = check_cancel() {
            keep_unplaced(scratch, &made);
            return Err(e);
        }
        let config_from = has_root_config.then(|| scratch.config_from());
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
            run_privileged(tool, &args, &scratch, os, scope, root_half, on_event).await
        };
        let done = match done {
            Ok(done) => done,
            Err(e) => {
                keep_unplaced(scratch, &made);
                return Err(e);
            }
        };
        // FR-129 B2 — the half's success is not the config's placement: on
        // macOS the script skips the root half with only a warning when the
        // package installed no LaunchDaemon. A root-owned config this attempt
        // enrolled must be where its daemon reads it, or the token spent on it
        // bought nothing; kept for a Retry, never shredded unplaced.
        if let Some(problem) =
            placement_problem(has_root_config, ud::system_config_path().is_file())
        {
            keep_unplaced(scratch, &made);
            return Err(failed(ErrorCode::MsiFatal, problem));
        }
        diag.ok();
        // Placed: the file has done its job.
        scratch.shred_config();
        // F1 — a cancel that came while the half ran (or the moment after):
        // it ran to its end, and the install stops here, saying so.
        if done.cancelled_late || cancel_requested() {
            keep_after_privileged(&made);
            Diagnostics::global().cancelled();
            return Err(cancelled_after_privileged(scope, root_half));
        }
    }

    // --- Step 4 (per-user): enroll in-process, as the person -------------
    if scope == Scope::PerUser {
        diag.begin(Step::Enroll);
        if let Err(e) = check_cancel() {
            keep_after_privileged(&made);
            return Err(e);
        }
        emit(on_event, ProgressEvent::EnrollStarted);
        // F4 — a config there that does not load is said, recovered from its
        // last good copy when it can be, and kept aside. #3116 — one this
        // person may not read (left root's by an old `sudo` install) stops
        // the install here, before the enroll spends the token.
        let existing = match crate::enroll_step::read_existing(&config_path) {
            Ok(existing) => existing,
            Err(said) => {
                keep_after_privileged(&made);
                return Err(failed(
                    ErrorCode::ConfigWrite,
                    format!(
                        "{said} It belongs to another account: give it back to yours (sudo chown \
                         \"$USER\" '{}') or remove it, then retry. Nothing was enrolled, and the \
                         enrollment token is still unused.",
                        config_path.display()
                    ),
                ));
            }
        };
        if let Some(message) = existing.warning.clone() {
            warn(on_event, message);
        }
        let reusable =
            kept_ids.filter(|(agent_id, _)| enrollment_in(existing.cfg.as_ref(), agent_id));
        match reusable {
            Some((agent_id, tenant_id)) => {
                warn(
                    on_event,
                    "reusing the enrollment from the previous attempt (its token was already spent)".to_string(),
                );
                emit_enrolled(on_event, "agent", &agent_id, &tenant_id);
                made.ids = Some((agent_id, tenant_id));
            }
            None => {
                let enrolled = crate::enroll_step::enroll_folded(
                    &config_path,
                    existing.cfg,
                    &server,
                    &token,
                    &device_name,
                    install_info(Scope::PerUser),
                )
                .await;
                let crate::enroll_step::Enrolled {
                    mut cfg,
                    outcome,
                    previous_row,
                } = match enrolled {
                    Ok(e) => e,
                    Err(e) => {
                        keep_after_privileged(&made);
                        return Err(failed(
                            enroll_error_code(&e),
                            person_enroll_refusal(&e, root_half),
                        ));
                    }
                };
                let note = crate::enroll_step::finish(
                    &mut cfg,
                    &outcome,
                    false,
                    &device_options(os, &advanced),
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
                // The device this install enrolled: on an org added beside the
                // primary, that org's row, never the primary's.
                let (agent_id, tenant_id) = roomlerd::enrollment::enrolled_identity(&cfg, &outcome);
                emit_enrolled(on_event, "agent", &agent_id, &tenant_id);
                made.ids = Some((agent_id, tenant_id));
            }
        }
        diag.ok();
    }
    let (agent_id, tenant_id) = made.ids.clone().ok_or_else(|| {
        failed(
            ErrorCode::Other,
            "no enrollment was made (a bug: the pipeline reached the user half without one)"
                .to_string(),
        )
    })?;
    if root_half && made.root_half_ids.is_none() {
        return Err(failed(
            ErrorCode::Other,
            "the root half was not enrolled (a bug: the pipeline reached the user half without it)"
                .to_string(),
        ));
    }

    // --- Step 5: the user half, as the person ----------------------------
    if let Err(e) = check_cancel() {
        keep_after_privileged(&made);
        return Err(e);
    }
    // The per-user unit cannot start, wherever its sandbox applies, until the
    // daemon's folders exist; `roomlerd enroll` would have made them, this
    // process's enrollment does not (`ud::unit_folders`). Here rather than at
    // the save, so that a Retry that reuses the enrollment makes them too.
    if scope == Scope::PerUser
        && let Err(e) = ud::make_unit_folders()
    {
        warn(
            on_event,
            format!(
                "{e}. The service cannot start where its sandbox applies until that folder exists."
            ),
        );
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
            keep_after_privileged(&made);
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
            keep_after_privileged(&made);
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
            keep_after_privileged(&made);
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
        root_half: made
            .root_half_ids
            .map(|(principal_id, tenant_id)| RootHalfReport {
                principal_id,
                tenant_id,
                name: crate::role::root_half_name(&device_name),
                config_path: ud::SYSTEM_CONFIG.to_string(),
            }),
    })
}

/// Step 2 for a Linux system scope: the enroll B1 made, into a fresh scratch
/// file — the running root daemon asked first which device it is, the machine
/// id derived from the system config's path, the fresh config finished with
/// the person's options, and a new device row said.
#[allow(clippy::too_many_arguments)]
async fn enroll_system(
    server: &str,
    token: &str,
    device_name: &str,
    config_path: &std::path::Path,
    install: roomlerd::enrollment::InstallInfo,
    options: &AdvancedOptions,
    overlay_default: Option<bool>,
    on_event: &Channel<ProgressEvent>,
) -> Result<(Scratch, (String, String)), String> {
    let s = Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?;
    // F3 — which device the root daemon here is, before the enroll makes one
    // (it may make ANOTHER: see below).
    let root = if ud::system_config_path().is_file() {
        crate::root_identity::query().await
    } else {
        None
    };
    // `existing: None` on purpose — root's config is unreadable here; the
    // privileged half folds into it (#2338).
    let crate::enroll_step::Enrolled {
        mut cfg, outcome, ..
    } = crate::enroll_step::enroll_folded(config_path, None, server, token, device_name, install)
        .await
        .map_err(|e| failed(enroll_error_code(&e), enroll_refusal(&e)))?;
    crate::enroll_step::finish(&mut cfg, &outcome, false, options, overlay_default)
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
        // The privileged half retires the key the previous row still claims
        // (`adopt-enrollment`); this says so first.
        warn(on_event, message);
    }
    let ids = (cfg.agent_id.clone(), cfg.tenant_id.clone());
    Ok((s, ids))
}

/// FR-129 B2 — step 2 for the macOS root half: its own enrollment, with its
/// own token, under its own name (`<name>-daemon`, as the terminal names it)
/// and a machine id from the hardware, so a reinstall finds its row however
/// the network has renamed the Mac since; the fresh config finished with the
/// overlay on, and saved to the scratch file the privileged half places (and
/// folds into the root half's existing config on a reinstall). Nothing here
/// reads or copies the person's config: the two halves never share a key
/// (`wg_key_taken_by_other` would refuse the second), and the root daemon
/// mints its own at its first start.
async fn enroll_root_half(
    server: &str,
    token: &str,
    device_name: &str,
    install: roomlerd::enrollment::InstallInfo,
    options: &AdvancedOptions,
    on_event: &Channel<ProgressEvent>,
) -> Result<(Scratch, (String, String)), String> {
    let s = Scratch::create().map_err(|e| failed(ErrorCode::ConfigWrite, e))?;
    let id = roomlerd::machine::resolve_root_half_machine_id(
        roomlerd::machine::read_mac_hardware(),
        random_bytes,
    );
    tracing::info!(
        from_hardware = id.from_hardware,
        "the root half's machine fingerprint"
    );
    if let Some(note) = &id.note {
        warn(on_event, note.clone());
    }
    let name = crate::role::root_half_name(device_name);
    let crate::enroll_step::Enrolled { cfg, .. } =
        crate::enroll_step::enroll_fresh_as(server, token, &name, &id.machine_id, install)
            .await
            .map_err(|e| failed(enroll_error_code(&e), root_half_refusal(&e)))?;
    let cfg = root_half_config(cfg, options)?;
    roomlerd::config::save(&s.config_from(), &cfg).map_err(|e| {
        failed(
            ErrorCode::ConfigWrite,
            format!("write the root half's enrolled config: {e}"),
        )
    })?;
    let ids = (cfg.agent_id.clone(), cfg.tenant_id.clone());
    Ok((s, ids))
}

/// FR-129 B2 — the root half's config, from ITS OWN fresh enrollment and the
/// root half's options only: the overlay on, the overlay's routes as given.
/// It takes no config to start from, so it cannot inherit the person's
/// WireGuard key; the key is minted by the root daemon at its first
/// overlay-enabled start (or kept from the root half's own previous config by
/// the fold in the privileged half).
fn root_half_config(
    fresh: roomlerd::config::AgentConfig,
    options: &AdvancedOptions,
) -> Result<roomlerd::config::AgentConfig, String> {
    let mut cfg = fresh;
    crate::enroll_step::finish(
        &mut cfg,
        &roomlerd::enrollment::EnrollOutcome::FreshPrimary,
        false,
        options,
        Some(true),
    )
    .map_err(|e| failed(ErrorCode::AdvancedOptions, e))?;
    Ok(cfg)
}

/// 32 bytes from the OS's generator, for a one-time machine id: two v4 UUIDs
/// (each `getrandom`'s), the only generator this crate already links.
fn random_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    out
}

/// FR-129 B2 — said at the preflight when the install adds the root half: a
/// Mac is two devices, and each counts against the plan.
fn two_devices_note(device_name: &str) -> String {
    format!(
        "This Mac joins as two devices: \"{device_name}\" for the screen (your session) and \"{}\" for the private network (a root service that owns the mesh's interface). Each counts as a device on the organization's plan.",
        crate::role::root_half_name(device_name)
    )
}

/// FR-129 B2 — after a privileged half that reported success: the root-owned
/// config this attempt enrolled (`enrolled_root_config`) is not where its
/// daemon reads it (`placed` is false). Said, so the person knows the
/// enrollment is kept for a Retry rather than lost. A belt: on a reinstall
/// the previous config stands there and this cannot tell the two apart.
fn placement_problem(enrolled_root_config: bool, placed: bool) -> Option<String> {
    (enrolled_root_config && !placed).then(|| {
        format!(
            "the administrator steps finished, but the enrolled config was not placed at {} — see \
             the lines above (on a Mac: did the package install its LaunchDaemon?). The \
             enrollment is kept: Retry places it without enrolling again.",
            ud::SYSTEM_CONFIG
        )
    })
}

/// F1 — the install stopped by a cancel that came after the administrator
/// steps had started: they ran to the end, and this says what that left.
pub fn cancelled_after_privileged(scope: Scope, root_half: bool) -> String {
    match (scope, root_half) {
        (Scope::System, _) => "Cancelled after the administrator steps had started. A step running as root cannot be stopped from here, so they ran to the end: the package is installed and this device is enrolled, its service running. Nothing else was done. Retry finishes the install without asking for the password or a token again.".to_string(),
        (Scope::PerUser, true) => "Cancelled after the administrator steps had started. A step running as root cannot be stopped from here, so they ran to the end: the package is installed and this Mac's private network (its root half) is enrolled, its service running, but the screen half is not enrolled yet. Retry finishes the install without asking for the password or the private network's token again.".to_string(),
        (Scope::PerUser, false) => "Cancelled after the administrator steps had started. A step running as root cannot be stopped from here, so they ran to the end: the package is installed, but this device is not enrolled yet. Retry finishes the install without asking for the password again.".to_string(),
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

/// FR-129 B2 — the root half's enroll refused: said as the private network's,
/// since it comes first — before the administrator steps, and before the
/// person's own half is enrolled.
fn root_half_refusal(e: &anyhow::Error) -> String {
    format!(
        "the private network's root half could not be enrolled — {}. The install stopped before the administrator steps.",
        enroll_refusal(e)
    )
}

/// The person's half refused. After a root half that WAS enrolled (FR-129
/// B2) the plan's device limit is the likely cause and worth naming: a Mac is
/// two devices, and the second may not fit.
fn person_enroll_refusal(e: &anyhow::Error, root_half: bool) -> String {
    let said = enroll_refusal(e);
    if root_half && said.contains("Device limit") {
        format!(
            "{said} This Mac is two devices, and its private-network half took the last free one: remove a device or upgrade the plan, then Retry, which reuses the private network's enrollment."
        )
    } else {
        said
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
    root_half: bool,
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
                    if scope == Scope::System || root_half {
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
        HalfOutcome::Unavailable(reason) => {
            terminal_fallback(args, reason, os, scope, root_half, on_event)
                .await
                .map(|()| PrivilegedDone {
                    cancelled_late: false,
                })
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
    root_half: bool,
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
    if (scope == Scope::System || root_half) && !ud::system_config_path().is_file() {
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

/// The Welcome page's words for what is here. On macOS the root config is
/// the root half's (FR-129 B2).
pub fn label_existing(d: &ud::UnixDetect, os: HostOs) -> String {
    let version = d.daemon_version.as_deref().unwrap_or("unknown version");
    let root = if os == HostOs::Macos {
        "its root half (the private network)"
    } else {
        "as a system daemon"
    };
    match (d.daemon_installed, d.per_user_config, d.system_config) {
        (false, false, false) => "clean".to_string(),
        (false, _, _) => "an enrollment without the daemon (the package was removed)".to_string(),
        (true, true, true) => format!("daemon {version}, enrolled per-user AND {root}"),
        (true, true, false) => format!("daemon {version}, enrolled per-user"),
        (true, false, true) => format!("daemon {version}, enrolled {root}"),
        (true, false, false) => format!("daemon {version}, not enrolled"),
    }
}

/// Installing one scope beside an enrollment of the other: said, not
/// refused — the person may want exactly that, or may be about to run two
/// daemons on one machine without knowing it. Not on macOS, where the two
/// are the design: the person's half and the root half (FR-129 B2).
pub fn other_scope_warning(d: &ud::UnixDetect, scope: Scope, os: HostOs) -> Option<String> {
    if os == HostOs::Macos {
        return None;
    }
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

fn emit_enrolled(
    on_event: &Channel<ProgressEvent>,
    principal_kind: &str,
    agent_id: &str,
    tenant_id: &str,
) {
    emit(
        on_event,
        ProgressEvent::EnrollOk {
            principal_kind: principal_kind.into(),
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
        assert_eq!(label_existing(&clean, HostOs::Linux), "clean");
        assert!(other_scope_warning(&clean, Scope::System, HostOs::Linux).is_none());
        let per_user = ud::UnixDetect {
            daemon_installed: true,
            daemon_version: Some("0.4.181".into()),
            per_user_config: true,
            system_config: false,
        };
        assert_eq!(
            label_existing(&per_user, HostOs::Linux),
            "daemon 0.4.181, enrolled per-user"
        );
        assert!(
            other_scope_warning(&per_user, Scope::System, HostOs::Linux)
                .unwrap()
                .contains("per-user")
        );
        assert!(other_scope_warning(&per_user, Scope::PerUser, HostOs::Linux).is_none());
        let system = ud::UnixDetect {
            daemon_installed: true,
            daemon_version: None,
            per_user_config: false,
            system_config: true,
        };
        assert_eq!(
            label_existing(&system, HostOs::Linux),
            "daemon unknown version, enrolled as a system daemon"
        );
        assert!(
            other_scope_warning(&system, Scope::PerUser, HostOs::Linux)
                .unwrap()
                .contains("system")
        );
        assert!(other_scope_warning(&system, Scope::System, HostOs::Linux).is_none());
    }

    /// FR-129 B2 — on macOS the root config is the root half's: named so on
    /// the Welcome page, and never warned about as "another daemon".
    #[test]
    fn on_macos_the_root_config_is_the_root_half() {
        let both = ud::UnixDetect {
            daemon_installed: true,
            daemon_version: Some("0.4.183".into()),
            per_user_config: true,
            system_config: true,
        };
        assert_eq!(
            label_existing(&both, HostOs::Macos),
            "daemon 0.4.183, enrolled per-user AND its root half (the private network)"
        );
        assert!(other_scope_warning(&both, Scope::PerUser, HostOs::Macos).is_none());
        assert!(other_scope_warning(&both, Scope::PerUser, HostOs::Linux).is_some());
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

    fn advanced(touched: &[&str]) -> AdvancedOptions {
        AdvancedOptions {
            overlay_enabled: Some(true),
            advertise_local_subnets: Some(false),
            auto_grant_session: Some(false),
            overlay_advertised_routes: Some("192.168.7.0/24".into()),
            advertise_routes: Some("10.9.0.0/16".into()),
            touched: touched.iter().map(|k| k.to_string()).collect(),
        }
    }

    /// FR-129 B2 — the Advanced options split between a Mac's two halves:
    /// the overlay's keys are the root half's, and never land on the
    /// person's half (whose overlay stays as B1 left it, off); everything
    /// else is the person's device's. Off macOS nothing is split.
    #[test]
    fn the_advanced_options_split_between_a_macs_two_halves() {
        let all = advanced(&[
            "overlay_enabled",
            "auto_grant_session",
            "overlay_advertised_routes",
            "advertise_routes",
        ]);
        let device = device_options(HostOs::Macos, &all);
        assert_eq!(device.overlay_enabled, None);
        assert_eq!(device.overlay_advertised_routes, None);
        assert_eq!(device.auto_grant_session, Some(false));
        assert_eq!(device.advertise_local_subnets, Some(false));
        assert_eq!(device.advertise_routes.as_deref(), Some("10.9.0.0/16"));
        assert_eq!(
            device.touched_keys(),
            vec!["advertise_routes", "auto_grant_session"]
        );

        let root = root_half_options(&all);
        assert_eq!(root.overlay_enabled, Some(true));
        assert_eq!(
            root.overlay_advertised_routes.as_deref(),
            Some("192.168.7.0/24")
        );
        assert_eq!(root.auto_grant_session, None);
        assert_eq!(root.advertise_local_subnets, None);
        assert_eq!(root.advertise_routes, None);
        assert_eq!(
            root.touched_keys(),
            vec!["overlay_advertised_routes", "overlay_enabled"]
        );

        // The root half's overlay is on whatever the box said: it exists for
        // the mesh (the box decides WHETHER it is installed, upstream).
        let mut off = all.clone();
        off.overlay_enabled = Some(false);
        assert_eq!(root_half_options(&off).overlay_enabled, Some(true));

        for os in [HostOs::Linux, HostOs::Windows] {
            let same = device_options(os, &all);
            assert_eq!(same.overlay_enabled, Some(true), "{os:?}");
            assert_eq!(same.touched_keys(), all.touched_keys(), "{os:?}");
        }
    }

    /// FR-129 B2 — the person's half on macOS never has the overlay written
    /// on by the box, fresh or refreshed; the root half's fresh config always
    /// has it on, with the routes, and carries no WireGuard key of anyone's:
    /// it is built from its own enrollment only, so it cannot hold the
    /// person's key (no `wg_key_taken_by_other` between the halves), and the
    /// root daemon mints its own.
    #[test]
    fn the_halves_configs_keep_the_overlay_and_the_keys_apart() {
        let all = advanced(&["overlay_enabled", "overlay_advertised_routes"]);

        // The person's half, fresh and refreshed: the overlay untouched.
        let mut person = roomlerd::config::test_fixture();
        person.overlay_enabled = false;
        person.overlay_wg_secret_key = Some("PERSON-KEY".into());
        crate::enroll_step::finish(
            &mut person,
            &roomlerd::enrollment::EnrollOutcome::FreshPrimary,
            false,
            &device_options(HostOs::Macos, &all),
            overlay_default(HostOs::Macos, Scope::PerUser),
        )
        .unwrap();
        assert!(!person.overlay_enabled);
        assert!(person.overlay_advertised_routes.is_empty());
        let mut refreshed = roomlerd::config::test_fixture();
        refreshed.overlay_enabled = false;
        crate::enroll_step::finish(
            &mut refreshed,
            &roomlerd::enrollment::EnrollOutcome::RefreshedPrimary,
            false,
            &device_options(HostOs::Macos, &all),
            None,
        )
        .unwrap();
        assert!(
            !refreshed.overlay_enabled,
            "a touched box does not reach the person's half"
        );

        // The root half: its own fresh enrollment, the overlay on.
        let mut fresh = roomlerd::config::test_fixture();
        fresh.agent_id = "aid-root".into();
        fresh.overlay_enabled = false;
        fresh.overlay_wg_secret_key = None;
        let root = root_half_config(fresh, &root_half_options(&all)).unwrap();
        assert!(root.overlay_enabled);
        assert_eq!(root.overlay_advertised_routes, vec!["192.168.7.0/24"]);
        assert_eq!(
            root.overlay_wg_secret_key, None,
            "minted by the root daemon"
        );
        assert_ne!(root.agent_id, person.agent_id);
    }

    fn kept(scope: Scope, privileged_done: bool, scratch: Option<Scratch>) -> KeptEnrollment {
        KeptEnrollment {
            scope,
            server: "https://example.invalid".into(),
            device_name: "lab".into(),
            root_half: false,
            ids: Some(("aid".into(), "tid".into())),
            root_half_ids: None,
            config_path: std::path::PathBuf::from(ud::SYSTEM_CONFIG),
            scratch,
            privileged_done,
        }
    }

    fn kept_root_half(privileged_done: bool, scratch: Option<Scratch>) -> KeptEnrollment {
        KeptEnrollment {
            scope: Scope::PerUser,
            server: "https://example.invalid".into(),
            device_name: "lab".into(),
            root_half: true,
            ids: None,
            root_half_ids: Some(("aid-root".into(), "tid".into())),
            config_path: std::path::PathBuf::from("/Users/lab/config.toml"),
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

    /// FR-129 B2 — F5 with two enrollments. A Mac's attempt that stopped at
    /// the password prompt (the root half enrolled, its config unplaced) hands
    /// the Retry that config and the root half's device, so the Retry places
    /// it without posting the root half's spent token; one that stopped after
    /// the privileged half hands the root half's device and skips the half;
    /// one whose work was removed since says the root half's token was spent,
    /// and keeps the person's enrollment for the per-user step to reuse from
    /// the person's config.
    #[test]
    fn a_retry_with_two_halves_reposts_neither_token() {
        // Stopped before the privileged half ran: the unplaced root config.
        let scratch = Scratch::create().unwrap();
        std::fs::write(scratch.config_from(), "agent_id = \"aid-root\"").unwrap();
        let (resume, note) = resume_from(
            Some(kept_root_half(false, Some(scratch))),
            Scope::PerUser,
            false,
        );
        assert!(note.is_none());
        assert!(!resume.privileged_done);
        assert_eq!(
            resume.root_half_ids,
            Some(("aid-root".into(), "tid".into()))
        );
        assert!(resume.scratch.expect("kept").config_from().is_file());

        // Stopped after it (a late cancel, a failed person's enroll): the
        // root half is done, and so is the half that placed it.
        let mut after = kept_root_half(true, None);
        after.ids = Some(("aid-user".into(), "tid".into()));
        let (resume, note) = resume_from(Some(after), Scope::PerUser, true);
        assert!(note.is_none());
        assert!(resume.privileged_done);
        assert_eq!(
            resume.root_half_ids,
            Some(("aid-root".into(), "tid".into()))
        );
        assert_eq!(resume.ids, Some(("aid-user".into(), "tid".into())));

        // Removed in between: from the start, the root half's spent token
        // said; the person's enrollment kept for the per-user step.
        let mut gone = kept_root_half(true, None);
        gone.ids = Some(("aid-user".into(), "tid".into()));
        let (resume, note) = resume_from(Some(gone), Scope::PerUser, false);
        assert!(!resume.privileged_done);
        assert!(resume.root_half_ids.is_none());
        assert_eq!(resume.ids, Some(("aid-user".into(), "tid".into())));
        let note = note.expect("said");
        assert!(
            note.contains("already used") && note.contains("new token"),
            "{note}"
        );

        // A per-user attempt without the root half, its work removed: nothing
        // root-owned was spent, so nothing to say.
        let mut plain = kept(Scope::PerUser, true, None);
        plain.config_path = std::path::PathBuf::from("/Users/lab/config.toml");
        let (resume, note) = resume_from(Some(plain), Scope::PerUser, false);
        assert!(note.is_none());
        assert_eq!(resume.ids, Some(("aid".into(), "tid".into())));
    }

    /// F1 — what a cancel after the start reports: never "nothing was
    /// installed"; the state the half left, per scope, and that a Retry does
    /// not ask again.
    #[test]
    fn a_late_cancel_says_what_the_half_left() {
        let system = cancelled_after_privileged(Scope::System, false);
        assert!(system.contains("ran to the end"), "{system}");
        assert!(system.contains("enrolled"), "{system}");
        assert!(!system.contains("nothing was installed"), "{system}");
        assert!(system.contains("password or a token"), "{system}");
        let per_user = cancelled_after_privileged(Scope::PerUser, false);
        assert!(per_user.contains("not enrolled yet"), "{per_user}");
        assert!(!per_user.contains("nothing was installed"), "{per_user}");
        // FR-129 B2: a Mac's root half was placed by that half; the screen
        // half was not enrolled yet, and neither the password nor the root
        // half's token is asked again.
        let mac = cancelled_after_privileged(Scope::PerUser, true);
        assert!(
            mac.contains("private network") && mac.contains("is enrolled"),
            "{mac}"
        );
        assert!(mac.contains("screen half is not enrolled yet"), "{mac}");
        assert!(
            mac.contains("password or the private network's token"),
            "{mac}"
        );
    }

    /// F5 — a kept enrollment is reused when its device is still in the
    /// config: the primary's, or the entry of an org the attempt added beside
    /// it. Without the second, a Retry after an install into another org
    /// enrolled again with the token the attempt had spent.
    #[test]
    fn a_kept_enrollment_is_found_in_an_added_org_too() {
        let mut cfg = roomlerd::config::test_fixture(); // primary "aid"
        assert!(enrollment_in(Some(&cfg), "aid"), "the primary");
        assert!(!enrollment_in(Some(&cfg), "aid-other"));
        let (merged, outcome) = roomlerd::enrollment::apply_enrollment(
            Some(cfg.clone()),
            {
                let mut other = roomlerd::config::test_fixture();
                other.server_url = "https://other.invalid".into();
                other.tenant_id = "tid-other".into();
                other.agent_id = "aid-other".into();
                other
            },
            None,
            false,
        )
        .unwrap();
        assert!(roomlerd::enrollment::is_added_org(&outcome));
        cfg = merged;
        let (kept, _) = roomlerd::enrollment::enrolled_identity(&cfg, &outcome);
        assert_eq!(kept, "aid-other", "the added org's own row");
        assert!(enrollment_in(Some(&cfg), &kept), "found in the added org");
        assert!(!enrollment_in(None, "aid"), "no config: nothing kept");
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

    /// FR-129 B2 — the refusals of a Mac's two enrollments. The root half's
    /// comes first, before anything is installed, and says so. The person's
    /// comes after the root half took a device; at the plan's limit that is
    /// the likely reason, and the Retry reuses the root half.
    #[test]
    fn a_macs_two_refusals_say_which_half_and_why() {
        let e = anyhow::anyhow!(
            "enrollment rejected (status 403 Forbidden): Device limit reached for the Free plan (100 of 100 devices used). Upgrade the plan or remove a device first."
        );
        let root = root_half_refusal(&e);
        assert!(
            root.contains("private network's root half")
                && root.contains("stopped before the administrator steps"),
            "{root}"
        );
        let person = person_enroll_refusal(&e, true);
        assert!(
            person.contains("two devices") && person.contains("reuses the private network"),
            "{person}"
        );
        assert_eq!(person_enroll_refusal(&e, false), enroll_refusal(&e));
        let other = anyhow::anyhow!("connection refused");
        assert_eq!(person_enroll_refusal(&other, true), enroll_refusal(&other));
        let note = two_devices_note("studio");
        assert!(
            note.contains("\"studio\"") && note.contains("\"studio-daemon\""),
            "{note}"
        );
    }

    /// The one-time id's bytes are random: two draws differ.
    #[test]
    fn random_bytes_differ() {
        assert_ne!(random_bytes(), random_bytes());
    }

    /// FR-129 B2 — an enrolled root-owned config that the half did not place
    /// is said (and kept for the Retry); a placed one, or none enrolled, is
    /// nothing to say.
    #[test]
    fn an_unplaced_root_config_is_said() {
        let said = placement_problem(true, false).expect("said");
        assert!(
            said.contains("was not placed") && said.contains("Retry places it"),
            "{said}"
        );
        assert!(placement_problem(true, true).is_none());
        assert!(placement_problem(false, false).is_none());
    }
}
