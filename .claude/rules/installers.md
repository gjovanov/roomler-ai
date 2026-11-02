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
publisher check: a genuine `roomler.exe` would load a `version.dll` left beside it. `wintrust.dll` is
delay-loaded for the same reason as `secur32.dll`, and `-NotImported` reads both.

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
