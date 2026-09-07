# Relinking the Roomler agent against your own FFmpeg

This page exists to make an LGPL right **exercisable** rather than theoretical.

The Windows Roomler agent (`roomlerd.exe`) links FFmpeg **statically**. FFmpeg is
LGPL-2.1-or-later, and §6 of that licence gives you the right to modify FFmpeg
and relink the application against your modified version. Below is how to do it.

If you only want to check that we are complying, the short version is: **the
whole application is open source, so the "work that uses the Library" is
available to you as source code, which is what LGPL-2.1 §6(a) asks for.** You do
not need anything from us that is not already published.

---

## Why no object-file archive is needed

LGPL-2.1 §6(a) requires the distributor to accompany the work with

> the complete corresponding machine-readable source code for the Library …
> **and**, if the work is an executable linked with the Library, with the
> complete machine-readable "work that uses the Library", **as object code
> and/or source code**, so that the user can modify the Library and then relink
> to produce a modified executable containing the modified Library.

Both halves are satisfied by publication, not by request:

| §6(a) requires | Where it is |
|---|---|
| Complete source of **the Library** | [`vendored-ffmpeg-9.0.1`](https://github.com/gjovanov/roomler-ai/releases/tag/vendored-ffmpeg-9.0.1) → `ffmpeg-<version>-corresponding-source.tar.xz` (pristine upstream + the full build recipe) |
| The **"work that uses the Library"**, as **source code** | This repository. The agent is MPL-2.0; `agents/roomlerd` and every crate it depends on are public. |
| Terms permitting modification and debugging | MPL-2.0 for our code; upstream licences for vendored crates |

The written offer in [THIRD-PARTY-NOTICES.md](../THIRD-PARTY-NOTICES.md) stands
in addition to this, not instead of it.

We apply **no patches to FFmpeg**. The only customisation is the configure flag
set, which selects components; it is in the build recipe.

---

## Procedure

### 1. Get the FFmpeg corresponding source

```bash
gh release download vendored-ffmpeg-9.0.1 --repo gjovanov/roomler-ai \
  --pattern 'ffmpeg-*-corresponding-source.tar.xz*'
sha256sum -c ffmpeg-n9.0.1-corresponding-source.tar.xz.sha256
tar xf ffmpeg-n9.0.1-corresponding-source.tar.xz
```

You now have the pristine upstream tarball and `recipe/`, which contains the
exact workflow that produced our binaries.

### 2. Modify and build FFmpeg

Make your changes, then build with the same component set we use — the flags are
in `recipe/vendor-ffmpeg-windows.yml`. The build must produce a tree laid out
like a normal prefix install:

```
<your-prefix>/
  include/
  lib/
    *.lib                 (or *.a / *.so on Unix)
    pkgconfig/
      libavcodec.pc
      libavutil.pc
```

⚠️ The **`.pc` files are load-bearing** — the Rust build finds FFmpeg through
`pkg-config`, not through hard-coded paths. If they are missing or their `prefix=`
is wrong, the build fails at the `ffmpeg-sys-next` step rather than silently
linking the wrong library.

You may of course build FFmpeg **shared** instead of static; nothing in the agent
requires static linkage.

### 3. Relink the agent

```bash
git clone https://github.com/gjovanov/roomler-ai
cd roomler-ai
git checkout <the tag matching your binary, e.g. agent-v0.4.13>
```

Point `pkg-config` at your build and rebuild. Windows (PowerShell), matching the
release workflow:

```powershell
$env:PKG_CONFIG_PATH = "<your-prefix>/lib/pkgconfig"
cargo build -p roomlerd --release --features `
  "full-hw,vp9-444,system-context,ffmpeg-encoder,overlay-l3,overlay-netstack,ssh-server"
```

Linux:

```bash
export FFMPEG_DIR=<your-prefix>
export PKG_CONFIG_PATH=<your-prefix>/lib/pkgconfig
cargo build -p roomlerd --release \
  --features "full,vp9-444,vp9-444-bindgen,overlay-l3,overlay-netstack,ffmpeg-encoder,ssh-server"
```

The result is `target/release/roomlerd(.exe)`, linked against **your** FFmpeg.

⚠️ Use the feature list for the platform you are reproducing — they differ, and
`ffmpeg-encoder` is deliberately **absent** on macOS (that build ships no FFmpeg
at all). The authoritative lists are the `cargo build` lines in
`.github/workflows/release-agent.yml`.

⚠️ Windows also needs libvpx for the VP9-4:4:4 path; the workflow vendors it the
same way (`vendor-libvpx-windows.yml`) and appends it to `PKG_CONFIG_PATH`. If
you do not need VP9-4:4:4, drop the `vp9-444` features.

### 4. Verify it works

```bash
./roomlerd --version
./roomlerd encoder-smoke --codec hevc     # exercises the FFmpeg encoder path
```

`encoder-smoke` feeds synthetic frames through the hardware cascade and reports
which encoder opened. That is the quickest confirmation that your FFmpeg is the
one actually being used.

---

## Note on the signature

A relinked binary is **not** signed by us, and our auto-updater deliberately
refuses artifacts that are not signed by `G ROX LTD` (see
[docs/code-signing.md](code-signing.md)). This is a security control, not a
restriction on your §6 rights: your modified build runs fine, but it will not be
distributed by our update channel, and you should point it away from that channel
or manage updates yourself.

---

Problems following this? **legal@roomler.ai** — if a step here does not work, the
compliance claim is wrong and we want to fix it.
