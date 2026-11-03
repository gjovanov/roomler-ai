# Tauri capabilities (intentionally none)

`roomler-desktop` grants its frontend no Tauri capability, so this directory
holds no `*.json` / `*.toml` capability file. Any such file placed here becomes
part of the app's ACL, so add one only as a deliberate security decision.

The directory exists anyway, because `tauri_build::build()` declares
`cargo:rerun-if-changed=capabilities` whether or not it does. Cargo treats a
watched path that does not exist as changed, so before this directory existed
every build re-ran the build script and recompiled `roomler-desktop`, including
a release tag run reusing its rehearsal's build (FR-6 wave 30). The capability
parser loads only capability extensions, so this README is ignored.
