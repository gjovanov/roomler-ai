# Linux self-update — design

**Status:** design. Prerequisite [#456](https://github.com/gjovanov/roomler-ai/pull/456) (arch-aware asset picker + aarch64 `.deb`) must land first.

## The problem

`Update all` from the web UI is a permanent no-op on any Linux host that is not
Debian-family **or** not x86_64. Three independent gaps, found 2026-08-15 on
`scw-m2-asahi` (Fedora Asahi Remix 42, Apple M2):

1. **No aarch64 asset.** Releases shipped an x86_64 `.deb`, two Windows MSIs and
   a macOS `.pkg`. `pick_asset_for_unix` found no match and every check ended
   `no installer asset for this platform in release <tag>`. Fixed by #456.
2. **The picker was arch-blind.** It accepted any `*.deb` while gated on
   `target_arch = "x86_64"`, so publishing a second architecture would have made
   x86_64 agents install a foreign package. Fixed by #456.
3. **The installer is Debian-only.** `linux_install_candidates` tries `apt-get`
   and `dpkg` (root) or `pkexec`/`sudo apt-get` (non-root). A Fedora / RHEL /
   SUSE / Arch host has none of them, so **no published artifact can install
   there on any architecture**. This document addresses (3).

`scripts/install.sh` has the same limitation for FIRST install: it hardcodes
`x86_64-unknown-linux-gnu\.deb` and `sudo dpkg -i`.

## Decision: a self-contained tarball as the universal path

Ship a per-arch `.tar.gz` carrying exactly what the `.deb` installs, and teach
the updater to use it when the host has no Debian tooling. Keep the `.deb` for
Debian-family hosts so `apt`/`dpkg` stay the source of truth there.

**Why a tarball rather than an `.rpm`:** one artifact per arch covers Fedora,
RHEL, openSUSE, Arch, Alpine-with-glibc and anything else, where an `.rpm`
covers only the RPM family and would make it three formats × two arches. The
payload is already self-contained — the `.deb` bundles FFmpeg + libvpx into
`/usr/lib/roomler-agent` with an RPATH precisely so it does not depend on the
distro's libraries — so the package manager is buying us very little beyond its
own bookkeeping. An `.rpm` lane stays available later for hosts where the
package DB matters; it is not needed to make self-update work.

**Cost is near zero in CI:** both Linux jobs already stage this exact payload for
`cargo deb`. The tarball is a `tar czf` over the same tree, no extra build.

## Artifacts

Per Linux arch (`x86_64`, `aarch64`), mirroring `release-tunnel.yml`'s naming:

```
roomler-agent-<version>-<arch>-unknown-linux-gnu.deb        (existing / #456)
roomler-agent-<version>-<arch>-unknown-linux-gnu.tar.gz     (new)
roomler-agent-<version>-<arch>-unknown-linux-gnu.tar.gz.sha256
```

Tarball layout — a prefix dir so extraction is never a surprise, mirroring the
`.deb` asset map in `agents/roomler-agent/Cargo.toml`:

```
roomler-agent-<version>-<arch>-unknown-linux-gnu/
  usr/bin/roomlerd                       # the daemon
  usr/bin/roomler                        # the CLI shim (re-execs `roomlerd cli`)
  usr/lib/roomler-agent/*.so.*           # bundled FFmpeg + libvpx ($ORIGIN RPATH)
  usr/lib/systemd/system/roomlerd.service
  usr/lib/systemd/user/roomler.service
  usr/share/doc/roomler-agent/README.Debian
```

## Asset selection

`pick_asset_for_unix` already qualifies by arch after #456. Add a format
preference resolved from the host, and **log the choice** — a silent pick is
indistinguishable from a wrong one:

| Host | Preferred | Fallback |
|---|---|---|
| `dpkg` or `apt-get` on PATH | `.deb` | `.tar.gz` |
| otherwise | `.tar.gz` | — (skip, as today) |

Detection is "is the tool actually on PATH", not `/etc/os-release` parsing: the
question we care about is whether the install can run, and a distro ID is a
proxy for that. Falling back to the tarball when a Debian host somehow lacks the
`.deb` keeps a missing artifact from becoming a dead end.

## Installing the tarball

New `install_tarball_linux` alongside `run_linux_install_candidates`, same
privilege ladder (root direct; otherwise `sudo -n`, then `pkexec`).

1. Extract to a staging dir under the same filesystem as `/usr`.
2. **Assert the expected members exist** before touching anything installed —
   a truncated download that passed the size floor must not half-install.
3. Copy `usr/bin/roomlerd` to `/usr/bin/roomlerd.new`, then `rename(2)` over the
   live path. Replacing a running executable this way is safe on Linux (the
   running process keeps its inode) and is already how the `.deb` path behaves.
   Keep the previous binary as `/usr/bin/roomlerd.prev` for the rollback path.
4. Same for `/usr/bin/roomler` and each `/usr/lib/roomler-agent/*.so.*`.
5. **Systemd units: write only if absent.** An update must never clobber a unit
   the operator edited — the field-test host carries a hand-written unit with
   `ROOMLER_AGENT_VIRTUAL_DESKTOP=1`, and silently reverting that on upgrade
   would be a data-loss-class bug. First install places them; upgrades leave
   them alone.
6. Exit and let the service manager restart, exactly as the `.deb` path does.

Integrity is unchanged: the updater already verifies GitHub's `sha256:` `digest`
per asset before install (`verify_sha256`), and that covers the tarball too.

## `scripts/install.sh`

First install currently only handles x86_64 + `.deb`. Teach it the same two
axes:

- arch from `uname -m` (`x86_64` → `x86_64`, `aarch64`/`arm64` → `aarch64`)
- format from the same PATH probe, with the tarball extracted to `/` under
  `sudo` and the units enabled as today

## Verification

- **Unit**: picker preference matrix (deb-preferred with tooling present,
  tarball when absent, arch never crossed, missing-asset fallback), and the
  tarball member assertion.
- **CI**: both Linux jobs must emit and upload the tarball; the existing
  stock-Ubuntu-24.04 load check already proves the payload runs.
- **Field, x86_64 Debian (no regression)**: the cluster nodes (`buildhost`, `fleet-host-2`,
  `fleet-host-1`, all Ubuntu) must keep taking `.deb` updates — they are the
  regression canary for the preference logic.
- **Field, aarch64 non-Debian (the target)**: `scw-m2-asahi` installs from the
  tarball and then takes an `Update all` end-to-end. That host is currently a
  source build with `auto_update = false` precisely because nothing publishable
  can install on it; this is the change that lets it rejoin the normal fleet
  lifecycle.

## Sequencing

1. #456 — arch-aware picker + aarch64 `.deb` *(open, rehearsal-verified)*
2. CI: stage + emit the tarball in both Linux jobs
3. Updater: format preference + `install_tarball_linux` + tests
4. `install.sh`: arch + format detection
5. Field-verify on `scw-m2-asahi`, then re-enable `auto_update` there

## Deliberately out of scope

- **`.rpm` lane** — the tarball makes self-update work everywhere; an `.rpm`
  only adds package-DB integration for one distro family. Revisit if a customer
  needs `dnf` to own the install.
- **musl / Alpine** — the payload is glibc-linked. A musl target is a separate
  build, not a packaging change.
- **arm64 FFmpeg** — the aarch64 build ships without `ffmpeg-encoder` (no
  linux-arm64 vendored FFmpeg exists, and its encoders dispatch to
  nvenc/qsv/amf, none of which exist on these hosts). Unrelated to install.
