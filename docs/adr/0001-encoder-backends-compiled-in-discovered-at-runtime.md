---
status: accepted
date: 2026-09-07
---

# Every hardware encoder backend is compiled into every build and discovered at runtime

Adding the remaining FFmpeg hardware encoder backends (VAAPI now; D3D12 and
Vulkan later) raised the question of shipping one agent build per GPU vendor
with an installer that picks the right one. We measured the alternative
instead: the vendor wrappers in FFmpeg 8.1.2's static library sum to under
1 MB of linkable code for every backend on every platform (NVENC 86 KB, QSV
76 KB, AMF 70 KB, Media Foundation 51 KB, D3D12 83 KB plus 349 KB of shared
bitstream writers), against a 35 MiB agent binary. So there is one build per
OS and architecture with every backend compiled in, and the start-up probe in
the child process decides what the host can actually open.

## Considered options

- **Per-vendor builds plus a "smart" installer.** Rejected: the saving is
  under 1 MB; a laptop routinely carries two vendors (this decision was taken
  on a box with an NVIDIA dGPU and an AMD iGPU), so a per-vendor build is wrong
  by construction; and the vendor axis would multiply the release matrix and
  the auto-updater's asset picker, which is the surface whose mistakes freeze
  the fleet.
- **Downloading backend plugins on demand.** Rejected: it turns the encoder
  path into a second updater with its own signing and rollback story.

## Consequences

- A Linux package links backends the host may not have; a missing vendor
  runtime is a failed open in the probe, never a failed daemon start. The one
  hard runtime dependency this adds is `libva` on Linux, declared as a package
  dependency rather than bundled so it matches the host's drivers.
- The probe opens more encoders at start-up; its duration is reported in the
  agent's hello so the fleet can measure the cost instead of assuming it.
- The vendor wrapper code stays under a size guard in the release workflow;
  the guard is what turns "cheap" from an assumption into a check.
