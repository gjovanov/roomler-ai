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
the EXE will not load.

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
