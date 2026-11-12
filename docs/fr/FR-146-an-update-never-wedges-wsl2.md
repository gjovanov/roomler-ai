# FR-146: An agent update must never wedge the host's WSL2

**Issue:** [#3768](https://github.com/gjovanov/roomler-ai/issues/3768) · **Status:** claimed 2026-11-12. Spec only,
nothing built. Reproduction first: the cause is **not proven**. P0 needs a lab host (D4), which the operator picks.
**Anchors:** master `7d9cea3d5` (2026-11-12). **Related:** FR-121 (two agents on one machine under mirrored
networking), FR-77 (the encoder capability probe), FR-84 (who restarts the daemon), #3765 and #3766 (other
after-restart effects on Windows).

## Goal

A Roomler update, or any other lifecycle event of the daemon (stop, start, restart, the probes a start runs),
never leaves a host's WSL2 unable to start a new session, on either side of the WSL boundary:

- the **Windows daemon** of a host that runs a WSL2 VM, whose overlay adapters mirrored networking copies into
  the VM, and
- a **daemon inside the WSL2 guest**, which shares one kernel and one VM with every other distro on that host.

"Never" is measured, not assumed: a session canary (`wsl -d <distro> -e true`, every 2 s) stays green through
20 updates in a row on a lab host under the conditions that wedged the incident host, and the fleet's WSL2 guests
show no probe hang at their next update.

## The incident (2026-11-12) and what the logs say

A Windows 11 dev laptop runs WSL2 in **mirrored** networking (its `.wslconfig` sets `networkingMode=mirrored`,
and the guest daemon logged "WSL2 MIRRORED networking detected" at 12:58:56.568Z). It carries **two enrolled
daemons**: the Windows one, a per-Machine service that self-updates through the MSI, and a Linux one inside the
Ubuntu distro, a systemd unit that self-updates through the `.deb` and runs a virtual desktop. A GPU job ran in
the same VM.

At 12:58:28Z one operator-triggered update (`rc:agent.update`) reached **both** daemons, 4 ms apart. About four minutes later every new
`wsl -d <distro> -e …` answered `Wsl/Service/E_UNEXPECTED`, until `wsl --shutdown` at 13:46:40Z. Earlier that day,
at 09:23Z, the Windows daemon's 0.4.214 MSI update had made **the same adapter churn**, and nothing broke.

```mermaid
sequenceDiagram
    autonumber
    participant S as Server
    participant W as Windows daemon (service)
    participant H as Hyper-V and HNS (mirrored)
    participant G as Guest daemon (.deb)
    participant K as Guest kernel (one VM)
    participant X as wsl.exe (a new session)
    S->>W: rc:agent.update (12:58:28.696Z)
    S->>G: rc:agent.update (12:58:28.700Z)
    W->>W: spawn msiexec /qn, worker exits (12:58:30–33Z)
    W->>H: both Wintun adapters removed (12:58:33.56Z, .66Z)
    H->>K: HCS Remove x2, the VM's mirrors of them (12:58:33.94Z, 34.68Z)
    G->>G: apt-get installs, exits, systemd restarts it (12:58:31.9Z to 45.0Z)
    W->>H: new service, adapters re-created (12:58:39.97Z, 53–58Z)
    H->>K: HCS Add x2 (12:58:56.64Z, 12:59:13.63Z)
    G->>K: caps probe child opens hevc_nvenc: a sysfs PCI config read (from 12:58:45.7Z)
    K-->>K: a vCPU sits in hv_pcifront_read_config; soft lockups 12:59:10Z to 13:04:40Z
    X->>K: new session: SessionLeader accept4 times out (110) from 13:02:16Z
    Note over X,K: E_UNEXPECTED until wsl --shutdown (13:46:40Z)
```

### What was re-verified, and from where

An earlier session wrote the first reading down. Every claim was re-read from the logs on 2026-11-12, read-only:
`Get-WinEvent` (Hyper-V-Compute, Hyper-V-VmSwitch, Hyper-V-Worker, Host-Network-Service-Admin, Kernel-PnP,
NetworkProfile, NCSI, the firewall log, Application/MsiInstaller and RestartManager, System/Service Control
Manager), the daemon's own service and worker logs, and the guest's **persistent journal** of the boot that ended
at 13:46:40Z (`journalctl -b -1`). Times are UTC; the laptop's clock runs at UTC+1. The Host-Network-Service
*Operational* log had rolled over at 13:47:15Z, so only its Admin log covers the windows.

| # | Claim | Result | Evidence |
|---|---|---|---|
| 1 | WSL2 in mirrored networking | **verified** | the `networkingMode` line of `.wslconfig`; the guest daemon's own detection line at 12:58:56.568Z |
| 2 | The Windows daemon self-updates through the per-Machine MSI | **verified** | MsiInstaller 1040 at 12:58:30.704Z (`roomlerd-0.4.215-perMachine-…msi`, client pid 78612); 1033 "Roomler (per-Machine) 0.4.215", status 0, at 12:58:41.742Z |
| 3 | The 0.4.214 update at 09:23:04Z removed the VM's vNICs, the guest logged nft errors, WSL survived | **verified** | 1040 at 09:23:04.871Z (client pid 115424); Hyper-V-Compute 2007 `RequestType: Remove` x2 at 09:23:07.781Z and .842Z; guest `GnsEngine … nft -a list chain ip nat WSLPOSTROUTING killed by signal 13` x2 at 09:23:07.780Z and .843Z; Add again at 09:23:27.935Z and 09:23:31.275Z; no session failure in the guest journal |
| 4 | The 0.4.215 update began at 12:58:30Z, msiexec client pid 78612 | **verified** | MsiInstaller 1040, 12:58:30.704Z |
| 5 | Three seconds later Hyper-V removed BOTH vNICs of the WSL VM | **verified, and narrowed** | Compute 2007 Remove at 12:58:33.943Z and 12:58:34.675Z. The two vNICs are the VM's **mirrors of the Windows daemon's own two Wintun adapters**: the primary `{B5A7D160-…}` (`ROOMLER_TUN_GUID`) and a secondary org's (`org_tun_guid`). Kernel-PnP 1010 reports both surprise-removed at 12:58:33.555Z and .660Z, as the old worker exited (service log: worker exited gracefully at 12:58:33.750Z). The host's own NICs stayed. Re-added at 12:58:56.639Z and 12:59:13.626Z, with one `HNS-Network-Create` 0x803B0006 retried to success at 12:59:00.406Z, which also happened at 09:23Z |
| 6 | The guest's GnsEngine logged nft errors | **verified** | 12:58:33.890Z and 12:58:34.665Z, the same `SIGPIPE` lines as at 09:23Z |
| 7 | 17 guest soft lockups 12:59–13:04Z, vCPU stalls up to 59 s | **verified** | 17 `watchdog: BUG: soft lockup` lines, 12:59:10Z to 13:04:40Z, 22–59 s; RCU stalls; journald killed by its watchdog (signal 6) at 13:01:46Z, which lost a minute of journal |
| 8 | A memory-heavy GPU job ran and the host was short of Windows commit (8–20 GB of headroom) | **not verified** from any system log (the figures are the job's own notes); the guest shows `dxg` ioctl failures at 13:01:46Z |
| 9 | `UtilAcceptVsock accept4 failed 110` from 13:02:16Z | **verified** | first at 13:02:16.008Z, from a `SessionLeader`; about 200 more until 13:46Z |
| 10 | From 13:03:47Z every new session answered `E_UNEXPECTED`, `--user root --exec` too | **not verified**: the error is printed by `wsl.exe` and no log keeps it. #9 is the guest's side of each failed session |
| 11 | `wsl -l -v` said Running, docker-desktop answered, processes in the VM kept running | **partly**: the guest daemon kept running (journal lines 13:08–13:38Z, its 10 ms timer canary at p50 2 ms). The rest is not verified |
| 12 | `wsl --terminate` reported success and changed nothing; `wsl --shutdown` fixed it | **shutdown verified** (the boot ends at 13:46:40Z; HNS detaches every endpoint at 13:46:40Z); the terminate is not verified |
| 13 | Ruled out: the Claude desktop service (it started at 13:10Z) | **verified** | Service Control Manager 7045 at 13:10:16.230Z; the service's own start line at 13:10:17Z. Noted, not linked: the `IsolationSession` service's start type flipped at 12:59:44Z and back at 13:00:14Z (also at 09:14:03Z); no event ties it to the WSL VM |
| 14 | Unproven: a day-old `wsl.exe` pair running `drop_caches` / `compact_memory` | **not verified**: no log names it |
| N1 | The guest's daemon updated in the same minute | **verified** (missed by the first reading) | guest journal: `rc:agent.update — operator-triggered self-update` at 12:58:28.700Z, `Unpacking roomlerd (0.4.215-1)` at 12:58:34.959Z, the new daemon started at 12:58:45.03Z |
| N2 | The guest daemon's start-up capability probe hung in the kernel | **verified** | `caps probe: cache miss — probing … reason=roomlerd build changed` at 12:58:45.729Z; `the probe child hung and did not die when killed` at 12:59:55.79Z (`last_open=hevc_nvenc:yuv420`, the VM 90 % idle), a second hang at 13:00:55.88Z (`hevc_vulkan:yuv420`), a later try at 13:04:40.47Z with a 120 s limit |
| N3 | Where the kernel was stuck | **verified** | NMI backtrace at 13:01:46.58Z: a `roomlerd` thread at `_hv_pcifront_read_config+0xd5` ← `hv_pcifront_read_config` ← `pci_user_read_config_dword` ← `pci_read_config` ← `sysfs_kf_bin_read` ← `vfs_read`, a user-space read of a PCI device's `config` file, served by the Hyper-V virtual PCI front end. The other stuck CPUs sat mostly in `smp_call_function_single` or `smp_call_function_many_cond`, waiting on another CPU |
| N4 | It is not stuck once: it recurs | **verified** | the guest journal of the same boot shows a probe hang and soft lockups at **every** guest daemon start since 2026-11-11 11:42Z (0.4.210): 27 lockup lines that hour, 23 at 17h, 14 at 01h, 11 at 05h on 11-12, and two `UtilAcceptVsock` failures at 11-11 11h. The boot began on 11-06 and logs no soft lockup before 11-11 11:42Z; the starts of 11-09 and 11-10 logged no probe hang. Only 11-12 13h wedged |
| N5 | The stall did not end | **verified** | the same kernel threads (two `kworker`s, and two `roomlerd` threads that by their timing are the probe children) are in soft-lockup lines at 13:00–13:01Z and again at 13:46:18Z, when the shutdown began |

> **Reading N1–N5.** The adapter churn of claims 3, 5 and 6 is real, and it is identical in the 09:23Z control,
> which wedged nothing. What differs at 12:58Z is the **guest** daemon's own start, whose GPU probe left threads
> stuck in a virtual-PCI config read, on a host whose GPU was busy. That is the leading hypothesis below. It is not
> a proven cause: the link from a stuck vCPU to a session that cannot start (H2) is inferred, and nothing has
> reproduced it yet.

## What the daemon does, mapped against master

### Windows: the update and what it does to the host's network

| Step | What it does to host network state | Anchor |
|---|---|---|
| The worker downloads and verifies the MSI, spawns `msiexec /qn` by full path, and exits | none by itself | `agents/roomlerd/src/updater.rs:1736` (`spawn_installer_with_watch`) → `:1886` (`spawn_installer_inner`) → `:1970` → `:2097`; `msiexec.exe` at `:2114`; `/qn` at `:1817` |
| The exiting worker reverts its MagicDNS split-DNS | removes its NRPT rules (`Remove-DnsClientNrptRule`); logged at 12:58:32.09Z | `crates/tunnel-core/src/overlay/dns.rs:568` and `:584` |
| The exit purge | drops the overlay's split default, peer and subnet routes from its TUN, the exit-node DNS steer and the host ledger's pins; 20 routes at 12:58:32.93Z | `agents/roomlerd/src/lib.rs:327` (`purge_exit_routes`), `:339`, `:360`; `crates/tunnel-core/src/overlay/tun.rs:5736`, `:5785` |
| The TUN's handle closes | **Wintun deletes an adapter its creator closes**, so both adapters vanish (one per org); mirrored networking removes their copies from the VM (Compute 2007 Remove) | `crates/tunnel-core/src/overlay/tun.rs:4184-4186` (the Wintun contract), `:4547` (`Drop`) |
| The MSI replaces the files and registers the service | the service is re-created (SCM 7045 at 12:58:39.48Z); no custom action touches an adapter, a route, DNS or WFP | `agents/roomlerd/wix-perMachine/main.wxs:90` (`MajorUpgrade`), `:138` (`StartRoomlerSvc`), `:361` (`RegisterService`), `:582` (the sequence) |
| The new service host writes the firewall rule before the worker | deletes and re-adds the inbound UDP allow rule (firewall 2052 and 2097 at 12:58:40Z) | `agents/roomlerd/src/win_service/firewall.rs:63` |
| The new worker brings the overlay up | re-creates each adapter with a fixed GUID, sets its profile to Private, its routes, its WFP permits where enabled and its NRPT rules; mirrored networking adds the copies back (Compute 2007 Add) | `crates/tunnel-core/src/overlay/tun.rs:4175` (`up_with`), `:4198` (`device_guid`), `:2919` (`ROOMLER_TUN_GUID`), `:2925` (`org_tun_guid`); `crates/tunnel-core/src/overlay/wfp.rs:104` |

Every adapter removal and creation, every route on the overlay adapters and the profile change is something
mirrored networking copies into the VM. The guest already knows it sees the host's overlay addresses as its own
`eth3` and `eth4` (`crates/tunnel-core/src/overlay/direct.rs:349-358`), and only its LAN-candidate gather acts
on that (`crates/tunnel-core/src/overlay/direct.rs:363`).

### Inside the WSL2 guest: the update and the start-up probe

| Step | What it does | Anchor |
|---|---|---|
| The `.deb` update | `apt-get install` inline, then the daemon exits and systemd starts the new one | `agents/roomlerd/src/updater.rs:2398` |
| The start-up capability probe (cache miss when the build changed) | runs `roomlerd caps-probe` in a child, 60 s per run, and opens each hardware encoder cell. On this host `hevc_nvenc` and `hevc_vulkan` hung | `agents/roomlerd/src/encode/caps.rs:1441` (`run_child`), `:1458-1459`, `:435` (the limit) |
| What a hang earns | "this process only": a later try and **every next start test the cell again**. Within one process, `STUCK` holds later tries back while a child is stuck; a new process starts at zero | `agents/roomlerd/src/encode/caps.rs:797-805`, `:884` (`deny`), `:1874` (`STUCK`) |
| The virtual desktop | starts an Xorg server with the dummy driver; an Xorg thread is among the stuck tasks at 13:46Z (P1 checks whether Xorg's PCI scan reads the same config space) | `agents/roomlerd/src/virtual_desktop.rs:89` |

> ⚠️ **The probe's process boundary contains a crash, not this hang.** `caps.rs` reads a hung child as "the
> daemon is unaffected; this is the process boundary doing its job". On a WSL2 guest the child is stuck in a
> kernel call that holds a CPU of the one VM every distro shares, so the boundary contains nothing: the cost
> lands on the host's WSL as a whole. That sentence, logged at 13:00:55Z and 13:04:40Z, was false on this host.

## Hypotheses, ranked, with the cheapest experiment that tells them apart

All experiments run on a **lab host**: a bare-metal Windows 11 machine with Hyper-V, WSL2 in mirrored networking
and an NVIDIA GPU visible in WSL (GPU paravirtualization does not reach a nested VM). **Never the operator's
laptop.** Each cell runs the session canary (`wsl -d <distro> -e true` every 2 s, with its exit code and latency)
and a guest `dmesg -w` capture, and is repeated until it fails or 20 times.

> ⚠️ On a guest where this can happen, `lspci`, `cat /sys/bus/pci/devices/*/config` and anything else that reads
> PCI config space **is the trigger**. They run on the lab host only, inside an experiment.

| Rank | Hypothesis | Evidence for | Evidence against or missing | Cheapest discriminating experiment |
|---|---|---|---|---|
| H1 | The guest daemon's start-up GPU probe reads the GPU's virtual-PCI config space; the read stalls in `hv_pcifront_read_config`, and the stuck CPU stalls the others (soft lockups VM-wide) | N2, N3, N4: the trace, and a hang with lockups at every guest start since 11-11 | Lockups happened five times and wedged once, so H1 alone is not enough to wedge | **E2**: restart the guest daemon with its caps cache removed, GPU idle, 20 times; count lockups and canary failures. **E6**: under the same conditions, a loop of `cat /sys/bus/pci/devices/<gpu>/config` with no Roomler at all |
| H2 | H1 plus a busy GPU (and possibly low Windows commit) keeps the stalled read from ever returning; the stuck kernel work then blocks new hvsocket connections (each is a new VMBus channel the guest kernel must set up), so `SessionLeader` times out and `wsl.exe` reports `E_UNEXPECTED` | N5 (stuck from 13:01Z to 13:46Z), #9; the earlier hangs were on a host without a GPU job (not verified) | The VMBus channel-setup link is inferred from the kernel's design, not from a trace | **E3**: E2 while a GPU load holds most of the VRAM. **E4**: E3 plus a host-side allocation that leaves under 10 GB of commit headroom. The canary failing in E3 or E4 and not in E2 confirms the load dependence |
| H3 | The Windows daemon's adapter churn (two vNICs hot-removed and hot-added) contributes when it overlaps H1 | #5, #6; the guest's `hv_netvsc … eth3: Ring buffer not empty after closing rndis` at 13:00:46Z on the re-added mirror | The identical churn at 09:23Z (#3) was harmless | **E1**: 20 Windows-only MSI updates (or service restarts) with the canary; then **E5**, the field's combination: a Windows MSI update and a guest `.deb` update in the same minute, under E3's load |
| H4 | Host memory pressure from the GPU job alone | the job ran from mid-morning and its own guard stopped it three times (its notes, not verified) | no lockup and no session failure in any of its earlier hours; lockups appear only at probe times (N4) | **E0** (control): the GPU load and the commit pressure of E4 with no Roomler event for 30 minutes |
| H5 | The stuck `wsl.exe` loop of `drop_caches` and `compact_memory` | none | not verified to exist; a day old with no earlier failure | rerun E4 with and without such a loop |

**E7** names the caller in P1: `strace -f -e trace=openat,read` on the probe child, or a `pci_read_config` kprobe,
gives the library and call that reads the config file (the CUDA or NVENC user-mode driver, libva, the Vulkan
loader or `dzn`, or Xorg's PCI scan).

## Design (decided after P1)

The fix is not chosen before P1 names the caller. The candidates, each behind a kill switch:

1. **A hang on WSL2 is kept for the kernel boot.** A cell that hung under the current `boot_id` is not opened
   again by a later try or by the next start until the guest's kernel restarts, the driver changes or the
   operator asks. Today the policy re-tests it every time (`agents/roomlerd/src/encode/caps.rs:797-805`), and each re-test adds a stuck
   child.
2. **On a WSL2 guest the start-up probe never issues the read that stalls.** Depending on E7: skip the cells
   whose open reads virtual-PCI config space, or open them only when no other process holds the GPU.
3. **The Windows side, only if E1 or E5 implicate it:** the adapters survive an update or a restart on a host
   with a mirrored WSL2 VM, so the VM sees no churn.
4. **Diagnostics either way:** a guest daemon says when it runs on WSL2 and which probe cells it skipped or found
   hung; the Windows daemon's update report says when a mirrored WSL2 VM was running.

A proposed switch for 1 and 2 is `ROOMLERD_CAPS_WSL_GPU_PROBE` (name settled in P2); `1` restores today's probe.

## Phases

| Phase | What | Kill switch | Status |
|---|---|---|---|
| P0 | Reproduce on a lab host: E0 to E5. The canary must fail on the current release in some cell, or the FR records every cell tried | none (no code) | not started; waits on D4 |
| P1 | Isolate: E6 and E7; name the kernel path and the user-space caller; settle D1 and D2 | none (lab builds only) | not started |
| P2 | Fix behind a kill switch, per D1 and D2 | `ROOMLERD_CAPS_WSL_GPU_PROBE` (proposed); a second one for the Windows side if D2 needs it | not started |
| P3 | Diagnostics and docs (a WSL2 page or section with a lifecycle diagram, linked from `docs/README.md`) | n/a | not started |
| P4 | Field-verify on the lab host, then on the fleet's WSL2 guests and Windows hosts that run WSL2 | the P2 switches | not started |

## Acceptance criteria

- [ ] **AC1** P0 is recorded in the field log: the cell that wedges session creation on the current release, with
  the canary failing first, or every cell tried when nothing reproduces.
- [ ] **AC2** The trigger is named with a trace, not an inference: the kernel path and the user-space library and
  call that issue the read.
- [ ] **AC3** After the fix, on the lab host, 20 guest-daemon updates under P0's wedging conditions leave the
  canary at 0 failures and the guest kernel log with no soft lockup in a `roomlerd` thread or its children.
- [ ] **AC4** 20 Windows-daemon MSI updates on a mirrored-WSL2 lab host leave the canary at 0 failures, and when
  P1 shows the adapter churn contributes, the fix covers it.
- [ ] **AC5** The kill switch restores today's behaviour, shown on the lab host.
- [ ] **AC6** A guest daemon never re-opens a cell that hung within the same kernel boot: a unit test, and the lab.
- [ ] **AC7** Field: after the roll, every enrolled WSL2 guest (the denominator from the server, not an exec sweep)
  shows no probe hang at its update, and one update of the incident laptop's two daemons with a GPU job running
  wedges nothing, run only with the operator's go.
- [ ] **AC8** Docs updated or created for what this FR built, in house style (a mermaid lifecycle diagram, tables,
  `file:line` anchors), with a row in `docs/README.md`.

## Open decisions

| # | Question | Options | Recommendation |
|---|---|---|---|
| D1 | What a WSL2 guest's probe does about GPU cells | (a) skip GPU cells on WSL2, software encode only; (b) a hang latch per kernel boot; (c) probe only an idle GPU; (d) skip only the cells whose open reads config space | (b) at once, then (d) once E7 names the caller; (a) only if (d) is not possible |
| D2 | Does the Windows daemon keep its adapters across an update on a host with a mirrored WSL2 VM | (a) no change; (b) keep them | (a) unless E1 or E5 fails |
| D3 | Should the server stagger an update of two daemons on one physical machine | (a) no; (b) yes, as a defence | (a): a stagger would hide the trigger, not remove it |
| D4 | Which lab host runs P0 | a bare-metal Windows 11 machine with an NVIDIA GPU, WSL2 mirrored, and a guest daemon enrolled in a test org | **the operator's call** |

## Out of scope

- A defect in WSL, Hyper-V or the GPU driver itself. If E6 reproduces it with no Roomler process, it goes to the
  vendor, and this FR still makes the daemon stop issuing the read.
- NAT-mode WSL2, unless P0 shows the wedge there too (then it is in).
- Two agents sharing one machine's loopback (FR-121) and the GPU job's own memory guard.

## Field log

| # | Date (UTC) | Where | What |
|---|---|---|---|
| 1 | 2026-11-12 | a Windows 11 dev laptop: WSL2 mirrored, the Windows daemon (per-Machine MSI) and a guest daemon (`.deb`) | Both daemons updated to 0.4.215 at 12:58:28Z on one "update all". The guest's start-up probe hung in `hv_pcifront_read_config` (`hevc_nvenc`, then `hevc_vulkan`); 17 soft lockups 12:59–13:04Z; new sessions failed from 13:02:16Z (`SessionLeader` accept4 110) until `wsl --shutdown` at 13:46:40Z. The 09:23Z Windows-only update made the same adapter churn and wedged nothing. The probe had hung, with lockups, at every guest start since 2026-11-11 11:42Z. Re-verified read-only from the event logs and the guest journal (the table above) |
