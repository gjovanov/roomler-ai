---
paths:
  - "agents/roomler-setup/**"
  - "crates/roomler-setup-core/**"
  - "agents/roomlerd/packaging/**"
  - "agents/roomlerd/wix/**"
  - "agents/roomlerd/wix-perMachine/**"
  - "agents/roomler-desktop/packaging/**"
  - "crates/modules/fleet/src/setup_release.rs"
  - "crates/modules/fleet/src/agent_release.rs"
  - "agents/roomlerd/Cargo.toml"
  - "agents/roomler-desktop/Cargo.toml"
  - "scripts/install.sh"
  - "scripts/deb-bare-check.sh"
---

# Installation and packaging

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps the UAC naming rule, which binds any new crate. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

### Installation and packaging

`docs/installation.md` · `docs/code-signing.md` · the **`ship-it` skill**.

ONE unified wizard (`agents/roomler-setup`, Tauri 2) covers every role; backend
proxies under `/api/setup/*` and `/api/agent/installer/*` stream installer bytes
through `roomler.ai` (**not** `github.com`) so corporate ESET/Defender allow-lists
trust the download.

⚠️ **UAC lib-naming rule** — Windows UAC's "installer detection" heuristic
auto-elevates any EXE whose filename contains "install"/"setup"/"update"/"patch",
and cargo derives test-binary names from the **lib** crate, so wizard lib targets
must dodge those substrings (`wizard_app`, `wizard_shared`). The user-facing bin
keeps the marketing name; `[[bin]] test = false` keeps `cargo test -p roomler-setup`
off the UAC prompt.

⚠️ **The wizard EXE loads DLLs from System32 alone.** It sits in the person's Downloads
and the same image goes on to run elevated, so a DLL left beside it would load behind a
UAC prompt naming our publisher. `restrict_dll_search()` is `main`'s first line and
`build.rs` sets `/DEPENDENTLOADFLAG:0x800`: the first covers what loads later, the
second the EXE's own imports, and neither does the other's job. A DLL shipped beside
the EXE will not load. The flag does not reach an import's imports: `secur32.dll` carries no
flag of its own, so `build.rs` delay-loads it, and with it in the import table its `SSPICLI.DLL`
came from the EXE's folder before `main` could run, elevated too. CI reads both from the EXE
(`pe-dependent-load-flags.ps1 -NotImported`). A new static import of a DLL outside KnownDLLs
needs the plant test, reading the module list and not a summary of it.

⚠️ **No program on the wizard's Windows path is named by a bare name.** `Command` looks
for a bare name in the running EXE's folder before System32, and the shell's `runas` looks
in the current directory, so a bare `icacls` ran a file left beside the EXE with the
administrator's token and no prompt (measured, the spec's log). Use
`appdirs::system32_tool`, which asks the kernel, not `%SystemRoot%`; `spawn_elevated`
refuses anything but a full path. `agents/roomler-setup/src/spawn_gate.rs` fails CI on a new
bare name anywhere that path compiles, and on a program name it does not know.

⚠️ **An elevated wizard takes no override from its user.** A limited process can write any
variable to `HKCU\Environment` and an elevated process of that user inherits it, machine
folders such as `%ProgramData%` included. `launch::start_clean`, `main`'s second line, gives
those folders their true values on every run, leaves the current directory, and in an elevated
run drops `ROOMLER*`, `WEBVIEW2_*`, `TAURI_*` and `SSL_CERT_*`. A new override goes through
`launch::distrusted_variable`. The elevated WebView2 folder sits where the limited user can
write, so `make_elevated_folder` makes it new each time and follows no link.

⚠️ **The MSI the wizard runs is the one it checked (#3246), and the check that counts runs
elevated.** A process at the person's level can end or inject into a limited wizard, so a handle
that wizard holds proves nothing across the prompt. A limited machine-wide install `runas`es its own
EXE (`package_step.rs`, `--run-package`), which makes a new `%ProgramData%\roomler-setup-<hex>` with
its DACL set at creation, copies the file in, holds it refusing writers, checks digest and publisher
through that handle, and runs `msiexec` by full path with `TEMP` set to that folder. It takes only a
file and a yes-or-no, builds `msiexec`'s arguments itself, and writes and deletes nothing outside its
folder. An elevated wizard does this in its own process and **never re-launches `current_exe`**: a
limited process can rename another file into that path, and an elevated `runas` raises no prompt. It
also does **not** download under `%TEMP%`, and does not override `TEMP` for itself (WebView2's browser
runs as the session's user). Any manifest name that becomes a path goes through
`asset_resolver::plain_name`. A zip has no signature, so every file in one but plain text passes the
publisher check: a genuine `roomler.exe` would load a `secur32.dll` left beside it (its imports are no
KnownDLLs, and it sets no `DependentLoadFlags`). `wintrust.dll` (the publisher check) and
`wtsapi32.dll` (#3229's look for the person's shell, which runs before any window) are
delay-loaded for the same reason as `secur32.dll`, and `-NotImported` reads all three.

⚠️ **The cards narrow to what can work here** (`role::RoleLimit`), each with its note.
Another account's password (`--session-data`, or on a start by hand the folder `LaunchArgs::from_env`
finds from the owner of the session's Explorer, differs from this process's `%LOCALAPPDATA%`)
leaves only `daemon-system`, because the other roles would install into that administrator's
profile and still report Done. A policy that refuses prompts from a standard user
(`ConsentPromptBehaviorUser=0`) leaves `daemon-user` and the tunnel, preselected, so nobody
pairs first and meets the refusal after.

⚠️ **WebView2's browser process runs as the session's user, not as the wizard.** Under
over-the-shoulder credentials an elevated wizard is the administrator and its browser
the standard user, so the data folder must be one that user can open
(`launch::elevated_webview_folder`). Otherwise the window stays white and goes away
after about a minute (`docs/fr/FR-129-guided-cross-platform-installer.md`, C3).
<!-- RETIRED-NAME-ANCHOR(2): these two directories were DELETED under their
     retired names. A history line has to name what actually existed; an
     earlier path sweep rewrote one of them into agents/roomler-cli-installer,
     a directory git has never heard of. FR-21 D6. -->
⚠️ The legacy wizards `agents/roomler-installer` (rc.28) and
`agents/roomler-tunnel-installer` (rc.59) were retired in P4c-2. The tunnel CLI's
`self-update` is **kept** — it is the sole updater for tunnel-only hosts ("one
updater" is per-role; daemon hosts get `roomler.exe` refreshed by the MSI).

⚠️ **The per-user task, the MSI's custom actions and the Start menu entry run `roomlerdw.exe`,
never `roomlerd.exe`** (#3292, `docs/installation.md`). Windows opens a terminal for a console
program started from any of those, on the person's desktop, and closing it stopped the device.
`FreeConsole` first thing does not help (the console exists before `main`), and `conhost
--headless` is an EDR rule. The launcher leaves with the daemon's exit code and holds it in a
kill-on-close job, because `schtasks /End` kills the launcher alone (measured); what the daemon
spawns stays out of that job, or `msiexec` dies with it on an update. It must not touch the data
tree: `appdirs::use_old_segment` is new-if-present, so a log folder made from outside the daemon
orphans a legacy host's config, which is why stderr capture lives in the daemon.
⚠️ **The per-Machine MSI carries the service's `Environment` across an upgrade, and that may never fail
one** (#3375, `docs/installation.md`). The old product's removal deletes the service, so
`CaptureServiceEnvironment` (`agents/roomler-msi-ca`) reads the block before `InstallInitialize`, and
`RestoreServiceEnvironment` writes it back after `RegisterService` and before every SystemContext action:
those restart the service and decide the switch, which is never carried. Both stay `Return="ignore"` and
return success after a logged miss, and a DLL that cannot load is no exception: it carries nothing and the
upgrade completes (under `"check"` it would be 1723 and a rollback). The DLL stays `+crt-static` with system
imports only so that the carry works without the VC++ runtime. No value goes on a command line or into the
log: the properties are hidden, the lines name entries.
⚠️ **An entry the installer writes into the service's `Environment` goes into `INSTALLER_OWNED`
(`agents/roomler-msi-ca/src/carry.rs`) or is written after the restore.** The restore puts a carried entry
over any entry of the same name, so a new installer-written value would lose to the old one on every
upgrade. And a carried entry the NEW release cannot start with now fails the upgrade where it used to be
dropped: `EnableSystemContext` is `Return="check"` and waits 120 s for RUNNING (`docs/installation.md`).
⚠️ **`RegisterAutostart` stays the last action before `InstallFinalize`.** The task names the
launcher, and a rollback onto a release without one would leave a task that starts nothing.
⚠️ **`AutostartBackOnRollback` and `DaemonBackOnRollback` stay ahead of `RemoveExistingProducts`.** A
rollback plays the script backwards, so they run last, with the old files back; anywhere later they
would run on files that are not there yet. WiX numbers siblings by name, so the names have to sort
that way, and `installer-smoke` holds it against the built MSI.
⚠️ **`InstallExecute` stays right before `RemoveExistingProducts`, so the MSI's schedule is
`afterInstallExecute`.** With anything spooled ahead of it, that action fails every upgrade, healthy
ones included: `Error 2613`, exit 1603 (measured on signed candidates). ICE63 only warns, and a
compile or a sequence read does not see it: only a real upgrade does.
⚠️ **`IgnoreRemoveFailure='yes'` on the per-user `MajorUpgrade` keeps a device updating after a failed
upgrade; it does not make a rollback work.** A standard user's rollback cannot restore the Installer's
registration (error 5 on each registry open, `Error in rollback skipped`), so the failed product stays
registered with no package, and without the attribute every later release stops at Error 1714, System
Error 1612 (measured). With it a self-update goes on, at the price of a stale Apps entry a person cannot
remove. The daemon has to be gone first, as on a self-update: by hand with it running, Error 1310, exit
1603, no harm (`docs/installation.md`, "When a per-user upgrade fails").
⚠️ **Nothing a custom action starts proves a policy allows a file.** The installer's token is
`SANDBOX_INERT`, which AppLocker does not check: the blocked launcher ran every action, with MSI exit
0, and the task it registered could not start (`0x800704EC`). A scheduled task is the start with the
real token, which is why `launcher_is_blocked` (`agents/roomlerd/src/service.rs`) goes through one.
⚠️ **The check reads `Exe.AppLocker`, the compiled policy, not the registry.** An emptied policy
leaves that file enforcing through a reboot, with `SrpV2\Exe` holding no rule (measured).
⚠️ **Restart Manager does not stop a running per-user daemon, so the MSI does, and the two stops are
not interchangeable.** The daemon declines `ENDSESSION_CLOSEAPP` on purpose, and the log still says
"Successfully shut down all applications", with no event 10002 (measured 0.4.196 to 0.4.197). Without
the actions an upgrade by hand keeps the old daemon until the next sign-in, and an uninstall leaves it
running with its files gone, both with exit 0. `StopDaemon` is the uninstall's plain `service stop`, so
an ephemeral device removes itself, and it stays before `UnregisterAutostart`, whose deleted task its
fallback needs. `StopOldDaemon` is the upgrade's, from the NEW product, with `--for-upgrade`, which
records an update attempt first: ⚠️ a plain stop during an upgrade deletes an ephemeral device (#1683,
`docs/ephemeral-nodes.md`).
⚠️ **`<RestartOnFailure>` is not a supervisor**: it restarted nothing after exit 1 or
`0xC000013A` (a crash stays down, #3295). What brings a device back after a self-update is the one-shot `<TimeTrigger>`s
`service install` adds when a config exists. A repeating trigger would cover a crash too, and was
rejected: it fires inside the installer's window on the old files, and needs a latch for
`service stop` and for a removed row.

⚠️ **A `.deb` can install cleanly and not start** (#3204). Any edit to a package's
`depends` goes through `scripts/deb-bare-check.sh` on the real package; a hand-written
package list proves nothing about what `depends` resolves to. The ALSA dependency is
`libasound2t64 (>= 1.0.29) | libasound2 (>= 1.0.29)`, and it is not a candidate for
simplifying: unversioned, a stub that lacks the daemon's symbols satisfies it, and
`libasound2 (>= …)` alone cannot replace that stub. Every spelling was measured
(`docs/installation.md`, "What a bare host needs").

⚠️ **Never a bare `apt-get -f install` on the daemon's path.** When it cannot reach a
dependency it removes the package and exits 0: over a working install that deletes a
running daemon. `deb_install` in `scripts/install.sh` names the package, so apt
refuses instead.
