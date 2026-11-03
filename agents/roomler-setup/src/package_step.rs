// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3246 — the file the wizard checks is the file `msiexec` runs.
//!
//! A digest arrives from the same origin as the URL, so it says the bytes came
//! through intact, never who made them. The anchor is the publisher: a valid
//! Authenticode signature whose signer is
//! [`roomlerd::code_signature::EXPECTED_PUBLISHER`], compiled in.
//!
//! A check is only worth something if nothing can change the file before it
//! runs, and on the machine-wide roles the run crosses to the administrator. A
//! process at the person's own level can write to their `%TEMP%`, can end or
//! inject into a limited wizard, and so can defeat anything that wizard does. So
//! the check that counts happens on the far side of the prompt:
//!
//! ```text
//! limited wizard: download to %TEMP% -> check it (a refusal needs no prompt)
//!   -> runas THIS EXE: --run-package <file> --stage <hex> [--digest ...]
//!                      --system-context <0|1>            (the one prompt)
//! elevated step:  a new folder only administrators can write
//!   -> copy the file in -> hold it open, refusing writers
//!   -> digest + publisher through that handle -> msiexec on it -> exit code
//! ```
//!
//! An elevated wizard runs the same thing in its own process, on a download it
//! made into such a folder in the first place. It starts no copy of its own
//! image: that file sits in the person's Downloads, where a limited process can
//! rename another into its place, and starting it from an elevated process
//! raises no prompt.
//!
//! The step takes nothing but a file and a yes-or-no. It builds `msiexec`'s
//! arguments itself, so a limited process that starts it can pass no property and
//! no transform, and it installs nothing we did not sign. It writes and deletes
//! only inside its own new folder, and only reads what it was pointed at: an
//! elevated write or delete under the person's `%TEMP%` would go wherever a link
//! planted there says. The person's `%TEMP%` does not reach `msiexec` either.
//!
//! The exit code is the one channel back, and a limited process cannot forge it:
//! [`exit_code`] has what it says.

use std::path::{Path, PathBuf};

use roomler_ai_remote_control::setup_diagnostics::ErrorCode;
use roomlerd::code_signature::{EXPECTED_PUBLISHER, VerifyError, verify_open_publisher};
use roomlerd::updater::WindowsInstallFlavour;
use wizard_shared::held_file::HeldFile;
use wizard_shared::msi_runner::MsiRunner;

use crate::diagnostics::failed;

/// The first argument of the elevated step.
pub const RUN_PACKAGE_FLAG: &str = "--run-package";
/// The name of the step's folder ([`stage_folder`]).
pub const STAGE_FLAG: &str = "--stage";
/// The digest the release published, when it published one.
pub const DIGEST_FLAG: &str = "--digest";
/// `1` for the pre-logon service, `0` for the plain machine-wide install.
pub const SYSTEM_CONTEXT_FLAG: &str = "--system-context";

/// The folder's name under `%ProgramData%`, before the random part. No shared
/// parent: a standard user can create folders in `%ProgramData%` and would own
/// one made first.
const STAGE_PREFIX: &str = "roomler-setup-";

/// Larger than any MSI we ship by far. A limited process can point the step at
/// any file, and it is copied before it is checked.
#[cfg(any(target_os = "windows", test))]
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;

/// The elevated step's exit codes beyond `msiexec`'s own, whose range they stay
/// out of.
pub mod exit_code {
    /// The base of a failure to stage the file; the OS error is added to it.
    pub(super) const STAGE_FAILED: i32 = 0x5243_0000;
    /// The base of a refusal.
    const REFUSED: i32 = 0x5244_0000;
    pub const BAD_ARGUMENTS: i32 = REFUSED + 1;
    pub const NOT_ELEVATED: i32 = REFUSED + 2;
    pub const DIGEST_MISMATCH: i32 = REFUSED + 3;
    pub const NOT_TRUSTED: i32 = REFUSED + 4;
    pub const WRONG_PUBLISHER: i32 = REFUSED + 5;
    pub const COULD_NOT_CHECK: i32 = REFUSED + 6;
    pub const MSIEXEC_DID_NOT_START: i32 = REFUSED + 7;
}

/// Why an installer was not run. Every variant is a refusal, and all of them
/// come before anything is installed and before the enrollment token is spent.
#[derive(Debug)]
pub enum Refusal {
    /// The bytes are not what the release's digest says.
    DigestMismatch,
    /// No valid signature, or one Windows does not trust.
    NotTrusted(String),
    /// Signed by someone else.
    WrongPublisher(String),
    /// The checks could not be made at all.
    CouldNotCheck(String),
    /// The elevated step refused a file this process had already passed: the
    /// file changed between the two.
    ChangedAfterCheck,
    /// The elevated step could not prepare its copy.
    StageFailed(std::io::Error),
    /// The step itself could not run as asked.
    StepFailed(String),
    /// A server named a download something that cannot be one file's name, or
    /// not the kind of file asked for.
    BadName(String),
    /// A release with no digest, where a digest is the only anchor there is.
    NoDigest,
    /// The installer itself did not start.
    DidNotStart(String),
}

impl Refusal {
    /// What the person reads. Each says what happened, that nothing happened
    /// because of it, and what to do next.
    pub fn words(&self) -> String {
        const NOTHING_HAPPENED: &str =
            "It was not run: nothing was installed, and the enrollment token is still unused.";
        const TRY_AGAIN: &str = "Try again. If this happens again, something on this computer or \
             network is changing what Roomler Setup downloads, or keeping Windows from checking it.";
        match self {
            Refusal::DigestMismatch => format!(
                "The installer did not arrive intact: it does not match the checksum the release \
                 published, and the download was deleted. {NOTHING_HAPPENED} {TRY_AGAIN}"
            ),
            Refusal::BadName(name) => format!(
                "Roomler Setup refused the download: the server named it {name:?}, which is not \
                 the name of an installer. {NOTHING_HAPPENED} {TRY_AGAIN}"
            ),
            Refusal::NoDigest => format!(
                "Roomler Setup refused the download: the release published no checksum for it, \
                 and on this system a checksum is the only way to tell it arrived intact. The \
                 download was deleted. {NOTHING_HAPPENED} Try again later."
            ),
            Refusal::DidNotStart(why) => {
                format!("The installer did not start ({why}). {NOTHING_HAPPENED} Try again.")
            }
            Refusal::NotTrusted(why) => {
                format!(
                    "Roomler Setup refused the installer: {why}. {NOTHING_HAPPENED} {TRY_AGAIN}"
                )
            }
            Refusal::WrongPublisher(found) => format!(
                "Roomler Setup refused the installer: it is signed by {found}, and Roomler is \
                 signed by {EXPECTED_PUBLISHER}. {NOTHING_HAPPENED} {TRY_AGAIN}"
            ),
            Refusal::CouldNotCheck(why) => format!(
                "Roomler Setup could not check the installer ({why}). {NOTHING_HAPPENED} Try again."
            ),
            Refusal::ChangedAfterCheck => format!(
                "The installer changed on this computer after Roomler Setup had checked it, so \
                 the administrator step refused it. {NOTHING_HAPPENED} Something on this computer \
                 replaced the file: have it looked at, then try again."
            ),
            Refusal::StageFailed(e) => format!(
                "The administrator step could not prepare the installer ({e}). \
                 {NOTHING_HAPPENED} Try again."
            ),
            Refusal::StepFailed(why) => {
                format!("The administrator step could not run ({why}). {NOTHING_HAPPENED}")
            }
        }
    }

    /// Its code in the opt-in install diagnostics.
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Refusal::DigestMismatch | Refusal::NoDigest => ErrorCode::Sha256Mismatch,
            Refusal::NotTrusted(_) | Refusal::WrongPublisher(_) | Refusal::ChangedAfterCheck => {
                ErrorCode::SignatureRefused
            }
            Refusal::CouldNotCheck(_) => ErrorCode::Other,
            Refusal::BadName(_) => ErrorCode::Resolve,
            Refusal::StageFailed(_) | Refusal::StepFailed(_) | Refusal::DidNotStart(_) => {
                ErrorCode::MsiSpawn
            }
        }
    }

    /// Record this in the install's diagnostics, and hand back what the person
    /// reads.
    pub fn fail(self) -> String {
        failed(self.error_code(), self.words())
    }

    /// The elevated step's exit code for this refusal.
    #[cfg(any(target_os = "windows", test))]
    fn exit_code(&self) -> i32 {
        match self {
            Refusal::DigestMismatch => exit_code::DIGEST_MISMATCH,
            Refusal::NotTrusted(_) => exit_code::NOT_TRUSTED,
            Refusal::WrongPublisher(_) => exit_code::WRONG_PUBLISHER,
            _ => exit_code::COULD_NOT_CHECK,
        }
    }

    /// What the elevated step's exit `code` means, when it is one of its own and
    /// not `msiexec`'s. By then this process has passed the same file, so a
    /// digest or signature refusal there means the file changed in between.
    pub fn from_step_exit(code: i32) -> Option<Refusal> {
        use exit_code as c;
        Some(match code {
            c::DIGEST_MISMATCH | c::NOT_TRUSTED | c::WRONG_PUBLISHER => Refusal::ChangedAfterCheck,
            c::COULD_NOT_CHECK => Refusal::StepFailed("the installer could not be read".into()),
            c::BAD_ARGUMENTS => {
                Refusal::StepFailed("it did not understand what it was asked".into())
            }
            c::NOT_ELEVATED => Refusal::StepFailed("it did not get administrator rights".into()),
            c::MSIEXEC_DID_NOT_START => {
                Refusal::StepFailed("Windows Installer did not start".into())
            }
            code if code & !0xffff == c::STAGE_FAILED => {
                Refusal::StageFailed(std::io::Error::from_raw_os_error(code & 0xffff))
            }
            _ => return None,
        })
    }
}

impl From<VerifyError> for Refusal {
    fn from(e: VerifyError) -> Refusal {
        match e {
            VerifyError::Untrusted(why) => Refusal::NotTrusted(why),
            VerifyError::WrongPublisher { found, .. } => Refusal::WrongPublisher(found),
            VerifyError::Unsupported => Refusal::CouldNotCheck(e.to_string()),
        }
    }
}

/// A file that passed, still held open, so what was checked is what stays there.
#[derive(Debug)]
pub struct Checked {
    _held: HeldFile,
    /// Who signed it.
    pub signer: String,
}

/// Hold `path` and check it through that handle: the published digest when
/// there is one, then the publisher, which decides whether or not there is.
pub fn check_held(path: &Path, digest: Option<&str>) -> Result<Checked, Refusal> {
    let mut held = HeldFile::open(path).map_err(|e| Refusal::CouldNotCheck(e.to_string()))?;
    if let Some(digest) = digest {
        match held.sha256_matches(digest) {
            Ok(true) => {}
            Ok(false) => return Err(Refusal::DigestMismatch),
            Err(e) => return Err(Refusal::CouldNotCheck(format!("{e:#}"))),
        }
    }
    let signer = verify_open_publisher(held.file(), path, EXPECTED_PUBLISHER)?;
    Ok(Checked {
        _held: held,
        signer,
    })
}

/// Where this run stages what it downloads, and a guard for it.
#[derive(Debug)]
pub struct Staging {
    root: PathBuf,
    /// Set when the folder is ours to remove: an elevated run's.
    remove: bool,
}

impl Staging {
    /// A limited run: the person's own `%TEMP%`, since nothing there crosses
    /// any boundary until the step copies it. An elevated run: a new folder only
    /// administrators can write. `%TEMP%` comes from `HKCU\Environment`, and a
    /// limited process can leave anything under it, so an elevated download
    /// there would write through whatever link it found.
    pub fn for_this_run() -> std::io::Result<Staging> {
        #[cfg(target_os = "windows")]
        if roomlerd::win_identity::process_is_elevated() {
            let root = stage_folder(&new_stage_name());
            roomlerd::updater::create_protected_dir(&root)?;
            return Ok(Staging { root, remove: true });
        }
        Ok(Staging {
            root: std::env::temp_dir().join("roomler-setup"),
            remove: false,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a download named by the server goes, or the refusal when the name
    /// is not one a file can have.
    pub fn path_for(&self, tag: &str, filename: &str) -> Result<PathBuf, Refusal> {
        wizard_shared::asset_resolver::staged_path(&self.root, tag, filename)
            .map_err(|_| Refusal::BadName(format!("{tag}/{filename}")))
    }

    /// Leave the folder when this drops, for the log of an install that failed,
    /// and take `package` out of it. A run in the person's own `%TEMP%` has
    /// nothing to keep.
    pub fn keep_log_of(&mut self, package: &Path) {
        if self.remove {
            let _ = std::fs::remove_file(package);
            self.remove = false;
        }
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.remove {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// A random folder name for the step, chosen by the caller, so that it knows
/// where the step's log is afterwards.
pub fn new_stage_name() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The step's folder for a stage name: directly under `%ProgramData%`.
pub fn stage_folder(name: &str) -> PathBuf {
    program_data().join(format!("{STAGE_PREFIX}{name}"))
}

/// `%ProgramData%`, as `launch::start_clean` set it from the machine's registry.
fn program_data() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// What the elevated step takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepArgs {
    pub source: String,
    pub stage: String,
    pub digest: Option<String>,
    pub system_context: bool,
}

impl StepArgs {
    /// The command line of the step, for `elevation::run_elevated_and_follow`.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![RUN_PACKAGE_FLAG.to_string(), self.source.clone()];
        args.push(STAGE_FLAG.to_string());
        args.push(self.stage.clone());
        if let Some(digest) = &self.digest {
            args.push(DIGEST_FLAG.to_string());
            args.push(digest.clone());
        }
        args.push(SYSTEM_CONTEXT_FLAG.to_string());
        args.push(if self.system_context { "1" } else { "0" }.to_string());
        args
    }

    /// Read a step's command line, without the program name. Nothing is ignored
    /// and nothing defaults: whoever started this may be a limited process, and
    /// the answer to anything unexpected is no. `None` when it is not a fit one.
    pub fn parse(args: &[String]) -> Option<StepArgs> {
        let mut args = args.iter().map(String::as_str);
        if args.next()? != RUN_PACKAGE_FLAG {
            return None;
        }
        let source = fit_source(args.next()?)?.to_string();
        if args.next()? != STAGE_FLAG {
            return None;
        }
        let stage = fit_stage(args.next()?)?.to_string();
        let mut flag = args.next()?;
        let digest = if flag == DIGEST_FLAG {
            let digest = fit_digest(args.next()?)?.to_string();
            flag = args.next()?;
            Some(digest)
        } else {
            None
        };
        if flag != SYSTEM_CONTEXT_FLAG {
            return None;
        }
        let system_context = match args.next()? {
            "1" => true,
            "0" => false,
            _ => return None,
        };
        if args.next().is_some() {
            return None;
        }
        Some(StepArgs {
            source,
            stage,
            digest,
            system_context,
        })
    }
}

/// A file on a local drive, by its full path, named like a plain `.msi`.
pub fn fit_source(value: &str) -> Option<&str> {
    let value = crate::launch::safe_folder(value)?;
    let name = value.rsplit('\\').next()?;
    wizard_shared::asset_resolver::plain_name(name).ok()?;
    name.to_ascii_lowercase().ends_with(".msi").then_some(value)
}

fn fit_stage(value: &str) -> Option<&str> {
    (value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())).then_some(value)
}

fn fit_digest(value: &str) -> Option<&str> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(value)
}

/// Does an install that ended with `code` leave its log for someone to read?
/// One that failed does. A prompt declined or another install in the way wrote
/// nothing worth keeping.
pub fn keeps_log(code: i32) -> bool {
    !matches!(code, 0 | 1602 | 1618 | 3010) && Refusal::from_step_exit(code).is_none()
}

/// Start `msiexec` on a package this process holds: full path, `TEMP` and `TMP`
/// pointed at `temp`, this process's own folder. It runs as this process, which
/// is elevated, so there is no second prompt and nothing of the person's in its
/// environment.
///
/// The property table, from rc.44: nothing for perUser, whose MSI does not
/// declare it; `ENABLE_SYSTEM_CONTEXT=0` for plain perMachine, explicitly, so the
/// `DisableSystemContext` custom action fires on a downgrade from a SystemContext
/// host; `=1` for SystemContext.
#[cfg(target_os = "windows")]
pub fn spawn_msiexec(
    package: &Path,
    flavour: WindowsInstallFlavour,
    temp: &Path,
    system_context: bool,
) -> std::io::Result<std::process::Child> {
    use std::process::Stdio;
    let properties: &[(&str, &str)] = match (flavour, system_context) {
        (WindowsInstallFlavour::PerUser, _) => &[],
        (WindowsInstallFlavour::PerMachine, true) => &[("ENABLE_SYSTEM_CONTEXT", "1")],
        (WindowsInstallFlavour::PerMachine, false) => &[("ENABLE_SYSTEM_CONTEXT", "0")],
    };
    let argv = roomlerd::updater::msiexec_argv_with_properties(package, flavour, properties);
    std::process::Command::new(roomlerd::appdirs::system32_tool("msiexec.exe"))
        .args(argv)
        .env("TEMP", temp)
        .env("TMP", temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// An install on its way: what to wait on, and where it leaves its log.
pub struct Started {
    pub runner: MsiRunner,
    log: PathBuf,
    log_needs_administrator: bool,
}

impl Started {
    /// Where the person finds out what went wrong.
    pub fn where_the_log_is(&self) -> String {
        if self.log_needs_administrator {
            format!(
                "Its log is at {}, which only an administrator can open.",
                self.log.display()
            )
        } else {
            format!("Its log is at {}.", self.log.display())
        }
    }
}

/// What the person reads after saying No to the administrator prompt.
///
/// rc.44: SystemContext REQUIRES perMachine and an administrator (it writes the
/// service's Environment block and restarts the service), so "pick perUser" is
/// not advice that applies there.
pub fn declined_words(system_context: bool) -> &'static str {
    if system_context {
        "Installation cancelled (you clicked No on the UAC prompt). \
         SystemContext mode requires admin rights — it writes to the \
         service's Environment block and restarts the service. Try \
         again and approve the UAC prompt, or pick plain perMachine \
         mode (without SystemContext) if you only need user-context \
         remote control."
    } else {
        "Installation cancelled (you clicked No on the UAC prompt). \
         Try again or pick perUser mode if you can't get admin rights."
    }
}

/// Start the install of a package this process has checked and still holds, for
/// the flavour the person's role picked. That is the role's and never the
/// wizard's own location: the EXE runs from wherever it was double-clicked, and a
/// perMachine MSI started as perUser is rejected with 1625 (rc.27/rc.28's B6).
///
/// Three ways it goes on Windows, and which one is decided by this process's own
/// token: elevated, it runs `msiexec` itself; limited and perMachine, it goes
/// through [the step](self); limited and perUser there is no boundary to cross.
pub async fn start_install(
    staged: &Path,
    digest: Option<&str>,
    flavour: WindowsInstallFlavour,
    system_context: bool,
    staging: &Staging,
) -> Result<Started, String> {
    let beside = |runner| Started {
        runner,
        log: PathBuf::from(format!("{}.log", staged.display())),
        log_needs_administrator: false,
    };
    #[cfg(target_os = "windows")]
    {
        if roomlerd::win_identity::process_is_elevated() {
            let child = spawn_msiexec(staged, flavour, staging.root(), system_context)
                .map_err(|e| Refusal::DidNotStart(e.to_string()).fail())?;
            // With `child` still held, so the process can't have gone yet.
            return follow(child.id()).map(beside);
        }
        if flavour == WindowsInstallFlavour::PerMachine {
            return start_step(staged, digest, system_context).await;
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = (digest, system_context, staging);
    let pid = roomlerd::updater::spawn_installer_for_flavour_with_properties(staged, flavour, &[])
        .map_err(|e| Refusal::DidNotStart(format!("{e:#}")).fail())?;
    follow(pid).map(beside)
}

/// A handle to follow a process by id. By now it is running, so this says it
/// could not be followed and not that nothing happened.
fn follow(pid: u32) -> Result<MsiRunner, String> {
    MsiRunner::attach(pid).map_err(|e| failed(ErrorCode::MsiWait, format!("attach msiexec: {e}")))
}

/// The one prompt, on this EXE, for [`run_step`].
#[cfg(target_os = "windows")]
async fn start_step(
    staged: &Path,
    digest: Option<&str>,
    system_context: bool,
) -> Result<Started, String> {
    use wizard_shared::elevation::{ElevationError, run_elevated_and_follow};
    let step_failed = |why: String| Refusal::StepFailed(why).fail();
    let source = staged.to_str().and_then(fit_source).ok_or_else(|| {
        step_failed(format!(
            "the download is at {}, which cannot be handed to it",
            staged.display()
        ))
    })?;
    let step = StepArgs {
        source: source.to_string(),
        stage: new_stage_name(),
        digest: digest.map(str::to_string),
        system_context,
    };
    let exe = std::env::current_exe().map_err(|e| step_failed(e.to_string()))?;
    let args = step.to_args();
    // Blocks for as long as the prompt is up.
    let answer = tauri::async_runtime::spawn_blocking(move || run_elevated_and_follow(&exe, &args))
        .await
        .map_err(|e| step_failed(e.to_string()))?;
    let runner = match answer {
        Ok(runner) => runner,
        Err(ElevationError::Cancelled) => {
            return Err(failed(
                ErrorCode::MsiUserCancel,
                declined_words(system_context).to_string(),
            ));
        }
        Err(ElevationError::Failed(why)) => return Err(step_failed(why)),
    };
    let name = staged.file_name().unwrap_or_default().to_string_lossy();
    Ok(Started {
        runner,
        log: stage_folder(&step.stage).join(format!("{name}.log")),
        log_needs_administrator: true,
    })
}

/// #3246 — a zip carries no signature of its own, so what is inside decides, and
/// all of it does: a genuine `roomler.exe` beside a `version.dll` of someone
/// else's would load that DLL. Each file is checked except plain text, in a new
/// folder under `work`, so a refused archive has overwritten nothing. The
/// signer's name is the program's.
#[cfg(any(target_os = "windows", test))]
pub fn check_archive(archive: &Path, work: &Path) -> Result<String, Refusal> {
    let folder = work.join(format!("check-{}", new_stage_name()));
    let verdict = check_unpacked(archive, &folder);
    let _ = std::fs::remove_dir_all(&folder);
    verdict
}

#[cfg(any(target_os = "windows", test))]
fn check_unpacked(archive: &Path, folder: &Path) -> Result<String, Refusal> {
    use wizard_shared::extract;
    extract::extract_archive(archive, folder)
        .map_err(|e| Refusal::NotTrusted(format!("the archive cannot be read ({e:#})")))?;
    let program = extract::find_tunnel_binary(folder)
        .map_err(|_| Refusal::NotTrusted("the archive holds no Roomler program".to_string()))?;
    let signer = check_held(&program, None)?.signer;
    check_each_file(folder)?;
    Ok(signer)
}

/// Check every file under `folder` that Windows could run or load.
#[cfg(any(target_os = "windows", test))]
fn check_each_file(folder: &Path) -> Result<(), Refusal> {
    let could_not_read = |e: std::io::Error| Refusal::CouldNotCheck(e.to_string());
    for entry in std::fs::read_dir(folder).map_err(could_not_read)? {
        let path = entry.map_err(could_not_read)?.path();
        if path.is_dir() {
            check_each_file(&path)?;
        } else if !is_plain_text(&path) {
            check_held(&path, None)?;
        }
    }
    Ok(())
}

/// A name nothing loads or runs by, which the archive may carry unsigned.
#[cfg(any(target_os = "windows", test))]
fn is_plain_text(path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name == "license" || name.ends_with(".md") || name.ends_with(".txt")
}

/// `main`'s first decision after it has made the process safe to run: is this
/// the elevated step? Then this is all the process does, before any window or
/// the single-instance lock, and `Some` is its exit code.
pub fn run_if_asked() -> Option<i32> {
    // Not `env::args`, which panics on a value that is not Unicode: this runs on
    // every start of the wizard.
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first()?.to_str() != Some(RUN_PACKAGE_FLAG) {
        return None;
    }
    let args: Option<Vec<String>> = args.into_iter().map(|a| a.into_string().ok()).collect();
    Some(args.map_or(exit_code::BAD_ARGUMENTS, |args| run_step(&args)))
}

#[cfg(target_os = "windows")]
fn run_step(args: &[String]) -> i32 {
    let Some(step) = StepArgs::parse(args) else {
        return exit_code::BAD_ARGUMENTS;
    };
    if !roomlerd::win_identity::process_is_elevated() {
        return exit_code::NOT_ELEVATED;
    }
    let root = stage_folder(&step.stage);
    if let Err(e) = roomlerd::updater::create_protected_dir(&root) {
        return stage_failed(&e);
    }
    let mut stage = Staging { root, remove: true };
    let source = Path::new(&step.source);
    // `fit_source` saw a file name.
    let copy = stage.root().join(source.file_name().unwrap_or_default());
    if let Err(e) = copy_in(source, &copy) {
        return stage_failed(&e);
    }
    let checked = match check_held(&copy, step.digest.as_deref()) {
        Ok(checked) => checked,
        Err(refusal) => return refusal.exit_code(),
    };
    let ran = spawn_msiexec(
        &copy,
        WindowsInstallFlavour::PerMachine,
        stage.root(),
        step.system_context,
    )
    .and_then(|mut c| c.wait());
    drop(checked);
    let code = match ran {
        Ok(status) => status.code().unwrap_or(exit_code::MSIEXEC_DID_NOT_START),
        Err(_) => exit_code::MSIEXEC_DID_NOT_START,
    };
    if keeps_log(code) {
        stage.keep_log_of(&copy);
    }
    code
}

/// There is no such step off Windows.
#[cfg(not(target_os = "windows"))]
fn run_step(_args: &[String]) -> i32 {
    exit_code::NOT_ELEVATED
}

#[cfg(any(target_os = "windows", test))]
fn stage_failed(e: &std::io::Error) -> i32 {
    exit_code::STAGE_FAILED + e.raw_os_error().unwrap_or(0).clamp(0, 0xffff)
}

/// Copy `source` to a new file at `copy`, stopping past [`MAX_PACKAGE_BYTES`].
/// Reads what it is pointed at, and writes only the new name.
#[cfg(any(target_os = "windows", test))]
fn copy_in(source: &Path, copy: &Path) -> std::io::Result<()> {
    use std::io::Read;
    let from = std::fs::File::open(source)?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(copy)?;
    let copied = std::io::copy(&mut from.take(MAX_PACKAGE_BYTES + 1), &mut to)?;
    if copied > MAX_PACKAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the file is too large to be an installer",
        ));
    }
    to.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str =
        r"C:\Users\Two Words\AppData\Local\Temp\roomler-setup\agent-v1\roomlerd-1-perMachine.msi";
    const STAGE: &str = "0123456789abcdef0123456789abcdef";

    fn step() -> StepArgs {
        StepArgs {
            source: SOURCE.to_string(),
            stage: STAGE.to_string(),
            digest: Some(format!("sha256:{}", "a".repeat(64))),
            system_context: true,
        }
    }

    /// What the limited copy sends is what the step reads.
    #[test]
    fn the_step_arguments_parse_back() {
        let sent = step();
        assert_eq!(StepArgs::parse(&sent.to_args()), Some(sent));
        let sent = StepArgs {
            digest: None,
            system_context: false,
            ..step()
        };
        assert_eq!(StepArgs::parse(&sent.to_args()), Some(sent));
    }

    /// The step runs for whoever can get it past the prompt, so it takes exactly
    /// its own arguments. A property or a transform has nowhere to go.
    #[test]
    fn the_step_takes_nothing_else() {
        let good = step().to_args();
        let parse = |edit: &dyn Fn(&mut Vec<String>)| {
            let mut args = good.clone();
            edit(&mut args);
            StepArgs::parse(&args)
        };
        assert!(parse(&|_| {}).is_some());
        assert!(StepArgs::parse(&[]).is_none());
        assert!(
            parse(&|a| a.push("TRANSFORMS=evil.mst".into())).is_none(),
            "an extra argument"
        );
        assert!(
            parse(&|a| a.truncate(a.len() - 1)).is_none(),
            "a value missing"
        );
        assert!(
            parse(&|a| a[0] = "--role".into()).is_none(),
            "another flag first"
        );
        assert!(
            parse(&|a| a[2] = "--stages".into()).is_none(),
            "another flag"
        );
        assert!(
            parse(&|a| *a.last_mut().unwrap() = "2".into()).is_none(),
            "not 0 or 1"
        );
        for source in [
            r"relative\x.msi",
            r"\\host\share\x.msi",
            r"\\?\C:\x.msi",
            r"C:\Users\..\x.msi",
            r"C:\x.exe",
            r"C:\x.msi:stream",
            r"C:\x.msi.",
            r"C:\nul.msi",
            r"C:\",
            "C:",
        ] {
            assert!(parse(&|a| a[1] = source.into()).is_none(), "{source}");
        }
        for stage in [
            "",
            "abc",
            r"..\..\Windows",
            &"g".repeat(32),
            &"a".repeat(33),
        ] {
            assert!(parse(&|a| a[3] = (*stage).into()).is_none(), "{stage}");
        }
        for digest in ["", "sha256:abc", &format!("md5:{}", "a".repeat(64))] {
            assert!(parse(&|a| a[5] = digest.into()).is_none(), "{digest}");
        }
    }

    /// `msiexec`'s own codes stay `msiexec`'s, the step's are the step's, and a
    /// refusal the limited copy did not make itself means the file changed.
    #[test]
    fn exit_codes_are_told_apart() {
        for code in [0, 1602, 1603, 1618, 1619, 1625, 3010, 42] {
            assert!(Refusal::from_step_exit(code).is_none(), "{code}");
        }
        for code in [
            exit_code::DIGEST_MISMATCH,
            exit_code::NOT_TRUSTED,
            exit_code::WRONG_PUBLISHER,
        ] {
            assert!(matches!(
                Refusal::from_step_exit(code),
                Some(Refusal::ChangedAfterCheck)
            ));
        }
        for code in [
            exit_code::BAD_ARGUMENTS,
            exit_code::NOT_ELEVATED,
            exit_code::COULD_NOT_CHECK,
            exit_code::MSIEXEC_DID_NOT_START,
        ] {
            assert!(matches!(
                Refusal::from_step_exit(code),
                Some(Refusal::StepFailed(_))
            ));
        }
    }

    /// The OS error survives the exit code, so disk-full reads as disk-full.
    #[test]
    fn a_stage_failure_carries_the_os_error() {
        let full = std::io::Error::from_raw_os_error(112);
        let code = stage_failed(&full);
        let Some(Refusal::StageFailed(back)) = Refusal::from_step_exit(code) else {
            panic!("not a stage failure: {code:#x}");
        };
        assert_eq!(back.raw_os_error(), Some(112));
        assert!(Refusal::from_step_exit(stage_failed(&std::io::Error::other("no code"))).is_some());
    }

    /// A failed install leaves its log; nothing else does.
    #[test]
    fn only_a_failed_install_keeps_its_log() {
        for code in [1603, 1625, 42] {
            assert!(keeps_log(code), "{code}");
        }
        for code in [
            0,
            1602,
            1618,
            3010,
            exit_code::NOT_TRUSTED,
            exit_code::BAD_ARGUMENTS,
            stage_failed(&std::io::Error::from_raw_os_error(5)),
        ] {
            assert!(!keeps_log(code), "{code:#x}");
        }
    }

    /// Every refusal says nothing happened because of it, and names the code the
    /// diagnostics carry.
    #[test]
    fn a_refusal_says_what_a_person_needs() {
        let all = [
            Refusal::DigestMismatch,
            Refusal::NotTrusted("the file is not signed".into()),
            Refusal::WrongPublisher("Evil Corp".into()),
            Refusal::CouldNotCheck("the disk went away".into()),
            Refusal::ChangedAfterCheck,
            Refusal::StageFailed(std::io::Error::from_raw_os_error(112)),
            Refusal::StepFailed("it did not get administrator rights".into()),
            Refusal::BadName(r"agent-v1/..\..\x.exe".into()),
            Refusal::NoDigest,
            Refusal::DidNotStart("access denied".into()),
        ];
        for refusal in &all {
            let words = refusal.words();
            assert!(words.contains("nothing was installed"), "{words}");
            assert!(words.contains("still unused"), "{words}");
            assert!(!words.contains("msiexec"), "not a person's word: {words}");
        }
        assert!(all[1].words().contains("the file is not signed"));
        assert!(all[2].words().contains("Evil Corp"));
        assert!(all[2].words().contains(EXPECTED_PUBLISHER));
        assert_eq!(all[0].error_code(), ErrorCode::Sha256Mismatch);
        assert_eq!(all[1].error_code(), ErrorCode::SignatureRefused);
        assert_eq!(all[4].error_code(), ErrorCode::SignatureRefused);
        assert_eq!(all[7].error_code(), ErrorCode::Resolve);
        assert_eq!(all[8].error_code(), ErrorCode::Sha256Mismatch);
    }

    /// A hostile name is refused before it can become a path.
    #[test]
    fn a_name_from_the_server_stays_under_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let staging = Staging {
            root: dir.path().to_path_buf(),
            remove: false,
        };
        assert_eq!(
            staging.path_for("agent-v1", "a.msi").unwrap(),
            dir.path().join("agent-v1").join("a.msi")
        );
        for (tag, filename) in [
            ("..", "a.msi"),
            ("agent-v1", r"..\..\Startup\x.exe"),
            ("agent-v1", "../x.msi"),
            ("agent-v1", r"C:\Windows\x.dll"),
        ] {
            assert!(
                matches!(staging.path_for(tag, filename), Err(Refusal::BadName(_))),
                "{tag} {filename}"
            );
        }
    }

    /// A refused or kept folder goes by what the run owns: an elevated run's
    /// folder is removed, but for a failed install's log, and a limited run's
    /// `%TEMP%` is left as it was.
    #[test]
    fn staging_removes_only_its_own_folder_and_keeps_only_a_log() {
        let dir = tempfile::tempdir().unwrap();
        let made = |name: &str| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("a.msi"), b"package").unwrap();
            std::fs::write(root.join("a.msi.log"), b"log").unwrap();
            root
        };

        let root = made("removed");
        drop(Staging {
            root: root.clone(),
            remove: true,
        });
        assert!(!root.exists());

        let root = made("kept");
        let mut staging = Staging {
            root: root.clone(),
            remove: true,
        };
        staging.keep_log_of(&root.join("a.msi"));
        drop(staging);
        assert!(!root.join("a.msi").exists(), "the package goes");
        assert!(root.join("a.msi.log").exists(), "the log stays");

        let root = made("limited");
        let mut staging = Staging {
            root: root.clone(),
            remove: false,
        };
        staging.keep_log_of(&root.join("a.msi"));
        drop(staging);
        assert!(root.join("a.msi").exists(), "not ours to touch");
    }

    /// Every file in an archive decides, except plain text.
    #[test]
    fn only_plain_text_goes_unsigned_in_an_archive() {
        for name in ["LICENSE", "license", "README.md", "NOTES.TXT"] {
            assert!(is_plain_text(Path::new(name)), "{name}");
        }
        for name in [
            "roomler.exe",
            "version.dll",
            "README.md.exe",
            "LICENSE.exe",
            "x.cpl",
            "x.lnk",
        ] {
            assert!(!is_plain_text(Path::new(name)), "{name}");
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("LICENSE"), b"text").unwrap();
        std::fs::write(dir.path().join("README.md"), b"text").unwrap();
        assert!(check_each_file(dir.path()).is_ok());

        let nested = dir.path().join("deeper");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("version.dll"), b"not signed").unwrap();
        assert!(check_each_file(dir.path()).is_err(), "found in a subfolder");
    }

    /// A bad archive refuses and leaves no check folder behind.
    #[test]
    fn a_bad_archive_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.zip");
        std::fs::write(&archive, b"not a zip").unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        assert!(matches!(
            check_archive(&archive, &work),
            Err(Refusal::NotTrusted(_))
        ));
        assert_eq!(std::fs::read_dir(&work).unwrap().count(), 0);
    }

    /// The copy goes to a new name only, is bounded, and leaves the source alone.
    #[test]
    fn the_copy_writes_only_its_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.msi");
        let copy = dir.path().join("b.msi");
        std::fs::write(&source, b"hello").unwrap();
        copy_in(&source, &copy).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"hello");
        assert!(
            copy_in(&source, &copy).is_err(),
            "never over something there"
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"hello");
        assert!(copy_in(&dir.path().join("none.msi"), &dir.path().join("c.msi")).is_err());
    }

    /// An unsigned file is refused through the held handle, and a wrong digest
    /// before that.
    #[test]
    fn an_unsigned_file_never_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.msi");
        std::fs::write(&path, b"hello").unwrap();
        let hello = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        for digest in [None, Some(hello)] {
            assert!(
                matches!(
                    check_held(&path, digest),
                    Err(Refusal::NotTrusted(_) | Refusal::CouldNotCheck(_))
                ),
                "{digest:?}"
            );
        }
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(matches!(
            check_held(&path, Some(&wrong)),
            Err(Refusal::DigestMismatch)
        ));
        assert!(matches!(
            check_held(&path, Some("nonsense")),
            Err(Refusal::CouldNotCheck(_))
        ));
    }

    /// A limited process makes no folder of its own this way, and an unfit
    /// command line ends the process before anything else runs.
    #[test]
    fn the_step_refuses_before_it_touches_anything() {
        assert_eq!(run_step(&["--run-package".to_string()]), {
            #[cfg(target_os = "windows")]
            {
                exit_code::BAD_ARGUMENTS
            }
            #[cfg(not(target_os = "windows"))]
            {
                exit_code::NOT_ELEVATED
            }
        });
    }

    /// `main` asks before it builds anything: the step never reaches a window,
    /// the single-instance lock or the wait for a parent.
    #[test]
    fn main_runs_the_step_before_any_window() {
        let calls = crate::spawn_gate::called_names(include_str!("main.rs"));
        let at = |name: &str| calls.iter().position(|c| c == name).expect(name);
        assert!(at("start_clean") < at("run_if_asked"));
        assert!(at("run_if_asked") < at("from_env"));
        assert!(at("run_if_asked") < at("wait_for_parent"));
        assert!(at("run_if_asked") < at("run"));
    }

    /// The stage name is what the parser accepts, and the folder sits directly
    /// under `%ProgramData%`.
    #[test]
    fn a_new_stage_name_is_a_fit_one() {
        let name = new_stage_name();
        assert_eq!(fit_stage(&name), Some(name.as_str()));
        assert_eq!(
            stage_folder(&name).parent(),
            Some(program_data().as_path()),
            "no shared parent a standard user could make first"
        );
    }
}
