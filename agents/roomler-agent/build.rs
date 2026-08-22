//! Embeds a Windows VERSIONINFO resource into `roomlerd.exe`.
//!
//! Authenticode says *who* signed a binary. VERSIONINFO is what Explorer's
//! Details tab, Task Manager's Publisher column, `Get-Process | Select
//! Company`, and most corporate AV/EDR consoles actually read. Until this
//! landed, `roomlerd.exe` -- a binary that runs as a LocalSystem service on
//! managed corporate fleets -- had NO version resource at all: no
//! CompanyName, no ProductName, no FileVersion, no copyright. That is an
//! independent AV heuristic signal on top of being unsigned, and it makes
//! the binary anonymous in every inventory tool an IT department runs.
//!
//! # Why `embed-resource` and not `winres` / `winresource`
//!
//! `winresource::WindowsResource::compile()` emits
//! `cargo:rustc-link-lib=dylib=resource`, and link-lib directives from a
//! dependency's build script propagate into the final link of every
//! DOWNSTREAM binary. This crate is a path dependency of BOTH Tauri crates
//! (`roomler-agent-tray` -> `roomler-desktop.exe`, `roomler-setup` ->
//! `roomler-setup.exe`), and both already embed their own VS_VERSION_INFO
//! via `tauri_build::build()`. The result would be two RT_VERSION resources
//! in each of those EXEs, with the winner decided by linker order.
//!
//! `embed_resource::compile_for(rc, ["roomlerd"], NONE)` emits
//! `cargo:rustc-link-arg-bin=roomlerd=...` -- scoped to one binary of one
//! package, and structurally incapable of leaking. It is also already in the
//! dependency graph (pulled by tauri-build), so it adds no supply-chain
//! surface.
//!
//! No-op on every non-Windows host.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    windows_version_info::embed();
}

#[cfg(windows)]
mod windows_version_info {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    /// Windows FILEVERSION is four u16 fields and Windows Installer ignores
    /// the fourth for comparison, so `CARGO_PKG_VERSION_*` would render every
    /// single rc of `0.3.0-rc.N` as the identical `0.3.0.0`.
    ///
    /// Use the same MAJOR.MINOR.RC remap `release-agent.yml` already applies
    /// to the MSI ProductVersion, so the EXE's file version and the MSI's
    /// product version agree: `0.3.0-rc.238` -> `0.3.238.0`. A final release
    /// maps the build field to 65535 so it outranks every rc of that minor.
    /// `release-agent.yml` asserts the two agree, so this cannot drift
    /// silently.
    fn file_version(pkg_version: &str) -> (u16, u16, u16) {
        let (core, rc) = match pkg_version.split_once("-rc.") {
            Some((core, rc)) => (core, rc.parse::<u32>().unwrap_or(0).min(65535) as u16),
            // Not an rc: a final release outranks every rc of the same minor.
            None => (pkg_version, 65535u16),
        };
        let mut parts = core.split('.');
        let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor, rc)
    }

    pub fn embed() {
        // Build scripts compile for the host, so `cfg(windows)` above is a
        // host check. Guard the TARGET too: a Windows box cross-compiling to
        // Linux must not try to staple a PE resource onto an ELF.
        if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
            return;
        }

        let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let (major, minor, build) = file_version(&pkg_version);
        let file_version_str = format!("{major}.{minor}.{build}.0");

        // Deliberately no `#include <winver.h>`: rc.exe's include path is not
        // guaranteed when it is invoked outside a Visual Studio developer
        // prompt. VS_VERSION_INFO is 1, VOS_NT_WINDOWS32 is 0x40004 and
        // VFT_APP is 0x1, so numeric literals make the resource
        // self-contained.
        let rc = format!(
            r#"1 VERSIONINFO
FILEVERSION {major},{minor},{build},0
PRODUCTVERSION {major},{minor},{build},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "G ROX LTD"
            VALUE "FileDescription", "Roomler Daemon"
            VALUE "FileVersion", "{file_version_str}"
            VALUE "InternalName", "roomlerd"
            VALUE "LegalCopyright", "Copyright (C) 2026 G ROX LTD"
            VALUE "OriginalFilename", "roomlerd.exe"
            VALUE "ProductName", "Roomler"
            VALUE "ProductVersion", "{pkg_version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
        );

        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));
        let rc_path = out_dir.join("roomlerd-version.rc");
        fs::write(&rc_path, rc).expect("failed to write the VERSIONINFO resource script");

        let result = embed_resource::compile_for(&rc_path, ["roomlerd"], embed_resource::NONE);

        // A missing rc.exe/llvm-rc must not break a developer's local build,
        // but it must never silently ship a release binary with no version
        // block -- exactly the state this build script exists to fix.
        if env::var("CI").is_ok() {
            result
                .manifest_required()
                .expect("VERSIONINFO embedding failed in CI (rc.exe / llvm-rc unavailable?)");
        } else if let Err(err) = result.manifest_optional() {
            println!("cargo:warning=roomlerd VERSIONINFO not embedded: {err:?}");
        }
    }
}
