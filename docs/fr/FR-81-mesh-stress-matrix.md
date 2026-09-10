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

- [x] **AC1** a throwaway VM enrolls into the fleet org, appears on the mesh, and is fully removed
      at teardown (device row gone, VM destroyed, fleet count back to baseline).
      *Both cells: `fleet-residue PASS — back to 21 devices (baseline 21), no ghost left`, plus
      `teardown/org-baseline` on both orgs and no VM on zeus.*
- [x] **AC2** every target's carrier is recorded with `why` evidence, and each is asserted against
      what that host can actually achieve — never a fixed expectation.
      *`VMTEST-CARRIER` per target, `roomler why` captured to `/tmp/why-<t>.txt`, and the verdict
      is reachability + stability, never which carrier won.*
- [x] **AC3** latency is reported as a distribution (p50/p95/max/loss), not a single ping.
      *40 samples × 5 targets × 2 arms; 0.0 % loss on every one.*
- [x] **AC4** bulk transfer completes in both directions with a verified sha256, and yields a
      throughput number per arm.
      *32 MiB round trip, sha256 verified, on CORPLAP-1 and CORPLAP-2 in both arms
      (0.36–0.41 MiB/s). CORPLAP-3 reports `target-has-no-sftp-server`; zeus and mars serve no
      SSH at all — each named, none silent.*
- [x] **AC5** SSH session establishment is exercised repeatedly and reported as a success rate.
      *3/3 on all three corporate laptops in both arms — including CORPLAP-3, which had never
      produced a successful session in this lane before #1565.*
- [x] **AC6** carrier transitions are counted over the run — the stability claim gets a number.
      *`carrier_transitions=0` on all ten (target × arm) pairs across ~3 ¼ hours.*
- [x] **AC7** the relay arm is *forced*, not hoped for, and the direct arm proves the forcing knob
      changed something.
      *mars: `carrier=direct` p50 0 ms in the direct arm → `carrier=relay:derp/tcp` p50 2 ms in the
      relay arm. Same VM image, same target, one config key.*
- [x] **AC8** the whole sweep runs from one command and is documented in a skill.
      *`vmtest.sh run --lane stress --host zeus`; the `meshstress` skill carries eleven traps.*
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

### 2026-09-09 — round two: the transfer arm, and two product bugs

The operator enabled SSH on all three corp laptops and placed a client public key in each device's
`ssh_authorized_keys` — precisely what round one's third finding asked for. Re-running against that
turned the transfer arm from an accepted `NA` into a measurement, and found two defects in the
product on the way.

#### Round one's conclusion was only a third right

Round one recorded *"a grant session cannot `scp`, because `roomler proxy` carries no client
identity."* True as a mechanism — but it was **reasoning, not measurement**: the lane discarded
scp's stderr, so the missing identity was an inference from the docs rather than something the run
had seen. With the key in place and stderr kept, **four** independent things had to be right, and
the identity was one of them.

| # | wall | what it looked like |
|---|---|---|
| 1 | **`scp` dials 22; roomler SSH listens on 2222** | `Error: connecting to <ip>:22 … Connection timed out`. OpenSSH hands its port to the ProxyCommand as `%p`, so `roomler proxy` faithfully dialled `:22`. ⚠️ `roomler proxy --help`'s own example has this gap. |
| 2 | **an identity** | with `-i` + `IdentitiesOnly`: `Authenticated to … (via proxy) using "publickey"`, and the device logs `sftp session started … run_as=daemon privileged=true`. |
| 3 | **the verdict must be the file, not scp's status** | see the first product bug below |
| 4 | **`/C:/…` on Windows** | a bare relative destination lands in the sftp default cwd, which is `C:\WINDOWS\system32`. |

#### Product bug 1 — `scp` exits 1 on a transfer that fully succeeded (#1559, merged)

`roomlerd`'s sftp subsystem was the only channel path that never sent an `exit-status`; the pty,
exec and every refusal do. OpenSSH reads it as the verdict on the transfer, logs `Exit status -1`
and exits 1:

```
scp rc=1
### did it land? (asked over SFTP, SAME identity that wrote it)
-rw-------    ? 0  0   2097152 Sep  9 21:30 /C:/Windows/Temp/rl-decide.bin
### sftp GET it back and compare
SHA MATCH — the upload was COMPLETE and CORRECT
```

`sftp` was unaffected — it never consults the channel status — which is exactly why this stayed
invisible. Anything that branches on scp's status retries or aborts work that is already done.

#### Product bug 2 — a corrupt host key wedges a device permanently (#1564 / #1565, merged)

CORPLAP-3 refused every SSH attempt with *"has not published an SSH host key … needs an agent that
has had SSH enabled at least once (rc.444+)"* — on a device running **0.4.96**, with
`ssh_enabled = true` and a key present in its config. The stored PEM is corrupt; its boundary line
against a working sibling:

| device | len | content |
|---|---|---|
| CORPLAP-3 | 41 | `-----BEGIN OPENSSH PRIVATE KEY-----\r\r\` |
| CORPLAP-1 | 35 | `-----BEGIN OPENSSH PRIVATE KEY-----` |

The daemon detects it and fails closed — correct — but the mint was gated on `is_none()`, and
`Some(garbage)` is not `None`, so it never replaced it. Two restarts (operator-approved) changed
nothing. ⚠️ The one WARN that explains it sits past the ≤64 KiB tail `roomler logs --grep` reads;
it took a whole-file `Select-String` to surface.

#### 🔑🔑 The cleanup check has now been wrong in three ways, and all three printed green

1. a path the checker was not looking in (sftp cwd ≠ shell cwd);
2. a not-found error that **quoted the path back**, so grepping for the path counted the error as a
   hit;
3. **an account that could not see the directory at all** — the transfer authenticates from the key
   list, so it runs as `ssh_account_mode = daemon` (SYSTEM) and writes `C:\Windows\Temp`, while a
   `roomler ssh` verification is a *grant* session that policy resolves to `console_user`:

```
--- did it land? ---
ABSENT
Test-Path : Zugriff verweigert
```

Removal and proof now run over SFTP on the transfer's own identity, and an unreadable listing is a
**third state** (`cleanup=UNVERIFIED`), never scored clean. That state then earned itself on the
very next run: with no identity the listing could not be read, the check refused to say "gone", and
a manual SFTP probe with the operator's key confirmed `not found` on both laptops — nothing left
behind. Under the previous code that situation printed `verified_gone`.

#### Two more harness defects, in this round's own code

6. **`sftp -b` echoes each command**, so its output opens with `sftp> ls -l <path>` — a line whose
   last field is *also* the path. Matching on the path alone matched the echo first, and `$(NF-4)`
   on a four-field line is `$0`, so every successful upload would have read `up=no`. Caught by
   unit-testing the awk against real sftp output before the run produced numbers.
7. **A readability test asked as the wrong user.** The lane runs as unprivileged `vmtest` while
   every consumer of the client key runs under `sudo`, and the key is staged 0600 root-only — so
   `[ -r "$CLIENT_KEY" ]` was false about a key that works. Measured in the live VM:
   `plain -r : FALSE` / `sudo -r : TRUE`. The same shape as defect 3 above, twenty lines away in
   the same file: **a permission answer read as an existence answer**, found once and not carried
   across. Both sites now call one `key_usable()` predicate.

#### Rail change, with operator approval

The standing *"nothing on a corp laptop is configured, restarted, or left behind"* was suspended
once, explicitly, to restart CORPLAP-3's daemon. The skill now records that enabling SSH on a
target, or restarting its daemon, is an **operator decision every time** — never inferred from the
fact that the matrix would be greener.

#### Known limit

CORPLAP-3 cannot transfer files even once #1565 ships: it has no `sftp-server.exe`. roomler spawns
the platform's binary rather than embedding one, so that a transfer runs as the session's account
instead of as the daemon — and on Windows that binary ships with the OpenSSH **Server** optional
feature, which a corp-managed laptop need not have. `Test-Path` reads `False` there and `True` on
both other laptops.

#### Result — 24 PASS / 0 FAIL, run `20260909-204509`

| target | arm | carrier | loss | p50 | p95 | max | SSH | 32 MiB round trip | transitions |
|---|---|---|---|---|---|---|---|---|---|
| CORPLAP-1 | direct | relay:derp | 0 % | 55 | 79 | 247 | 3/3 | ✅ 0.37 / 0.36 MiB/s · `scp_rc=1` | 0 |
| CORPLAP-1 | relay | relay:derp | 0 % | 55 | 135 | 265 | 3/3 | ✅ 0.36 / 0.36 · `scp_rc=1` | 0 |
| CORPLAP-2 | direct | relay:derp | 0 % | 57 | 105 | 199 | 3/3 | ✅ 0.41 / 0.34 · `scp_rc=0` | 0 |
| CORPLAP-2 | relay | relay:derp | 0 % | 56 | 131 | 289 | 3/3 | ✅ 0.40 / 0.34 · `scp_rc=0` | 0 |
| CORPLAP-3 | direct | relay:derp | 0 % | 46 | 82 | 95 | 3/3 | ⛔ no `sftp-server` | 0 |
| CORPLAP-3 | relay | relay:derp | 0 % | 46 | **50** | 89 | 3/3 | ⛔ no `sftp-server` | 0 |
| zeus | direct | relay:derp | 0 % | 2 | 8 | 27 | `ssh_enabled=false` | — | 0 |
| zeus | relay | relay:derp | 0 % | 2 | 2 | 29 | `ssh_enabled=false` | — | 0 |
| mars | **direct** | **direct** | 0 % | **0** | 0 | 0 | no host key published | — | 0 |
| mars | **relay** | **relay:derp** | 0 % | 2 | 3 | 6 | no host key published | — | 0 |

Zero loss on every target in both arms, and `carrier_transitions=0` on all ten pairs over ~3 ¼
hours — the number that separates a mesh which connects once from one that holds.

#### The matrix verified #1559 on its own, by accident and then on purpose

`agent-v0.4.97` rolled **during** the run, so the fleet was mid-rollout and `scp_rc` — added to the
metric as standing evidence rather than a one-off check — captured both sides:

| CORPLAP-1, the SAME device | agent | `scp_rc` | bytes |
|---|---|---|---|
| direct arm | 0.4.96 | **1** | `up=yes sha_match=yes` |
| relay arm | 0.4.96 | **1** | `up=yes sha_match=yes` |
| targeted re-test after its update | **0.4.97** | **0** | `remote_size=2097152 sha_match=yes` |

Three measurements on one laptop, one variable. The two 0.4.96 rows also rule out a timing race:
a race would vary *within* a device across arms, and it does not.

⚠️ The between-device comparison (CORPLAP-1 vs CORPLAP-2) came first and was the **weak** form —
two laptops differ in more than their agent version. It is recorded because it is what prompted the
within-device test, not because it settled anything.

#### Residue, checked as the identity that could actually see it

`cleanup=verified_gone` on CORPLAP-1/2 over SFTP; on CORPLAP-3 **neither identity reachable from
inside the VM can look** (sftp cannot start, and a `roomler ssh` grant session is `console_user`,
denied on `C:\Windows\Temp`), so it correctly reported `UNVERIFIED` and was checked from mars with
`roomler exec`, which runs as the daemon: **0 files** matching `roomler-stress*`. zeus and mars: 0.
Fleet org back to 21 devices, zero ghosts, no VM on zeus, k8s untouched.
