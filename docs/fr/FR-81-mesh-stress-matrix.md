# FR-81: Overlay stress matrix — latency, throughput and carrier stability from a throwaway VM

**Issue:** [#1546](https://github.com/gjovanov/roomler-ai/issues/1546) ·
**Status:** proposed 2026-09-08

## Goal

Boot a **throwaway VM** into the fleet org and hammer the private mesh against real endpoints —
the three corporate laptops (CORPLAP-1/2/3) and the two fleet servers — measuring what the overlay
actually delivers: **latency distribution, packet loss, bulk throughput, SSH session reliability,
and carrier stability over time**, on **both** the direct and relay carriers. Repeatable as a
skill.

Everything the mesh promises is a *latency and stability* claim, and nothing in CI or the existing
matrices measures either. FR-61 proves a device installs and can ping once; FR-75 proves a profile
carries traffic once. Neither asks the mesh to keep working for ten minutes under load, and neither
records a distribution.

## Evidence — what the recon already showed

Measured 2026-09-08 from mars before any code was written. This is the design's foundation, because
it disqualifies the obvious matrix:

| host | `netcheck` | carrier now | direct achievable? |
|---|---|---|---|
| CORPLAP-1 | `stun/udp: NO MAPPING` · relay band **BLOCKED** · nat untyped | `relay:derp/tcp` | **No** — no UDP egress at all |
| CORPLAP-2 | `stun/udp: NO MAPPING` · relay band **BLOCKED** · nat untyped | `relay:derp/tcp` | **No** — no UDP egress at all |
| CORPLAP-3 | `stun/udp: ok` · relay band **BLOCKED** · nat **symmetric** | `relay:derp/tcp` | **No** — symmetric NAT, srflx ineligible (penalty 74.9) |
| zeus / mars | stun ok · relay band reachable · nat cone | `direct` | Yes |

⚠️ **`roomler why` says every tier is "eligible" for all three corp laptops — and that is not a
contradiction.** The ELIGIBLE column is the *local* end's willingness to offer a tier; the
**MEASURED PATH** rows are the truth. CORPLAP-2's measured direct path runs at **83 % loss**,
CORPLAP-3's at **100 %**. Reading "eligible" as "achievable" is the trap this table exists to
prevent, and a matrix that demanded `direct` from a corp laptop would report three permanent reds
for a mesh behaving exactly as designed.

**So the matrix asserts the carrier that is CORRECT for each host, never a fixed one**, and the
direct-vs-relay comparison is obtained where it is obtainable — against the fleet servers, with the
relay arm *forced* rather than hoped for.

## The matrix

One throwaway VM per **arm**; each arm sweeps every target.

| arm | how the carrier is obtained | targets |
|---|---|---|
| `direct` | VM's overlay defaults | zeus, mars → **direct** · CORPLAP-1/2/3 → **relay** (expected; the ladder is right) |
| `relay` | `overlay_direct=false` on the VM, forcing DERP | every target → **relay** |

The `direct` arm's corp-laptop rows are not a failure and not filler: they are the **control** that
shows the relay arm's numbers are not an artefact of the forcing knob.

### Per (arm, target) measurements

| check | what it measures | how |
|---|---|---|
| `carrier` | the carrier actually in use, plus `why` | `roomler peers` + `roomler why` |
| `latency` | p50 / p95 / max / loss over N samples | `roomler ping` ×N |
| `ssh` | session establishment reliability + cost | `roomler ssh <t> -- <trivial>` ×M, success rate + wall time |
| `throughput` | bulk bytes over the mesh, both directions, integrity-checked | `scp` via `roomler proxy` ProxyCommand, sha256 verified |
| `stability` | carrier transitions during the run | sample `roomler peers` every 10 s; count changes |

## Key design decisions

**The VMs join the FLEET org, not the vmtest org** — operator-approved, and unavoidable: the mesh
is tenant-scoped, so a vmtest-org VM cannot see a fleet-org laptop at all. This deliberately
suspends the vmtest rail "never the prod fleet org" for this lane only.

⚠️ **Enrollment is a STANDARD single-use token, not an FR-51 ephemeral key** — the fleet org has
`ephemeral_keys_enabled: false`, and flipping a security switch on the production org for a test is
a worse trade than deleting a row. Consequence: **there is no self-cleaning safety net**, so the
cell deletes its device row from an `EXIT` trap and the run asserts the fleet device count returns
to baseline. (Same shape as FR-68's org-B cell.)

⚠️ **The corp laptops' configuration is never touched.** Carrier pinning happens only on the
throwaway VM. Their `ssh_enabled: true` / `ssh_account_mode: daemon` state is read, relied on, and
left alone — which is also why `scp` works there at all (the sftp subsystem spawns as the daemon;
a `console_user` host would refuse).

⚠️ **These are someone's working machines.** Transfer size, sample counts and session counts are
bounded and configurable, files land in the platform temp dir and are deleted, and the whole sweep
is minutes not hours.

## Acceptance criteria

- [ ] **AC1** a throwaway VM enrolls into the fleet org, appears on the mesh, and is fully removed
      at teardown (device row gone, VM destroyed, fleet count back to baseline).
- [ ] **AC2** every target's carrier is recorded with `why` evidence, and each is asserted against
      what that host can actually achieve — never a fixed expectation.
- [ ] **AC3** latency is reported as a distribution (p50/p95/max/loss), not a single ping.
- [ ] **AC4** bulk transfer completes in both directions with a verified sha256, and yields a
      throughput number per arm.
- [ ] **AC5** SSH session establishment is exercised repeatedly and reported as a success rate.
- [ ] **AC6** carrier transitions are counted over the run — the stability claim gets a number.
- [ ] **AC7** the relay arm is *forced*, not hoped for, and the direct arm proves the forcing knob
      changed something.
- [ ] **AC8** the whole sweep runs from one command and is documented in a skill.

## Out of scope

Tuning anything. This measures; it does not fix. Also: no carrier pinning on any host but the
throwaway VM, no org-setting changes, no conclusions about corp-network policy beyond what
`netcheck` reports.

## Field-verification log

_(filled as runs land — every entry records what failed first)_

### 2026-09-08 — first full run, 22 PASS / 0 FAIL

Run `20260908-210103` on zeus, both arms, five targets each. No VM left; fleet org back to 21
devices (baseline 21) on both arms. Full table on #1546.

**`carrier_transitions=0` on all ten (target × arm) pairs** — no carrier flapped during either
sweep. That is the stability answer, and the first number that distinguishes a mesh which connects
once from one that holds.

**The forcing knob is proven**: mars measured `direct` at p50 **0 ms**, then `relay:derp/tcp` at
p50 **2 ms** under `overlay_direct=false`. Same host, same sweep. Without forcing, never-ratchet
would have made the relay arm a second direct arm.

Corp-laptop latency over DERP is **p50 47–57 ms**, p95 tail 55–116 ms, loss 0–2.5 % — usable, and
the tail is the number to watch across releases.

**Findings:** a VM running *on* zeus reaches zeus over DERP rather than LAN (2 ms, so cheap here,
but the lan tier never won); CORPLAP-3 and mars report `ssh_enabled: true` while having never
published an SSH host key, so `roomler ssh` correctly refuses and SSH reads as enabled-but-unusable;
and a grant-issued session gets a shell but **cannot `scp`**, because `roomler proxy` carries no
client identity.

**Five harness defects, each found only by running it:**

1. `roomler peers` CONN is field **5**, not 4 (a status bullet is field 1) — `$4` reported the IPv6
   *address* as a carrier. It looked like data.
2. The ssh arm swallowed stderr and reported a bare `ssh_ok=0`; the real answer was a POLICY gate.
3. That gate is `SshPolicy.can_originate`, default false — which forced the guest lane into
   `--phase enroll` / `--phase sweep`, since the row does not exist until enrol has run.
4. Corp-laptop SSH lands in **PowerShell**, so `echo ok` returns `ok\r` and `grep '^ok$'` failed on
   a session that worked — the metric read `ssh_ok=0 ssh_err="ok"`, contradicting itself.
5. The cleanup check grepped for the payload **path**, and PowerShell's not-found error quotes the
   path back — so a clean machine reported `LEFT_BEHIND`. It failed safe; the same shape inverted
   is how real leftovers get reported as gone. It now probes for a token.
