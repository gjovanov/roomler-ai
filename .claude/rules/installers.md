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
write, so `make_elevated_folder` makes it new each time and follows no link. The publisher
check and the `%TEMP%` swap stay open in #3246, the last ways left for a limited process to reach
the administrator's token.

⚠️ **The cards narrow to what can work here** (`role::RoleLimit`), each with its note.
Another account's password (`--session-data` differs from this process's `%LOCALAPPDATA%`)
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
⚠️ **`RegisterAutostart` stays the last action before `InstallFinalize`.** The task names the
launcher, and a rollback onto a release without one would leave a task that starts nothing.
⚠️ **Restart Manager does not stop a running per-user daemon, console or not, so the MSI's
`StopDaemon` does, and it stays before `UnregisterAutostart`.** The log says "Successfully shut down
all applications", with no event 10002 (measured 0.4.196 to 0.4.197, and on the launcher); that
event only appears on a self-update, where the daemon is leaving anyway. Without the action an upgrade
by hand keeps the old daemon until the next sign-in, and an uninstall leaves it running with its
files gone, both with exit 0. A major upgrade runs it from the OLD product's table, and its fallback
ends the task, so after `UnregisterAutostart` it would reach nothing. `StopOldDaemon` is the same stop
from the NEW product, for an old table without it.
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
