# Functional Requirements

Every substantial feature or multi-step program gets an **FR**: a spec in this directory
and a tracking issue in [`gjovanov/roomler-ai/issues`](https://github.com/gjovanov/roomler-ai/issues),
modeled on [`gjovanov/lgr#21`](https://github.com/gjovanov/lgr/issues/21). The convention
itself lives in `CLAUDE.md` (`## Functional Requirements (FR) workflow`) so that it loads
in every session; **this file is the number registry.**

## Claiming a number

Add your row to the registry below **in the same commit as your spec file**, then push.

That is the whole protocol — and the reason it is one shared table rather than "scan
`docs/fr/` for the highest N" is that scanning **cannot** work, with or without a
post-create re-check. Two sessions both read `max = N`, both write `FR-N+1`, and git merges
them without a murmur because they touched *different* files. Not hypothetical: the
convention collided **five times on its first day**, the closest pair seven seconds apart —
and the last three happened *after* a scan-and-re-verify rule was already in force — the
fifth even after this ledger existed (see the row-is-the-claim warning below).

| collided | resolution |
|---|---|
| `FR-1` (#767, 09:55:42) vs `FR-01` (#768, 09:57:20) | #768 → FR-9 |
| `FR-3` (#773, 10:04:58) vs `FR-3` (#774, 10:05:05) | → FR-6 / FR-5 |
| `FR-5` — three sessions within minutes | → FR-4 / FR-5 / FR-6 |
| `FR-7` (#778, 10:07:14) vs `FR-7` (#780, 10:09:41) | #780 → FR-8 |
| `FR-10` (#783, 11:45:34) vs `FR-10` (#784, 11:47:18) | #784 → FR-11 |
| `FR-24` (#838, licensing) vs `FR-24` (#840, viewer) | #840 → FR-26 |

The tie-break when one slips through anyway is **deterministic, and it is not "the younger
one"**: the **LOWER issue number keeps `FR-N`; the HIGHER renumbers** to the next free N —
title, spec filename, ledger row and in-body references together. Never renumber *into* a
vacated number, and numbers already settled stay settled. That is repair, though — not
allocation.

⚠️ The retracted "younger claim renumbers (ambiguous ⇒ the retrospective one)" rule is why
the `FR-3` collision needed a *third* commit (`7ad2ca0b`): both sessions applied it, both
concluded they were the younger, and they renumbered **past each other** into a shared
`FR-5`. Timestamps seconds apart are not an ordering, and "the retrospective one" was true
of both at different moments. Issue ids are server-assigned and monotonic, so two sessions
that never talk compute the same winner (`3af6761d`, #779).

Editing **one** file makes git the arbiter instead: the second push is rejected as
non-fast-forward, and the rebase shows the number is already taken *before* anything is
published. Same shape as the overlay block allocator, where a unique index arbitrates
concurrent slot claims rather than a lock.

⚠️ Claim **immediately before** you write, never at the start of a long session — the same
discipline as the release-tag race.

⚠️ A number is never reused, including by a withdrawn or superseded FR. Mark the row
instead: a reader who meets `FR-4` in an old commit message must land on the right document.

⚠️ **A row that is still in an open PR is not a claim either — only master arbitrates.**
The sixth collision (`FR-24`, 2026-08-28) had *both* sessions do the protocol right: each
wrote its row in the same commit as its spec. But #838's row sat on an unmerged branch
(#844) while #840's merged, so at no point did a push get rejected — git arbitrates at
merge, and one side had not reached it yet. A number is therefore only yours once the row
is **on master**; park the claiming commit in a long-lived PR and someone can take it from
under you, with the repair falling due later and costing more (here: a rename across a
shipped spec, a ledger row, an issue title and ten code comments). Land the claim first,
then take your time with the implementation.

⚠️ **The row IS the claim — a spec file alone is not.** `FR-10`'s spec reached master on
2026-08-27 (`2366859d`) without a row here, so this table still read `max = FR-9` and the
next session claimed `FR-10` 104 s later (#783 vs #784) — the fifth collision, and the first
one the registry existed to prevent and didn't. Pushing the spec without the row re-opens
exactly the hole the row closes.

## Registry

| FR | Issue | Title | Status |
|---|---|---|---|
| [FR-1](FR-1-remote-desktop-drag-smoothness.md) | [#767](https://github.com/gjovanov/roomler-ai/issues/767) | RustDesk-parity remote-desktop drag smoothness | in progress — P1–P5 shipped + field-verified (0.4.4 "Rozalina works nicely"), P6 + P7-HUD open |
| [FR-2](FR-2-pin-update-downgrade-guard.md) | [#770](https://github.com/gjovanov/roomler-ai/issues/770) | Pin-update downgrade guard | closed — shipped (#771) + field-verified on prod 2026-08-27 |
| ~~FR-3~~ | — | *vacated* — the #773/#774 collision; both claimants renumbered | never reuse |
| [FR-4](FR-4-conference-media-path-integrity.md) | [#776](https://github.com/gjovanov/roomler-ai/issues/776) | Conference media-path integrity | closed — shipped + field-verified (retrospective) |
| [FR-5](FR-5-macos-unattended-update-chain.md) | [#774](https://github.com/gjovanov/roomler-ai/issues/774) | macOS unattended update chain | closed — field-closed at agent rc.482 (retrospective) |
| [FR-6](FR-6-ci-release-build-speed-slo.md) | [#773](https://github.com/gjovanov/roomler-ai/issues/773) | CI + release build-speed SLO — every lane ≤10 min warm | shipped + field-verified (retrospective) |
| [FR-7](FR-7-signed-releases.md) | [#778](https://github.com/gjovanov/roomler-ai/issues/778) | Signed releases — Windows Authenticode, Linux GPG + provenance, macOS identity | in progress — retrospective; all criteria field-verified except Apple Developer ID (enrolment 5XS5WN8R99 under review); closes on `spctl … Notarized Developer ID` |
| [FR-8](FR-8-claude-session-restore.md) | [#780](https://github.com/gjovanov/roomler-ai/issues/780) | Claude session restore after reboot (`crestore`) | closed — shipped (retrospective); renumbered from FR-7, which #778 claimed 147 s earlier |
| [FR-9](FR-9-lan-pairs-converge-direct.md) | [#768](https://github.com/gjovanov/roomler-ai/issues/768) | Two nodes sharing a LAN converge to a direct carrier | **closed** — all criteria met + field-verified; shipped rc.480 → 0.4.8 (#741 #744 #746 #747 #758 #765 #782) |
| [FR-10](FR-10-relay-drag-quality.md) | [#783](https://github.com/gjovanov/roomler-ai/issues/783) | Relay drag quality — IDR thrift on constrained transports | closed — shipped in 0.4.5, field PASS 2026-08-27 ("CORPLAP-3 from neo16 very smooth"); CORPLAP-1 residual attributed to DERP RTT; kept FR-10 by the lower-issue-id rule vs #784 |
| [FR-11](FR-11-server-grids-members-files-invites.md) | [#784](https://github.com/gjovanov/roomler-ai/issues/784) | Server-side grids — members/files/invites, devices online-first default sort, mesh display names | closed — shipped (#790) + field-verified on prod 2026-08-27; renumbered from `FR-10`, which #783 claimed 104 s earlier |
| [FR-12](FR-12-onboarding-tutorial.md) | [#788](https://github.com/gjovanov/roomler-ai/issues/788) | Onboarding tutorial — guided welcome tour, callable anytime | P1 shipped (#797); P2 spotlight + P3 extra art planned |
| [FR-13](FR-13-rc-keyboard-hid-mapping-mac-chords.md) | [#789](https://github.com/gjovanov/roomler-ai/issues/789) | RC keyboard: unmapped HID usages type garbage (CapsLock→`9`); mac Ctrl→Cmd chords | implemented — viewer half ships with the next web deploy, agent half with the next agent release; field pending |
| [FR-14](FR-14-direct-link-jitter-episodes.md) | [#792](https://github.com/gjovanov/roomler-ai/issues/792) | Direct-link jitter episodes — AIMD sawtooth on VPN-churning links | design — evidence collected (CORPLAP-1 2026-08-27); child of FR-1 |
| [FR-15](FR-15-relay-age-feedback.md) | [#795](https://github.com/gjovanov/roomler-ai/issues/795) | Relay age feedback — close the rate loop with the viewer's own clock | shipped in 0.4.9 (#796); **P2 shipped in 0.4.11 (#804)** — the floor is now bounded by half the viewer own probe RTT, so a clock-biased sample is rejected rather than learned; field gate pending |
| [FR-16](FR-16-rc-quality-benchmark.md) | [#798](https://github.com/gjovanov/roomler-ai/issues/798) | Systematic remote-desktop quality benchmark — the codec/path/device matrix | proposed — hand-testing cannot compare cells (drag speed is itself a first-order input); child of FR-1 |
| [FR-17](FR-17-partial-reliability-video.md) | [#799](https://github.com/gjovanov/roomler-ai/issues/799) | Video rides a reliable+ordered DataChannel — HOL blocking costs SECONDS on a relay | **stages A + B shipped** (#820, #834) — per-chunk framing negotiated via `AgentCaps.video`, then `{ordered:false, maxRetransmits:0}` behind an opt-in. Both INERT until an agent advertising `chunk-framing` rolls; "constrained-only" proved un-implementable (ordering is fixed before ICE) and is recorded as a deviation. Stage C is blocked on a reorder buffer, not on the harness. `send_wait_max_ms` measured at 10 263 ms on a healthy agent; child of FR-1 |
| [FR-18](FR-18-carrier-queue-discipline.md) | [#801](https://github.com/gjovanov/roomler-ai/issues/801) | Carrier queue discipline — the relay path holds seconds of video before the wire | in progress — LAN-relay test came back NEGATIVE (both corp VPNs block LAN on the endpoint), so relaying is structural for those hosts and the queue is the whole lever |
| [FR-19](FR-19-peer-relays.md) | [#805](https://github.com/gjovanov/roomler-ai/issues/805) | Peer relays — tenant-owned UDP relay nodes between direct and DERP | proposed — independently reviewed on 3 lenses pre-publication; carrier is a third `RelayKind` (not a new tier), bind is key-authenticated, **E2E-3 RUN: port settled at 3478** (clk reaches 3478 and nothing else); P0 wire forward-compat SHIPPED (#811). P1 next |
| [FR-21](FR-21-retire-obsolete-binary-names.md) | [#809](https://github.com/gjovanov/roomler-ai/issues/809) | Retire the obsolete `roomler-agent` / `roomler-agent-tray` names — code, docs, file/folder names, all three OSes | design — 1 765 occurrences across 233 files on `fa364b12`; the deliverable is a classifier + CI guard, because the hard part is the freeze list (7 dual-read anchors, incl. the TCC-keyed macOS bundle) |
| [FR-22](FR-22-time-to-first-frame.md) | [#819](https://github.com/gjovanov/roomler-ai/issues/819) | Time-to-first-frame — connecting sometimes takes 10-15 s | **parts 1 + 3 shipped** (#821) — phase-aware bound (`requesting` 4 s vs ICE's 15 s) cuts the bad case ~18 s to ~7 s, plus eight-mark connect timing so "sometimes 10-15 s" can become a distribution. Part 2 was found ALREADY IMPLEMENTED and recorded, not rebuilt. **3b (#822)** puts the verdict in a SNACKBAR — the console is shut during the sessions that stall, so console-only reporting could never yield a root cause. Root cause still open |
| [FR-23](FR-23-company-identity-and-domain-consolidation.md) | [#827](https://github.com/gjovanov/roomler-ai/issues/827) | Company identity on the site, and one product on both domains | in progress — phases 0–3, **5 and 6 shipped + field-verified**; phase 4 (teardown) **skipped** by the operator, since `janus`/`coturn` on roomler.live still depend on that namespace. #828 #830 #831 #835 #837 #841. Imprint live (`grep imprint` on the shipped bundle 0→1); `roomler.live` now **301s to roomler.ai** (apex only — the domain hosts live unrelated services); Terms + Privacy rewritten to cover all three pillars. ⚠️ **Three false claims were found in legal copy** — the Privacy Policy said the session JWT lives in `localStorage` (untrue since #680), the imprint cited the **repealed** EU ODR platform, and I wrote an absolute security guarantee that has a documented exception: *legal copy invites template drafting; check every claim against code*. ⚠️ `ROOMLER__APP__CORS_ORIGINS` cannot be set from a configmap at all (no `list_separator` ⇒ **boot crash**), which is why the 301 beat serving the app. Open: send Apple the correction. Unblocks the Apple half of FR-7 |
| FR-20 | [#807](https://github.com/gjovanov/roomler-ai/issues/807) | Per-tenant relay cost metering | in progress — row backfilled by FR-25 |
| FR-24 | [#838](https://github.com/gjovanov/roomler-ai/issues/838) | Licensing split — AGPL-3.0 control plane, MPL-2.0 everything the customer runs | in progress — **row reserved here by the collision repair**; the spec (`FR-24-licensing-split.md`) and this row's full text land with #844. #838 keeps the number as the lower issue id; the viewer FR that had merged into this slot moved to FR-26 |
| [FR-25](FR-25-call-layout-mentions-pip.md) | [#839](https://github.com/gjovanov/roomler-ai/issues/839) | Call UX: layout controls that honour their labels, mentions, PiP, spotlight + fullscreen | shipped #842 + **#859**, live on prod `v20260829-ae6bfa254495`; mentions criterion closed by an **e2e A/B** (fails on both pre-#859 images, passes on #859). ⚠️ **#842 did not fix the crash** — it grouped five copies of prosemirror-model into one chunk instead of removing them, and "2 chunks → 1 chunk" was a placement proxy read as an instance count; the real cause was nested installs and the lever is `resolve.dedupe`. Remaining criteria (PiP, the three layout controls, spotlight/fullscreen) need a real call |
| [FR-26](FR-26-rc-viewer-settings-and-metrics.md) | [#840](https://github.com/gjovanov/roomler-ai/issues/840) | Remote-desktop viewer: display name, quality-metric toggles, FSR default, settings reorg | shipped (#845), live on prod `v20260828-f98ca7c0b356` (the served `RemoteControl` chunk carries `roomler-rc-metrics`); **renumbered from FR-24** by the lower-issue-id rule vs #838, whose claiming row was parked in an unmerged PR |
| [FR-27](FR-27-consent-surfaces-and-desktop-companion.md) | [#854](https://github.com/gjovanov/roomler-ai/issues/854) | Host consent — every mode, every OS, and a desktop companion that works | design — 15 defects catalogued on `eabc332e`; the picker has never had an effect for a device's OWNER (`resolve_session_authz` returns `Auto` before the policy is read) |
| [FR-28](FR-28-derp-drain-on-pod-roll.md) | [#865](https://github.com/gjovanov/roomler-ai/issues/865) | A pod roll freezes every relay-carried session — the agent learns the /derp WS died by failing to send on it | P0 implemented — measured 2436 ms delivery gap + decode stall on an IDLE session; chain confirmed on 2 hosts against a 69 s-old pod. The `DerpCancelRegistry` primitive already existed and was simply not wired to SIGTERM |
| [FR-29](FR-29-x11-damage-capture.md) | [#864](https://github.com/gjovanov/roomler-ai/issues/864) | X11 capture reads the whole screen even when nothing changed — the ~27 fps Linux ceiling | P1 field-verified — idle CPU **45.8 %→3.4 %** on `scw-m2-asahi`; motion deliberately unchanged (that is P3). ⚠️ **Renumbered from FR-28**: #865 landed its ledger row on master first, and the claim protocol makes the ledger the arbiter — "the rebase shows the number is taken before anything is published" |
