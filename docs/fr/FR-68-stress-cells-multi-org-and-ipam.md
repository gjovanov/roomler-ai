# FR-68: Stress cells for multi-org guard contention and IPAM growth

**Status**: proposed · **Owner**: overlay/networking · **Issue**: TBD

## Goal

Turn two overlay properties from **promises into properties**:

1. **FR-47's growth half can come out of the dark.** `overlay.multi_block_enabled` is
   `false` because its P5d wire-compatibility claim was never tested. FR-47's own spec says
   it "must be tested against a pinned old-agent decoder, not argued"
   (`docs/fr/FR-47-overlay-multi-block-ipam.md:169`). Until that exists, an org that reaches
   1 022 devices is refused an address.
2. **A #1237-class multi-org route war becomes catchable before the field.** It is fixed
   (`57cc7223` / #1246, shipped 0.4.51), but nothing in CI or the VM matrix would catch the
   next one — FR-61's matrix is single-tenant by construction.

## Evidence (why this exists)

- **#1237 ran for weeks in the field.** Two per-org runtimes evicted each other's
  `fd72:6f6f:6d6c::/96` on every guard wave: ~100 carrier revalidations/min, **718
  evictions/day on neo16**, CORPLAP-3 the v4 twin. Nothing failed; it was pure waste plus
  demoted real carriers.
- **The same shape against a third party is unbounded.** Our guard evicted Cisco
  AnyConnect's route mirrors **25,197 → 33,294 in one day** while Cisco re-added within
  milliseconds. Neither side holds the FIB and nothing yields (§ auto-yield below).
- **The matrix cannot see either.** FR-61 has one `VMTEST_TENANT_ID` and one
  `VMTEST_EPHEMERAL_KEY`; every guest driver performs exactly one enrollment. There is no
  multi-org cell and no IPAM cell — verified by exhaustive grep, not assumed.
- **`multi_block_enabled` has never run anywhere but a unit test.** Its one e2e
  (`crates/tests/src/overlay_growth_tests.rs`) fakes the scale with a cursor jump and says
  so in its own header; it cannot prove an *old daemon* sizes its netmask and NAT scope
  correctly from the string the server sends.

## Key design (anchors verified against `origin/master` @ `b91a7404`)

### Three layers, cheapest first

**L1 — in-repo tests.** Most of the IPAM risk is closable without a VM:

- **The decoder pin.** No test feeds a P5d netmap *containing* `cidrs` into a pre-P5d
  decoder. `a_pre_p5d_netmap_still_decodes_and_carries_no_block_list`
  (`crates/remote_control/src/signaling.rs:4099`) only tests old-server → new-decoder.
  Copy `agent-v0.4.42`'s `OverlayNetworkInfo` verbatim as a local struct and decode a real
  grown-org netmap into it.
- **Cross-tenant carve racing a grow.** `allocate`'s `DuplicateKey` arm
  (`crates/services/src/dao/overlay_block.rs:216`) is single-block-shaped: with multi-block
  the `network_id_1` index is dropped, so a `DuplicateKey` can only mean *another tenant
  took the slot* — yet the arm returns the network's existing block, the grow is a silent
  no-op, and the joiner is refused `AddressSpaceExhausted`.
- **Renumber-after-grow `seq` collision.** The renumber route
  (`crates/api/src/routes/overlay_block.rs:538-552`) uses the **singular**
  `find_assigned_for_network` and stamps `seq: 0`; `crates/db/src/indexes.rs:408` is a
  plain, non-unique `{network_id, seq}`. A tie on the sort key silently re-points every
  ordinal above it — the exact failure `seq` exists to prevent.
- **The one-way door.** `ensure_indexes(db, false)` after a network holds two assigned
  blocks must fail loudly (`crates/db/src/indexes.rs:17-21`). Untested.
- **Grow-while-live.** `OverlayNetmapDelta` carries no network info
  (`crates/api/src/ws/overlay.rs:652-674`), so a live subnet router never learns the space
  grew; its NAT scope stays block-0-only until the runtime restarts. Assert the bound.

**L2 — two VM cells** (FR-61 matrix, dedicated vmtest orgs, destructive).

**L3 — fleet, read-only.** Two `roomler peers --json` + `roomler status` snapshots ≥30 s
apart on the real multi-org Windows hosts (neo16, CORPLAP-3). No mutation, no VPN cycling,
nothing on prod cluster nodes.

### Why each cell lands on the lane it does

- **Multi-org ⇒ win11.** `defend_self_route` is **Windows-only**
  (`crates/tunnel-core/src/overlay/tun.rs:2663-2719`; no-op default at `tun.rs:95`). An
  Ubuntu cell would pass vacuously.
- **IPAM ⇒ ubuntu.** The pre-P5d degradation is in NAT *scope* — which CIDRs get a
  MASQUERADE rule (`crates/tunnel-core/src/overlay/nat.rs:119-133`) — which is
  platform-independent, and the ubuntu lane is the cheaper driver.

### What actually breaks on a pre-P5d agent (narrower than assumed)

A pinned old agent receives its **own** block in the per-recipient `cidr`
(`crates/api/src/ws/overlay.rs:623-626`), so netmask, per-peer `/32` routes and
`set_local_scope` are all **correct**. Unknown *fields* are silently ignored — there is no
`deny_unknown_fields` anywhere in `crates/remote_control` or `crates/tunnel-core`. Unknown
*variants* are fatal to the whole `ServerMsg` parse, but `agent-v0.4.42` already carries
`OverlayJoinRefused`, and the server gates that send on `supports_join_refusal` anyway.

The one real degradation: **a pre-P5d subnet router masquerades only its own block**, so
traffic from block 1 to a LAN behind a block-0 router black-holes one-way, silently.

⚠️ The genuinely catastrophic shape is a **pre-P5d SERVER** against a grown org with
**mixed prefixes** (`/16` block 0 + `/22` block 1): the node computes a connected prefix
that swallows other tenants' blocks. Uniform `/22` survives a rollback by accident.

### The two gaps in #1246's shipped fix, closed here

**C2(a)** `foreign_in_block_fp` (`tun.rs:816-820`) still reads
`r.InterfaceLuid.Value == ours` where its three siblings were converted to
`route_belongs_to_us` (`tun.rs:681`, `:755`, `:867`). #1246's commit message claims all four
were converted; three were. A sibling org's peer churn therefore flips the in-block debounce
fingerprint and triggers a full FIB walk that deletes nothing — the anti-flap guard defeated
on exactly the multi-org host it was written for.

**C2(b)** FR-64 C2 specifies "a WARN when two orgs' blocks overlap". It was never
implemented (`grep -i overlap` over `tun.rs` and `agents/roomlerd/src/overlay.rs` finds
nothing). With nested blocks — a legacy `/10` primary beside a carved `/22` secondary —
`defended_ula_prefix` (`tun.rs:1773-1791`) produces **nested, not disjoint** v6 prefixes and
the war can silently re-open.

⚠️ A WARN is a detector, not a fix. It makes the condition attributable instead of silent —
the argument FR-33's `lan` line won on. The structural fix (refuse to carve a block nested
inside a live one) belongs to FR-47's reconciler, not here.

### The assertion surface does not exist yet (C1)

There is **no counter** for route evictions or carrier revalidations. Both are log-only,
throttled to **1 WARN/min/prefix** carrying `suppressed_since_last` — a naive `grep -c`
under-reports by up to 40×. Three cumulative counters, following the canonical path
`crates/tunnel-core/src/evidence.rs:36-37` → `crates/localapi/src/lib.rs:180` →
`agents/roomler-cli/src/localclient.rs:1381-1387`:

`ROUTE_EVICTIONS` (split `sibling` / `foreign`) · `ROUTE_WAVES` · `FORCED_REVALIDATIONS`

Cumulative, printed only when non-zero, carrying the "DIFF two readings" note.

### Auto-yield is still gated to metric-0 — noted, not fixed here

The per-prefix strike counter and the `keeps DISAPPEARING` WARN are generalised to every
metric (`tun.rs:488-528`), but the **yield latch is hard-gated on `metric == 0`**
(`tun.rs:517-527`) and metric-0 is default-OFF, so on the fleet the latch is dead code.
A metric-1 war — Cisco's, or #1237's — can never self-limit. Out of scope here; recorded so
the next reader does not assume back-off exists.

## Phases

| # | Phase | Kill switch |
|---|---|---|
| P0 | Claim FR-68 (spec + ledger row, same commit) + issue | — |
| P1 | L1 in-repo tests | n/a (tests only) |
| P2 | C1 — three cumulative counters; field-verify on neo16 + CORPLAP-3 | read-only instrumentation |
| P3 | C2(a) predicate swap · C2(b) overlap WARN | `OVERLAY_SIBLING_EXEMPT` (a); WARN-only (b) |
| P4 | Harness two-tenant plumbing + full 18-cell regression run | `VMTEST_TENANT_ID_B` absent ⇒ cells NA |
| P5 | The two cells, incl. the `multiorg-control` negative control | the cell tuples |
| P6 | L3 fleet read-only arm | — |
| P7 | Handover; decide on `multi_block_enabled` | the flag itself |

## Acceptance criteria

- [ ] **AC1** A pre-P5d `OverlayNetworkInfo` decodes a real grown-org netmap without error
      and ignores `cidrs` — proven with the struct copied verbatim from `agent-v0.4.42`.
- [ ] **AC2** A real `agent-v0.4.42` daemon meshes in a grown org, and its **subnet-router**
      limitation is demonstrated rather than assumed (block-1 → block-0-router LAN).
- [ ] **AC3** A cross-tenant carve racing a grow does not silently refuse the joiner.
- [ ] **AC4** Renumber against a grown org does not duplicate a `seq`.
- [ ] **AC5** `ensure_indexes(db, false)` after growth fails loudly.
- [ ] **AC6** `ROUTE_EVICTIONS` / `ROUTE_WAVES` / `FORCED_REVALIDATIONS` are live and were
      read on two real multi-org hosts, **as a diff of two readings**.
- [ ] **AC7** With C2(a)+C2(b), a two-org Windows guest shows **zero sibling evictions**
      over a sustained window.
- [ ] **AC8** The negative control (`OVERLAY_SIBLING_EXEMPT=0`) **reproduces** the war —
      i.e. AC7 is not passing vacuously.
- [ ] **AC9** A nested-block pair (legacy `/10` primary + carved `/22` secondary) emits the
      overlap WARN.
- [ ] **AC10** The 18 pre-existing vmtest cells are unchanged by the two-tenant plumbing.

## Open decisions

- Whether `multi_block_enabled` flips on after P5, or waits for a real org to approach its
  ceiling. Largest org today is 17 devices against 1 022.
- Whether the mixed-prefix (`/16` + `/22`) shape should be **refused at grow time** rather
  than merely tested — it is the only shape where a server rollback is catastrophic.

## Out of scope

- Generalising auto-yield beyond metric-0 (its own FR — see above).
- The structural fix for nested blocks (FR-47 reconciler).
- The pump-loop stall detector and the other items from the blocking-work analysis.
- Anything that mutates grox / demo / jovanov: the fleet arm is **read-only**.

## Field-verification log

_(empty — nothing verified yet)_
