# FR-123: The daemon compiles in parallel — `roomlerd` split into sibling crates

**Issue:** [#2941](https://github.com/gjovanov/roomler-ai/issues/2941) · **Status:** P2 in flight 2026-10-27 —
spec, claim, the Phase 0 measurements and two probes on the release runners; the operator decided A as P2 and
holds P1 (#2944, a draft) for a re-measurement on top of A · **Owner:** the agent's build shape (with FR-6 for the lane and
FR-90 P8 for Hive's crate) · **Anchors:** master `12a0edc81` (2026-10-26) unless a row says otherwise ·
**Parent:** [FR-6](FR-6-ci-release-build-speed-slo.md) (#773), whose residual lever this is

## 1. Goal

A release build of the daemon stops being one serial, single-core compile. Today `agents/roomlerd` is one
crate — a lib of 355 files and a 6,945-line `main.rs` — built at `codegen-units = 1`
(`.github/workflows/release-agent.yml:117`), so the last six to thirteen minutes of every MSI and `.pkg`
job are two units that run one after the other, each on one core, while the runner's other cores idle.
The goal is a crate graph in which the daemon's independent parts compile **beside each other**, the
hub that wires them compiles **while** they finish, and the binary that ships is the same binary: same
features, same behaviour, same size within one per cent.

This is FR-6's last open lever. Everything cheaper was measured and rejected there: more codegen units
cost +14 % gzip size on a 22 MB budget whose ceiling is 26; a bigger runner cannot help a one-core
compile; the version bump is not why our crates rebuild. Only crates that compile in parallel are
size-neutral.

## 2. Evidence

### 2.1 Where a release spends its time (FR-6, 2026-10-25/26)

`cargo --timings` from FR-6's wave-29 probes, dependencies warm, `codegen-units = 1`
(`docs/fr/FR-6-ci-release-build-speed-slo.md`, log of 2026-10-25):

| Runner | run | `roomlerd` lib | `roomlerd` bin (`main.rs`) | whole build | lib + bin share |
|---|---|---|---|---|---|
| Windows (`windows-latest`, 2 cores / 4 threads) | 40228482936 | 322 s | 172 s | 565 s | 87 % |
| macOS (`macos-15`, 3 vCPU) | 40230929108 | 477 s | 197 s | 766 s | 88 % |
| macOS, the release build in the same log | — | 564 s | 229 s | 858 s | 92 % |

The two units run one after the other: the bin waits for the lib's object code, and each is a single
crate. On 0.4.173's rehearsal (run 40249389321) the MSI job took 16.1 min and the `.pkg` 16.3; a tag
now reuses its rehearsal's compile (FR-6 wave 29, #2914), but every rehearsal pays the full compile,
and our crates rebuild on every release because the version bump changes them.

**Codegen units were measured and rejected** (FR-6 log, 2026-10-25/26; our crates only, dependencies
at 1):

| `codegen-units` | Windows build | lib + bin | `roomlerd.exe` | gzip -9 | macOS build | macOS gzip |
|---|---|---|---|---|---|---|
| 1 (shipped) | 565 s | 322 + 172 | 64,755,712 | 22,479,787 | 766 s | — |
| 4 | 426 s (−25 %) | 231 + 122 | +16.5 % | +13.7 % | 494 s (−35 %) | +14.2 % |
| 16 | 463 s | 250 + 140 | +23.8 % | +19.5 % | 439 s | +23.7 % |

The MSI budget (`release-agent.yml:2088-2154`) is a 22.00 MB baseline, a warning at 24 and a hard
ceiling at 26; today's MSI is 22.18 MB. At 4 units it would land near 25.2 MB, at 16 near 26.6. So
the lever is structural, not a flag.

### 2.2 The crate, measured (Phase 0, 2026-10-26, master `d016f81d2`)

**Size.** `agents/roomlerd/src`: 356 `.rs` files, 325,526 lines, 13.7 MB. FR-24 counted the same crate
at 89,737 lines on 2026-08-28 (`FR-24-licensing-split.md` §"Crate classification"): ×3.6 in eight weeks,
and the trend is FR-90, FR-97, FR-119 and their successors.

**Lines per top-level module** (directories recursive; `cargo` compiles all of it in one unit):

| Module | Files | Lines | Release builds | Notes |
|---|---|---|---|---|
| `hive/` | 62 | 63,839 | yes (`hive`) | FR-90; its P8 moves it (§3.6) |
| `encode/` | 40 | 38,210 | yes | FR-119 / FR-118 hot |
| `capture/` | 26 | 22,824 | yes | FR-119 / FR-97 hot |
| `peer.rs` | 1 | 15,261 | yes | hub: reaches 31 modules |
| `recording/` | 21 | 14,965 | yes (`recording` ⊂ `full`) | |
| `vault_proxy/` | 26 | 11,750 | **no** (`vault` is not in any release feature set) | |
| `input/` | 14 | 10,347 | yes | |
| `win_service/` | 16 | 10,088 | Windows | |
| `signaling.rs` | 1 | 8,217 | yes | hub: reaches 49 modules |
| `tunnel/` | 11 | 8,195 | yes | out-degree 1 (§3.4) |
| `keep_busy/` | 14 | 7,277 | yes | |
| `main.rs` (the bin) | 1 | 6,945 | yes | uses 60 lib modules |
| `updater.rs` | 1 | 6,831 | yes | |
| `system_context/` | 11 | 5,441 | Windows (`system-context`) | |
| `ssh.rs` | 1 | 5,160 | yes (`overlay-*`) | |
| `files.rs` | 1 | 5,115 | yes | |
| `indicator/` | 5 | 4,711 | yes | |
| `exec.rs` | 1 | 3,755 | yes | in-degree 13 |
| `apps/` | 4 | 3,426 | yes | |
| `localapi_state.rs` | 1 | 3,325 | yes | hub: reaches 26 modules |
| the other 72 modules | 97 | ~50,900 | mostly | 56 of them under 1,000 lines |

The release feature sets are `full,hive,vp9-444,vp9-444-bindgen,overlay-l3,overlay-netstack,ffmpeg-encoder,ssh-server,viewer-indicator-x11`
on Linux (`release-agent.yml:537`, `:1159`), `full-hw,hive,vp9-444,system-context,ffmpeg-encoder,overlay-l3,overlay-netstack,ssh-server`
on Windows (`:1645`) and `full,hive,vp9-444,vp9-444-bindgen,overlay-l3,overlay-netstack,ssh-server,ffmpeg-encoder,viewer-indicator-macos`
on macOS (`:2723`). `vault` is in none of them, so `vault_proxy/` and the `vault`-gated halves cost a
release nothing.

**The module graph** (every `crate::X` reference between top-level modules, comments stripped;
`scratchpad/fr-roomlerd-split/modgraph.pl`): the crate is a hub and spokes.

```mermaid
flowchart BT
    subgraph hubs["the hubs — reach everything"]
        main["main.rs 6.9k<br/>60 modules"]
        sig["signaling.rs 8.2k<br/>out 49"]
        peer["peer.rs 15.3k<br/>out 31"]
        lapi["localapi_state.rs 3.3k<br/>out 26"]
    end
    subgraph media["the media plane (~115k lines)"]
        enc["encode/ 38k"]
        cap["capture/ 23k"]
        inp["input/ 10k"]
        kb["keep_busy/ 7k"]
        rec["recording/ 15k"]
        ind["indicator/ 4.7k"]
        aud["audio/ 2k · display_match · presence · clipboard · …"]
    end
    subgraph hive["Hive (64k) — FR-90 P8"]
        hv["hive/ · hive_win · vault · vault_proxy"]
    end
    tun["tunnel/ 8.2k<br/>out 1"]
    subgraph host["host plumbing"]
        ws["win_service/ 10k"]
        ex["exec.rs 3.8k (in 13)"]
        comp["companion 3.5k"]
        upd["updater.rs 6.8k"]
        misc["power · win_identity · win_token · supervision · watchdog · consent · …"]
    end
    enc --> cap
    cap --> inp
    inp --> cap
    inp --> kb
    kb --> inp
    rec --> cap & enc & aud
    ind --> misc
    enc -. "6 flags in caps.rs" .-> hv
    cap -. "3 session lookups" .-> comp
    hv --> tun & ex
    tun -. "2 fns" .-> misc
    peer --> enc & cap & inp
    sig --> hv & tun & enc & ex
    lapi --> tun & rec
    main --> sig & peer & lapi & ws & comp & upd
```

What the edges say:

- **The hubs are the hubs.** `signaling.rs`, `peer.rs`, `localapi_state.rs` and `main.rs` together name
  every other module. They stay in the crate that depends on all the rest.
- **The media plane is a cycle of three plus everything that hangs off it.** `capture ↔ input ↔
  keep_busy` reference each other (16, 10, 10, 5, 6 and 1 references); `encode` reaches `capture` 20
  times; `recording` reaches `capture`, `audio` and `encode`. Its edges *out* of the plane are few and
  small, listed in §3.5.
- **`tunnel/` is one edge from a leaf.** Its only references outward are two functions in
  `tunnel/netwatch.rs` (`:43`, `:57`) that read a registry `overlay.rs` owns
  (`DERP_TUNNEL_MUXES`, `agents/roomlerd/src/overlay.rs:117-160`); everything else it needs is
  `roomler_node_core::config` (re-exported as `crate::config`) and `tunnel_core`. Four modules depend
  on it (`signaling.rs`, `localapi_state.rs`, `ssh.rs`, `hive/`), all above it.
- **Hive reaches down** into `vault` (69), `hive_win` (36), `vault_proxy` (35), `exec` (20), `tunnel`
  (11) and `ssh_client` (9); the hubs reach into Hive (`signaling.rs` 48 references). FR-90 P8 already
  plans its boundary as traits (`FR-90-hive-agent-sessions.md` §3i).

**Churn**, because a file move lands on whoever has it open. Commits touching each module in the last
14 and 30 days (to 2026-10-26), and the open PRs:

| Module | 14 d | 30 d | Open PRs touching it |
|---|---|---|---|
| `signaling.rs` | 80 | 112 | #1642 |
| `hive/` | 72 | 115 | — |
| `encode/` | 55 | 65 | #2932, #900 (draft, Aug.) |
| `main.rs` | 52 | 78 | #2932, #2924 |
| `peer.rs` | 50 | 62 | — |
| `capture/` | 46 | 57 | — |
| `vault_proxy/` | 21 | 21 | — |
| `input/` | 18 | 26 | — |
| `localapi_state.rs` | 15 | 25 | #2924, #2912 |
| `updater.rs` | 12 | 19 | — |
| `recording/` | 10 | 24 | — |
| `win_service/` | 8 | 18 | — |
| `companion` | 7 | 7 | #2912, #1642 |
| `tunnel/` | 6 | 13 | — (`client_mgr.rs` 4 / 9; FR-90 P4-6-1b will touch it) |
| `keep_busy/` | 3 | 16 | — |

291 commits touched `agents/roomlerd/src` in 14 days; 11 PRs are open, 5 of them in the daemon.

**Doc anchors**, because a moved file moves every `file:line` claim about it: 1,400 anchors into
`agents/roomlerd/src/*` across `docs/`, `ui/docs/`, `CLAUDE.md` and `.github/`. Per module: `hive/`
573, `encode/` 72, `signaling.rs` 61, `vault_proxy/` 54, `capture/` 53, `input/` 52, `peer.rs` 45,
`vault.rs` 31, `tunnel/` 31, `display_match.rs` 31, `main.rs` 27 (62 `main.rs:N` forms in 33 files),
`updater.rs` 22, `win_service/` 21.

### 2.3 What cargo can and cannot overlap

Three facts bound the design; the first two are cargo's, the third is the lane's.

1. **An rlib's dependents start at its metadata.** rustc writes the `.rmeta` when the front end is
   done and keeps running LLVM; cargo starts every dependent *rlib* at that moment (pipelining). At
   `codegen-units = 1` the LLVM half is one thread, so a dependent's whole compile can run beside it on
   the idle cores.
2. **A binary waits for objects.** cargo's `requires_upstream_objects` is true for every `bin` target:
   the bin's rustc does not start until each upstream rlib is fully written. That is why the timings
   show `main.rs` strictly after the lib, and why "move `main.rs` into the lib" buys no overlap by
   itself — it collapses two serial units into one. Only code in an **rlib** below a trivial bin can
   overlap a predecessor's codegen.
3. **The runners have three or four threads** (Windows: an EPYC 7763 at 2 cores / 4 threads, FR-6
   log 2026-10-06; `macos-15`: 3 vCPU). Parallelism saturates at three or four sibling crates of
   comparable size; twenty small crates buy nothing more than four large ones and cost a metadata
   round each.

4. **An `async fn`'s body is emitted where its future is polled, not where it is written.** rustc
   treats closures and coroutine bodies as always cross-crate-inlinable — the same rule as
   `#[inline]` — so a lib's `pub async fn run()` is codegen'd in the crate that polls it, and so is
   every `async fn` it awaits, transitively. The probe (§2.4) measured what that means for the
   daemon: the bin's front end takes **2 s**, its codegen **165 s**, and its 3.19 M lines of LLVM IR
   are 84 % instantiations (492 copies of tokio's task harness, 95 of `ServerMsg`'s deserializer,
   `drop_in_place` glue) around the daemon's own main loops —
   `signaling::handle_server_msg::{{closure}}`, `signaling::connect_once::{{closure}}`,
   `peer::media_pump_ffmpeg_dc::{{closure}}`, `OverlayRuntime::run::{{closure}}` are among its
   largest functions — emitted in `main.rs`'s unit because `run_cmd` polls them. Two consequences:
   - a sibling crate keeps only the codegen of what is polled **inside** it. The media plane's
     capture backends and encoders run on threads (sync), so their cost moves with their files; the
     tunnel plane's `client_mgr` spawns its own tasks, so most of its cost moves too; an `async fn`
     the hub awaits — `acceptor::handle_forward_request` from `signaling.rs` — stays the hub's
     wherever its file lives. Lines of code are therefore a poor proxy for a sibling's share, and
     every phase is measured, never estimated from `wc -l`;
   - the bin and the lib **instantiate the same generics twice**, and LLVM optimises each copy. The
     v2 probe (§2.4, same runner) measured what one unit saves: `main.rs` moved into the lib took
     lib + bin from 610 to 516 s on Windows (**−15 %**) and from 702 to 548 s on macOS (**−22 %**),
     and the binary shrank 5.5 % and 11.5 %. The first draft of this section predicted "moves the
     work, does not remove it" from the v1 rows, which came from two runners; the same-runner rows
     retired that prediction.
5. **Every crate boundary costs binary size, in the same coin.** The mirror image of fact 4's
   second point: two crates that use the same generic with the same types each emit a copy, and
   without LTO nothing merges them. P1 (§3.4) moved 8k lines into a sibling and the release binary
   grew **+3.4 % on Windows, +3.1 % on macOS** (gzip +3.1 %, +3.2 %), both OSes, same runner. A
   sibling is therefore not size-neutral by construction; it is paid for in the MSI budget, and
   AC2 is the gate that says how much (±1 %). FR-6's P3e measured thin LTO at +5 % size on the
   rc.363 tree and fat LTO worse, so LTO is not the obvious answer either (decision 7).

So the shape that pays is narrower than the first draft drew: a short graph whose boundaries are
**few and cold** — crossed by sync, non-generic calls, so little is instantiated twice — not many
siblings. And the two largest levers are the ones facts 2, 4 and 5 together expose, which pull in
opposite directions on size:
- **one unit**: the former bin inside the lib — −15 % / −22 % time and −5.5 % / −11.5 % size, one
  file moved, no package change (§3.6, option A);
- **the overlap**: the daemon's async tree, 165–220 s of codegen, in an **rlib** below a three-line
  bin, starting at the lib's metadata and finishing inside the lib's 240–413 s codegen window —
  about −35 % time on Windows and −31 % on macOS, at today's size (§3.6, option B).

### 2.4 The Phase 0 probe (run 40303391151, branch `exp/fr-roomlerd-split-probe`, 2026-10-26)

A temporary push-triggered workflow on the real release runners, the lane's seeds restored
read-only (never saved, never stamped; `SEED_REUSE_SAME_TREE=off`), the lane's own feature set per OS.
Two variants on each of Windows and macOS: `baseline` (the tree as released) and `collapse`
(`main.rs` moved into the lib as `roomlerd::app` behind a three-line bin, to measure whether the
bin's time is its own code or the lib's generics instantiated again). Both record `--timings`, a
release-profile `cargo check` (front-end-only times) and the binary's size; Windows also runs
`cargo llvm-lines` on the lib and the bin and aggregates it per module.

**What it measured** (all four jobs green; artifacts `probe-{windows,macos}-{baseline,collapse}`):

| Runner · variant | roomlerd lib: front end + codegen | roomlerd bin | lib + bin | release-profile `cargo check`: lib / bin | binary (gzip -9) |
|---|---|---|---|---|---|
| Windows · baseline | 58.3 + 240.0 = **298.2 s** | **167.0 s** (starts the second the lib ends) | 465.2 s | 34.1 s / 2.0 s | 64,855,040 (22,503,191) |
| Windows · collapse | 95.6 + 554.4 = 650.0 s | 2.6 s | 652.6 s | 54.3 s (lib, with the former bin inside) | 61,256,704 (21,341,558), **−5.5 % (−5.2 %)** |
| macOS · baseline | 90.2 + 511.4 = **601.6 s** | **250.1 s** | 851.7 s | 34.3 s / — | 54,840,320 (21,962,603) |
| macOS · collapse | 72.1 + 634.9 = 707.0 s | 4.7 s | 711.7 s | 52.3 s | 48,543,856 (19,619,415), **−11.5 % (−10.7 %)** |

- The bin is the daemon's async tree (§2.3 fact 4): its front end is 2 s of 167. Its `llvm-lines`:
  3,190,408 IR lines, 16 % under a `roomlerd::` path, 84 % instantiations; the lib's: 6,290,620
  lines, 26 % under `roomlerd::`, the rest tokio (22.7 %), core, alloc, serde, `remote_control`,
  `tunnel_core`, `localapi`, `mp4_atom`, `russh`, `webrtc` instantiated for it. Per module, the
  attributable part of the lib is `hive` 5.8 %, `peer` 3.4 %, `encode` 2.2 %, `recording` 1.9 %,
  `tunnel` 1.2 %, `win_service` 0.9 %, `ssh` 0.8 %, `input` 0.7 % — and 74 % that no module name
  can be hung on, which is why §2.3 ends with "measured, never estimated".
- Pipelining is live for lib → lib edges: the lib unit starts at 76.2 s, the second `roomler-cli`'s
  metadata is out (its unit starts at 71.2 s with a 5.0 s front end). The bin starts at 374.4 s,
  the second the lib's codegen ends: fact 2, observed.
- **The four jobs ran on four runners, and Windows hosts differ by ~1.4×** (the `collapse` job's
  dependency phase took 103 s where the baseline's took 76 s; its release-profile check 54 s
  against 34 + 2). The collapse's time is therefore unchanged within that noise, as fact 4
  predicts, while its size effect is consistent across both OSes. The v2 probe (run 40308429566)
  builds base, collapse and P1 **back to back on one runner per OS**, dependencies warm after the
  first build.

**v2, the same-runner rows** (run 40308429566, 2026-10-26; Windows job 121523246197, macOS job
121523245998; the Windows runner is a slower host than v1's baseline runner, which is the point of
measuring on one machine). "Critical path" is from the first of our units to start to the bin's end:

| Runner · variant | sibling | roomlerd lib: front end + codegen | bin | critical path | Δ time | binary (gzip -9) | Δ size |
|---|---|---|---|---|---|---|---|
| Windows · base | — | 77.7 + 310.9 = 388.6 s | 221.4 s | **610.0 s** | — | 64,855,040 (22,503,190) | — |
| Windows · collapse | — | 65.1 + 449.0 = 514.0 s | 2.0 s | **516.0 s** | **−15.4 %** | 61,256,704 (21,341,558) | **−5.5 % (−5.2 %)** |
| Windows · P1 | `roomler-node-tunnel` 3.1 + 35.7 = 38.7 s, beside the lib | 54.5 + 288.0 = 342.6 s, starting 3.8 s in at the sibling's metadata | 222.5 s | **568.9 s** | **−6.7 %** | 67,061,760 (23,206,233) | **+3.4 % (+3.1 %)** |
| macOS · base | — | 69.9 + 413.4 = 483.4 s | 218.4 s | **701.8 s** | — | 54,840,320 (21,962,603) | — |
| macOS · collapse | — | 94.8 + 450.0 = 544.8 s | 2.9 s | **547.7 s** | **−22.0 %** | 48,543,856 (19,619,415) | **−11.5 % (−10.7 %)** |
| macOS · P1 | 4.0 + 30.1 = 34.1 s, beside the lib | 53.3 + 352.8 = 406.0 s, starting 5.7 s in | 201.9 s | **613.6 s** | **−12.6 %** | 56,558,880 (22,656,007) | **+3.1 % (+3.2 %)** |

- **One unit is cheaper than two** (fact 4): the collapse took 94 s off Windows and 154 s off
  macOS, and 3.6 MB / 6.3 MB off the binary. The bin's work did not merely move: the part the two
  units had been doing twice vanished.
- **P1 shortened the hub by more than the sibling's own time**: the lib lost 46 s on Windows and
  77 s on macOS while `roomler-node-tunnel` took 39 s and 34 s beside it — the generics the tunnel
  plane drags (`tunnel_core`'s QUIC and transports, tokio) were a larger share of the lib than the
  1.2 % its own `roomlerd::tunnel` names suggested. The bin is unchanged, as fact 4 says it must be.
- **P1 grew the binary 3.1–3.4 %** (fact 5): the generics both sides instantiate, emitted twice.
  AC2 allows ±1 %; the MSI would move from 22.18 MB toward 22.9 (the budget warns at 24, stops at
  26). Decision 7 is the operator's.
- Pipelining did its part: the lib's front end started 3.8 s / 5.7 s in, the moment the sibling's
  metadata was out, and ran beside the sibling's codegen.

## 3. Key design

### 3.1 The target shape

```mermaid
flowchart BT
    subgraph L0["L0 — exists today"]
        nc["roomler-node-core<br/>crates/agent-core"]
        tc["roomler-ai-tunnel-core"]
        rc["roomler-ai-remote-control"]
        hn["roomler-hive-node"]
    end
    subgraph L1["L1 — siblings, no edges between them, compiled in parallel"]
        media["roomler-node-media<br/>capture · encode · input · keep_busy · audio · display_match · …"]
        tunnel["roomler-node-tunnel<br/>tunnel/ (P1)"]
        hive["Hive's crate<br/>FR-90 P8-1"]
        hostc["roomler-node-host (P3)<br/>power · win_identity · win_token · supervision · …"]
    end
    subgraph L2["L2 — the hub: package roomlerd, lib"]
        hub["peer · signaling · localapi_state · ssh · files · updater · win_service · companion · … · app (the former main.rs)"]
    end
    bin["L3 — package roomlerd, bin: three lines"]
    media --> nc & tc & rc
    tunnel --> nc & tc & rc
    hive --> hn & nc & tc
    hostc --> nc
    hub --> media & tunnel & hive & hostc
    bin --> hub
```

- **Package `roomlerd` keeps its name, its directory, its lib and its bin.** `cargo build -p roomlerd`,
  `cargo deb`, `cargo wix -p roomlerd`, `build.rs` (`hive_host`, the `/STACK` link argument, the
  Windows version resource — `agents/roomlerd/build.rs:36-88`), the `.deb` asset map and the MSI
  mapping are untouched by every phase. The lane's four build lines never change. What moves is
  *below* the package: sibling crates it depends on.
- **Every `crate::X` path inside `roomlerd` keeps resolving.** An extracted module is re-exported at
  its old place (`pub use roomler_node_tunnel as tunnel;` where `lib.rs:236` has `pub mod tunnel;`),
  exactly as FR-21's P3e did for `roomler-node-core` (`agents/roomlerd/src/lib.rs:16-19`). The
  integration tests (`crates/tests`, 160 `roomlerd::` paths) and the wizard (`agents/roomler-setup`,
  which links the lib for `updater`, `enrollment`, `install_detect`) compile unchanged.
- **The hub overlaps the siblings.** Its front end starts when the slowest sibling's metadata is out;
  its LLVM half runs after its own front end. With the former `main.rs` inside the hub (P3) the bin is a
  shell, and the whole of today's second serial unit rides inside the overlap.
- **Hive's crate is FR-90's.** P8-1 moves the session-facing machinery behind `SessionEgress` and
  `ControlLink` into `crates/hive-node` or a sibling (`FR-90-hive-agent-sessions.md` §3i). This FR asks
  one property of it: the crate depends on L0 and the seams, never on `roomlerd`'s hub, so it compiles
  beside `roomler-node-media`. Its 64k lines are the largest single sibling; nothing here moves them.

### 3.2 The rules every extraction follows

1. **Behaviour is identical.** No runtime switch exists because nothing at runtime changes: the same
   functions, the same `cfg`s, the same features, linked into the same binary. The proof per PR is
   every CI lane green on every release feature set, the binary's size within ±1 % (AC2) and the next
   release's fleet check (AC3). The kill switch of a phase is its revert, which is why each phase is
   one PR.
2. **A moved file's body stays byte-identical.** Only its `use` header changes, and `crate::config::`
   becomes `roomler_node_core::config::` on the same line. Then every doc anchor into it shifts by a
   constant per file, `anchor-xcheck.pl` proves each by content, and a PR that has the old file open
   rebases across a rename git detects.
3. **Features mirror.** A sibling declares the features its code gates on (`overlay-l3`,
   `overlay-netstack`, `recording`, …) and `roomlerd`'s feature of the same name enables it
   (`overlay-l3 = ["roomler-ai-tunnel-core/overlay-l3", "roomler-node-core/overlay-l3", "roomler-node-tunnel/overlay-l3"]`),
   so the lane's feature sets mean what they meant. `build.rs`'s `cfg(hive_host)` stays `roomlerd`'s.
4. **Boundaries are inverted, never widened.** A back-edge from a sibling into the hub (the
   `tunnel → overlay` registry; `encode::caps` reading Hive's flags) becomes a registry or a value the
   hub hands *down*, never a dependency of the sibling on the hub. §3.4 and §3.5 list each one.
5. **`#[inline]` where the boundary is hot.** Without LTO a non-generic function is not inlined across
   crates. A per-frame or per-packet call that crosses a new boundary (the capture → encode hand-off,
   the DC send path) gets `#[inline]` or stays on one side; the review of each PR names the hot calls
   it checked. The tunnel plane's boundary is per-flow, not per-packet (its data path is inside
   `tunnel_core`), so P1 has none.
6. **A new crate joins the tables on its first commit.** `CLIENT_PATHS` and `CLIENT_CRATES` in
   `scripts/licence-classes.sh` (`:45`, `:100`), an MPL-2.0 SPDX header on every file, `cargo clippy
   --workspace` (`.github/workflows/ci.yml:285`, which compiles every member), the "remaining
   crates" unit-test step (`ci.yml:496`) and whichever feature-gated clippy or test line its features
   need. A crate CI does not name has decorative tests (#1626).
7. **Versions stay `version.workspace = true`.** A pinned version alone would not keep a crate cached
   across releases: FR-6 proved our crates rebuild because a checkout makes every file newer than the
   seed's dep-info, and the lane now reuses a whole stamped tree instead. Per-crate reuse is open
   decision 2, not a rule.

### 3.3 Naming and place

Daemon-side shared crates live under `crates/` as `roomler-node-*` since FR-21 D3 (`roomler-node-core`
at `crates/agent-core`) and FR-90 P0a (`roomler-hive-node` at `crates/hive-node`). The siblings follow:
`crates/node-tunnel` → `roomler-node-tunnel` (lib `roomler_node_tunnel`), `crates/node-media` →
`roomler-node-media`, `crates/node-host` → `roomler-node-host`. Open decision 1 asks the operator to
confirm the names before P2; P1 ships with `roomler-node-tunnel` and a rename is one commit while it
is the only sibling.

### 3.4 P1 — `roomler-node-tunnel`, the pilot

The smallest extraction with a one-edge boundary and no open PR, chosen to prove the recipe (rules 1–7)
where a mistake costs little: 11 files, 8,195 lines, 31 doc anchors, 6 commits in 14 days.

| Today | After P1 |
|---|---|
| `agents/roomlerd/src/tunnel/{mod,acceptor,client_mgr,dialer,netwatch,peer,port_holder,quic_peer,quic_setup,reap,route_reconciler}.rs` (`tunnel/mod.rs:28-46`) | `crates/node-tunnel/src/{lib,…}.rs`, bodies byte-identical; `mod.rs` becomes `lib.rs` |
| `lib.rs:236` `pub mod tunnel;` | `pub use roomler_node_tunnel as tunnel;` |
| `tunnel/mod.rs:32` `pub use roomler_node_core::acl;` | the same line in the new `lib.rs`, so `crate::tunnel::acl::…` still resolves |
| `tunnel/netwatch.rs:43`, `:57` call `crate::overlay::{primary_,}derp_tunnel_handle` | the registry moves into the crate: `DERP_TUNNEL_MUXES`, `register_derp_tunnel_mux`, `derp_tunnel_handle`, `primary_derp_tunnel_handle` (`overlay.rs:117-160`) become `roomler_node_tunnel::netwatch::*`; `overlay.rs` registers into it. `PRIMARY_TENANT_ID` stays in `overlay.rs` and is passed in |
| `pub(crate) mod netwatch` and its `pub(crate)` functions, used by `signaling.rs` and `hive/supervisor/stream.rs` | `pub` |
| `crate::config::{load, save, WriteLock, PRIMARY_ORG_LABEL, test_fixture}` (12 sites) | `roomler_node_core::config::…`, same lines; tests take `roomler-node-core` with `test-fixtures` as a dev-dependency |
| features: `overlay-l3`, `overlay-netstack` gates inside `tunnel/` | the crate declares both, forwarding to `tunnel-core` and `node-core`; `roomlerd` forwards its own |

Consumers above the crate are unchanged: `signaling.rs` (31 `crate::tunnel::` sites), `localapi_state.rs`
(17), `ssh.rs` (4), `hive/` (9). The CI lane adds `-p roomler-node-tunnel` to the remaining-crates step;
`cargo clippy -p roomlerd --features overlay-l3` (`ci.yml:401`) compiles it with its features through
the forwarding.

**Measured** (§2.4, v2, same runner): the critical path fell **−6.7 % on Windows (610 → 569 s) and
−12.6 % on macOS (702 → 614 s)** — the hub lost more than the sibling took, because the tunnel plane's
generics left with it — and the release binary **grew +3.4 % / +3.1 %** (gzip +3.1 % / +3.2 %),
which AC2's ±1 % does not allow. The PR (#2944) is green on every lane, including the integration
lane dispatched on the branch, and is parked as a draft until decision 7 says whether the size is
paid, the boundary is made colder, or the extraction waits for an LTO re-measurement. The recipe —
the manifest, the licence tables, the CI lanes, the registry inversion, the anchor pass — is proven
either way and is what P2 reuses.

### 3.5 P2 — `roomler-node-media`, the plane that pays

The media plane is ~115k lines, 35–40 % of the release compile, and FR-119's and FR-118's working
area. Its boundary, measured on master, is a short list of small edges, each with its inversion:

| Edge out of the plane | Sites | Inversion |
|---|---|---|
| `encode::caps` reads Hive's switches (`adopt_enabled`, `replica_enabled`, `archive_enabled`, `stream_enabled`) | `encode/caps.rs:2114-2143` | `caps::detect` takes a `HostWords` value the hub fills from Hive, `files::is_remote_browse_enabled` (`:1961`), `rc_local_turn::enabled` (`:2041`) and `apps` (`:1916-1926`, `:3285`) |
| `capture` asks `companion` which graphical session is live | `capture/mod.rs:2163`, `capture/portal/mod.rs:1316-1318` | the session lookup moves to a leaf both can see (`roomler-node-host`, P3) or is passed in by the pump's caller |
| `capture`, `input` ask `system_context::worker_role` and `desktop_rebind` | `capture/mod.rs:2807-2810`, `input/system_context_backend.rs:84`, `:380`, `input/sas.rs:194`, `input/mod.rs:576` | `worker_role` (pure: SYSTEM / worker / user) goes to the host leaf; the SystemContext capture pump is a capture backend and moves with `capture/` |
| `capture::scrap_backend` takes `power::DisplayAwake` | `capture/scrap_backend.rs:143` | `power` goes to the host leaf |
| `keep_busy` reads `win_identity`, `lock_state`, `presence` | `keep_busy/state.rs:135-248`, `keep_busy/host_win.rs:28`, `:67` | `win_identity`, `win_token` to the host leaf; `lock_state`/`lock_on_end`/`presence` move with the plane |
| `indicator` owns `rc_sessions::RcSessionRegistry` and calls `companion::ensure_running` | `indicator/mod.rs:100-606`, `indicator/notify.rs:212`, `:526` | `rc_sessions` moves with the plane (it is the indicator's registry; `peer.rs` reads it from above); the companion start becomes a callback the hub installs |
| `encode` names `peer::TargetResolution` | `encode/resample.rs:43`, `encode/plan.rs:645`, `encode/policy.rs:28`, `encode/sim.rs:2958` | the type moves into the plane and `peer.rs` re-exports it |
| `recording` reaches `consent`, `supervision`, `companion`, `signaling::COMPANION_START_BUDGET` | `recording/remote.rs:711-734`, `:1266-1300`; `recording/manager.rs:479`; `recording/launch.rs:439`, `:1494`, `:1587` | `recording/` **stays in the hub** (17 out-edges, the most entangled of the group); it depends on the plane from above |

What moves: `capture/`, `encode/`, `input/`, `keep_busy/`, `audio/`, `audio_source.rs`,
`display_match.rs`, `presence.rs`, `virtual_desktop.rs`, `desktop_brief.rs`, `clipboard.rs`,
`media_share.rs`, `indicator/`, `rc_sessions.rs`, `lock_state.rs`, `lock_on_end.rs`,
`lock_overlay.rs`, `host_mute.rs`, `gpu_clock.rs`, `fp16.rs`, `clock.rs`, `win32_monitors.rs`,
`dpi.rs`, `apps/` — about 115k lines and ~290 anchors. Its features: `scrap-capture`,
`openh264-encoder`, `mf-encoder`, `wgc-capture`, `vp9-444(-bindgen)`, `enigo-input`, `clipboard`,
`viewer-indicator(-x11|-macos)`, `audio`, `drm-capture`, `portal-capture`, `uinput-input`,
`ffmpeg-encoder`, `synthetic-frame-source`, `system-context` — mirrored (rule 3). The
`encoder-cells` skill's invariants are untouched: the probe child, the cache, the denylist and the
cell matrix move as files.

⚠️ **P2 needs a merge window.** It lands only when FR-118/FR-119 have no PR open against `capture/`
or `encode/`, prepared on a branch so that its rebase, CI and merge fit inside a few hours. The main
session schedules it; this FR does not open it unasked.

### 3.6 P2 — the former bin: one unit, or an rlib below a three-line bin

The probe moved this ahead of the media plane. The bin's 167–221 s (Windows) / 218–250 s (macOS) is
the daemon's async tree, emitted where `run_cmd` polls it (§2.3 fact 4), and a bin unit cannot start
before the lib's objects exist (fact 2). Two ways to spend that fact, measured and projected on the
v2 runners (§2.4):

| | **A — one unit**: `main.rs` → `src/app.rs`, `pub mod app`, the bin three lines | **B — the overlap**: the same code as an rlib the bin depends on |
|---|---|---|
| time, Windows | **516 s, −15 % (measured)** | ≈ the lib alone, about 390 s, −36 % (projected: the tree's 221 s fits inside the lib's 311 s codegen window) |
| time, macOS | **548 s, −22 % (measured)** | ≈ 483 s, −31 % (projected) |
| binary | **−5.5 % Windows, −11.5 % macOS (measured)**: the duplicated generics vanish | today's: two units still instantiate the same generics twice |
| packages | none change; `roomlerd` keeps its lib and bin | the lib and the bin must sit in different packages (a package's bin may not depend on a crate that depends on the package's lib) |
| the lane | untouched | layout (ii) below leaves it untouched; (i) changes four build lines |
| what else | `main.rs`'s tests (`agents/roomlerd/src/app.rs:6682` after the rename) start running under `--lib`; 59 anchors in 30 docs take the new file name at the same numbers (FR-28's two `main.rs:N` are the API's and stay) | the same, plus the layout's CI edits |

**A first, then B if more time is needed.** A is one moved file, no package change, and the only
option that improves both axes — it buys 1.2 MB of MSI headroom on Windows against a 22.00 MB
baseline that has been creeping. B is the larger time win but gives that headroom back, and it is
`ship-it` territory. The two are exclusive (B is two units by definition), so B replaces A if the
operator wants the time more than the size. **Decided 2026-10-27 (the operator): A is P2.**

How A is cut, so that it is the measured shape and nothing else:

- **The bin is one call, and the poll stays in the lib.** `src/bin/roomlerd.rs` is
  `fn main() -> anyhow::Result<()> { roomlerd::app::main() }`. `app::main` — both of its `cfg`
  arms, the AppKit hand-off on macOS included — builds the runtime and `block_on`s `daemon_main`
  *inside the lib*. A bin that kept the runtime and polled a lib future would put the whole async
  tree back into a bin unit (fact 4), which is the 167–221 s this phase removes. Nothing else is
  bin-only: the crate has no `#![…]` attributes, no global allocator, and `build.rs`'s link
  arguments key on the bin's *name*.
- **A pure rename, so git carries the others' edits.** `git mv src/main.rs src/app.rs`; the body
  changes on seven lines (two `pub fn main`, one reworded comment of equal length), so every anchor
  into it keeps its number. The old path must *vanish* for git to pair the rename — a merge never
  break-detects a rewritten file — so the bin is `src/bin/roomlerd.rs`, not a new `main.rs`. The
  file keeps its `roomlerd::` spelling (219 sites) behind `extern crate self as roomlerd;` in
  `lib.rs`, so an edit in flight on the old file still applies *and compiles* when its branch
  rebases. The `crate::` respelling is a later, quiet commit once the open PRs have landed.
- **Its tests run for the first time.** `app::tests` (`agents/roomlerd/src/app.rs:6682`) — the CLI
  parses the wizard's snippets rely on, the config-path ladder, the self-heal predicate, the
  retired-spelling guard — ran in no lane while they were a bin's; under `--lib` the Linux, Windows
  and macOS unit steps now run them. They are pure (clap parses, pure predicates, a process-env
  round trip through `tunnel_core::env::test_env`) and touch no service.

For B, an rlib between the lib and the bin needs the two in different packages. Two layouts, both
leaving the lane's build lines, `cargo deb`, `cargo wix -p roomlerd`, `build.rs` and the packaging
directories exactly where they are:

| | (i) the bin leaves | (ii) the lib leaves, files in place |
|---|---|---|
| what moves | `main.rs` → a new package's `src/lib.rs` (`pub fn main`) + a three-line `src/main.rs`; `build.rs`'s bin parts (the `/STACK` link argument, the version resource) with it | `main.rs` → `agents/roomlerd/src/app.rs`, package `roomlerd`'s lib (`[lib] path = "src/app.rs"`), its bin three lines; the daemon lib becomes package `roomler-daemon` at `crates/daemon/Cargo.toml` with `[lib] path = "../../agents/roomlerd/src/lib.rs"` — 355 files stay where every anchor and every open PR has them |
| the release lane | `cargo build -p <new package>` on four lines; `cargo deb` / `cargo wix -p roomlerd` keep their package, which then has no bin: cargo-deb's explicit asset list should carry it, cargo-wix's `create` is unproven and the probe must build an MSI first | unchanged: `-p roomlerd` is still the package with the binary, the deb and wix metadata, `build.rs` and `packaging/` |
| features | the new package forwards the lane's feature strings | package `roomlerd` forwards them to `roomler-daemon`; `cfg(hive_host)` is emitted by both build scripts |
| CI | the `--bins` check lines and `installer-smoke.yml` / `macos-mesh-test.yml` name the new package | every `cargo test -p roomlerd --lib …` line (about 40 in `ci.yml`) names `roomler-daemon`; `crates/tests` and the wizard either depend on it or keep `roomlerd` through `pub use roomler_daemon::*` in the app lib |
| what fails loudly | an MSI that will not build, at rehearsal time | a CI test line, at PR time |

For B, recommended: **(ii)**, because the release lane is byte-for-byte untouched and the risk
moves to CI lines that fail at PR time. It touches `ship-it` territory either way (a second package
in the graph the lane builds), so it waits for the main session's and the operator's go, and its
probe run builds the MSI and the `.pkg` on the exp branch before the PR opens.

### 3.7 P3 — the media plane; P4 — the host leaf; Hive

- The media plane (§3.5) follows P2, in its merge window, measured sibling by sibling: with the
  async tree out of the serial tail, the hub's remaining codegen is the sync bulk, and the plane's
  capture backends and encoders are the largest sync part of it.
- `roomler-node-host`: the OS-session and process plumbing the plane and the hub both read —
  `power`, `win_identity`, `win_token`, `instance_lock`, `supervision`, `watchdog`,
  `system_context::worker_role`, the companion's session lookup, `arch`, `mem_trim`, `clock`-class
  leaves. Small (~12k lines) and needed by P3's inversions; it may ship before P3 if P3's window is
  late.
- Hive: FR-90 P8-1, coordinated on #1827.

### 3.8 What the design refuses

- **No `codegen-units` or LTO change**, in any crate: FR-6 measured both (§2.1; P3e: thin LTO +5 %
  size, fat worse).
- **No crate split along platform lines.** A `roomlerd-windows` crate would not shorten the Windows
  job, which is the pole.
- **No renaming of the package or the binary**, and no second package producing `roomlerd`:
  `cargo wix -p roomlerd`, the `.deb` metadata and the six MSI version copies are couplings the
  `ship-it` skill lists as fleet-freezing when they drift.
- **No feature re-shaping.** The lane's feature strings are a compatibility surface (a build without
  `hive` advertises no `hive` verb); every sibling mirrors, nothing is renamed.
- **No moves of `hive/`, `vault*`, `hive_win.rs`.** FR-90 owns them (P8).

## 4. Phases

Each phase is one PR, reverted as one; its kill switch is that revert, because no phase changes
behaviour. Every phase is measured the same way: the temporary probe workflow builds the PR's tree
and its base tree on the **same runner, back to back**, with the lane's seeds restored read-only and
`--timings` on — FR-6's method, whose repeat-to-repeat noise was ±1 s on one machine against a
±10 % spread across runners.

| Phase | What | Measured by | Status |
|---|---|---|---|
| **P0** | this spec, the claim, the Phase 0 measurements (§2.2) and the probes (§2.4, §8) | runs 40303391151 (v1), 40308429566 (v2) | spec PR #2942; v1 measured 2026-10-26, v2 in flight |
| **P1** | `roomler-node-tunnel` (§3.4): the crate, the registry inversion, the licence tables, the CI lanes, the anchor pass | the v2 probe's same-runner A/B, Windows + macOS; then **re-measured on top of P2** (base vs A vs A + P1, one runner per OS) | PR #2944, every lane green, **measured −6.7 % / −12.6 % time and +3.4 % / +3.1 % size**; **held** (decision 7, 2026-10-27): a draft until A lands, then re-measured on top of A |
| **P2** | the former bin as one unit (§3.6, option A, decided 2026-10-27): `src/main.rs` → `src/app.rs`, `pub mod app`, the bin one call; its tests run for the first time; the anchor pass | same-runner A/B against today's base, after 0.4.178 has published (the probe waits out the release's queue) | **PR #2999** open; the probe owed |
| **P3** | `roomler-node-media` (§3.5), in a merge window FR-118/119 agree to | same-runner A/B, sibling by sibling | after P2 |
| **P4** | `roomler-node-host` (§3.7) | same-runner A/B | after P3 (or before, if P3's window is late) |
| **P5** | Hive's crate — FR-90 P8-1, with the dependency property of §3.1 | FR-90's | FR-90's schedule |
| **P6** | docs: the crate layout in `docs/architecture.md` (workspace map + a daemon section with §3.2's rules), `CLAUDE.md`'s Architecture tree, a row in `docs/README.md`'s index, this spec's status | — | the map and the tree carry P1 since #2944; the section and the row with P2 |

## 5. Acceptance criteria

- [ ] **AC1 — the release compiles shorter.** The rehearsal's MSI build step and `.pkg` build step are
  each ≥ 30 % shorter than 0.4.173's rehearsal (13.8 min and 18.2 min for the whole build step on
  0.4.171's; the daemon's lib + bin 494 s on Windows, 674–793 s on macOS) on three consecutive
  releases, and each phase's same-runner A/B (§4) shows its own share.
- [ ] **AC2 — the binary is the same size.** `roomlerd.exe`, the macOS `roomlerd`, the `.deb`'s
  `roomlerd` and both MSIs are within ±1 % of the release before each phase (the MSI size step's
  `::notice::` lines; the `.deb`/`.pkg` asset sizes on the release).
- [ ] **AC3 — no behaviour change.** Every CI lane green on every PR, including the `--features full`
  clippy step, the Windows and macOS lanes and the integration lane dispatched on the branch; then one
  real release carrying each phase installs on the fleet through the normal update, and the post-roll
  check passes: version on the online agents, a remote-desktop session, an overlay pair, a tunnel
  forward.
- [ ] **AC4 — the anchors hold.** For every PR: `anchor-xcheck.pl` reports no MISMATCH over all of
  `docs/` and `ui/docs/`, `scripts/anchor-range-audit.sh` reports no fault, dated field-log rows keep
  their anchors, and moved files' bodies are byte-identical to their base (rule 2).
- [ ] **AC5 — a new crate is a full citizen.** MPL-2.0 header on every file; `CLIENT_PATHS` and
  `CLIENT_CRATES`; `licensing.yml` green; a CI step names its unit tests and runs them (the step's log
  shows the binary ran); `cargo clippy --workspace -- -D warnings` and the feature-gated clippy lines
  green on Linux, Windows and macOS.
- [ ] **AC6 — the shape is parallel.** On the release runner, `cargo build --timings` of the daemon
  shows at least two of our crates compiling concurrently during the daemon's own compile, and the
  hub's unit starting before the slowest sibling's unit ends.
- [ ] **AC7 — docs.** `docs/architecture.md` (the workspace map with the siblings; a daemon
  crate-layout section with the rules of §3.2), `CLAUDE.md`'s Architecture tree, and a row for the
  layout doc in `docs/README.md`'s index, before the close.

## 6. Open decisions

1. **Names and place** (the operator): `crates/node-{tunnel,media,host}` as `roomler-node-*` (§3.3),
   or `agents/roomlerd-*` beside the daemon. P1 ships with the former; a rename is one commit while
   it is alone.
2. **Per-crate reuse across releases.** A sibling whose sources did not change between two releases
   still rebuilds today for two reasons: its `version.workspace = true` changes its package id, and a
   fresh checkout makes its files newer than the seed's dep-info (FR-6, 2026-10-06). Pinning a
   sibling's version removes the first; only the lane's seed logic (FR-6's) could remove the second,
   by restoring per-crate mtimes against recorded blob hashes. Not pursued here; measured only if FR-6
   wants it.
3. **`recording/`: hub or plane.** The design keeps it in the hub (17 out-edges, consent and
   supervision among them); if P3's host leaf absorbs `consent` and `supervision`, it can follow the
   plane down in a later PR.
4. **The SystemContext plumbing's home** (Windows, `system_context/` 5.4k, `win_service/` 10k): the
   capture pump belongs to the plane, the winlogon-token spawn to the hub, `worker_role` to the host
   leaf. The split inside `system_context/` is P3's to draw.
5. **P2's shape** — **decided 2026-10-27 (the operator): option A**, one unit, `main.rs` into
   the lib. Option B (the rlib overlap) and its layout question — (ii) the daemon lib as
   `roomler-daemon` with its 355 files in place and package `roomlerd` keeping the binary, the deb
   and wix metadata, `build.rs` and `packaging/`, the release lane untouched; or (i) the bin into a
   package of its own, which changes the lane's four build lines and must first prove `cargo wix`
   and `cargo deb` on a package without a bin — stay on the table only if more time is wanted
   later, and would replace A.
6. **Hive's crate** — FR-90's decisions 63–65; this FR asks only for the dependency property.
7. **P1's size cost** — **decided 2026-10-27 (the operator): hold P1.** #2944 stays a draft until
   A lands; then P1 is re-measured **on top of A** (base vs A vs A + P1, one runner per OS, the
   release runners) and reported. The operator expects the combination to come out smaller than
   today's binary; if it does not, the report says so. The alternatives stay recorded: *pay it*
   (−6.7 % / −12.6 % of the release compile for +3.4 % / +3.1 % of binary, §2.4; the MSI toward
   22.9 MB, and every later sibling pays again); *cool the boundary* (intersect `cargo llvm-lines`
   of both crates and move or de-generify what both instantiate); or an LTO re-measurement on
   today's tree (FR-6 P3e's +5 % thin-LTO finding is from the rc.363 tree, seven months and ×3.6
   lines ago).

## 7. Out of scope

- Any change to `codegen-units`, LTO, `opt-level`, `panic` or `strip` (FR-6 §Open decisions).
- The seed, stamp and reuse mechanics of `release-agent.yml` (FR-6 wave 29 and after).
- Moving `hive/`, `vault.rs`, `vault_proxy/`, `hive_win.rs` (FR-90 P8).
- Renaming the `roomlerd` package, binary, `.deb` or MSI; a second package that ships a binary.
- Any behaviour, feature or wire change. A phase that needs one is not this FR's.
- The server crates and `crates/tests`' own build time.
- The wizard's link against the daemon lib (`agents/roomler-setup/Cargo.toml:50`): freeing it is a
  side benefit of a later `roomler-node-install` leaf, noted, not planned.

## 8. Field-verification log

| When | What | Result |
|---|---|---|
| 2026-10-26 09:30Z | Phase 0, static (master `d016f81d2`): lines, graph, churn, anchors (§2.2) | 356 files / 325,526 lines; hubs `signaling` (out 49), `peer` (31), `localapi_state` (26), `main` (60 modules); `tunnel/` out-degree 1; media plane ~115k lines with 7 small boundary edges; 1,400 doc anchors |
| 2026-10-26 09:56Z | Phase 0 probe v1: run 40303391151 on `exp/fr-roomlerd-split-probe` (Windows + macOS × baseline, collapse, each on its own runner) | all four green. Windows baseline: lib 298.2 s (58.3 front end + 240.0 codegen), bin 167.0 s starting the second the lib ends, bin `cargo check` 2.0 s; macOS baseline: lib 601.6, bin 250.1. `llvm-lines`: bin 3.19 M IR lines, 84 % instantiations, the daemon's main loops among its largest functions. Collapse: binary −5.5 % / −11.5 %; its time unreadable across runners (§2.4) |
| 2026-10-26 10:34Z | Probe v2: run 40308429566, base → collapse → P1 **back to back on one runner per OS**, dependencies warm after the first build | Windows: 610.0 → 516.0 s (collapse, −15.4 %) / 568.9 s (P1, −6.7 %); macOS: 701.8 → 547.7 (−22.0 %) / 613.6 (−12.6 %). Size: collapse −5.5 % / −11.5 %, P1 **+3.4 % / +3.1 %**. Table in §2.4 |
| 2026-10-26 11:35Z | P1 PR #2944: every CI lane green (23 pass, 1 skip), the integration lane dispatched on the branch green (run 40308764338); locally on Windows: 72 tests on both feature sets, clippy clean on three feature sets, `fmt`, SPDX, name audit, registry and range audits | **parked as a draft on the size finding** (decision 7); the recipe is proven |
| 2026-10-27 12:40Z | The operator decided 5 and 7: A is P2; P1 is held and re-measured on top of A | recorded in §3.6, §4, §6 |
| 2026-10-27 | P2-A cut on master `208e42385` (0.4.178): `git mv src/main.rs src/app.rs` (7 lines changed in 6,987, every anchor keeps its number), the bin `src/bin/roomlerd.rs` one call, `extern crate self as roomlerd;`, 59 anchors re-pathed in 30 docs plus the two pinned inverted ranges' baseline lines; `app::tests` run locally for the first time (see the PR) | **PR #2999** open (17 `app::tests` pass on both feature sets, the Windows-line lib suite 2,487 pass); the same-runner probe (base vs A vs A + P1) waits for 0.4.178 to publish |
