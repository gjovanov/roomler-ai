# Code signing

How every published Roomler artifact gets signed, why the pipeline is shaped
the way it is, and the exact sequence to (re)establish the credentials.

Publisher of record: **G ROX LTD** (Plovdivska 110, 4400 Pazardzhik, Bulgaria
— UIC/EIK `205174895`, VAT `BG205174895`). The Windows "Verified publisher"
string, the macOS Developer ID team name, the MSI `Manufacturer`, the `.deb`
maintainer and the EXE `CompanyName` all carry this name; if any of them
drifts, sweep them back together (see [Identity alignment](#identity-alignment)).

---

## 1. The two facts that shaped the design

**A PFX-based public code-signing certificate can no longer be bought.**
Since June 2023 the CA/Browser Forum requires every publicly-trusted
code-signing private key to live in FIPS 140-2 Level 2 hardware. The
`WIN_CODESIGN_PFX_BASE64` secret the workflows historically gated on was a
secret that could never be filled — which is why every release up to and
including `agent-v0.3.0-rc.361` shipped `-unsigned`. Signing therefore runs
against a cloud HSM.

**Signing does not clear SmartScreen on day one.** Microsoft removed
instant-reputation-for-EV in 2024; OV, EV and Azure Artifact Signing all build
SmartScreen reputation organically from download volume. What signing buys
*immediately*:

| Outcome | Effect |
|---|---|
| UAC shows **Verified publisher: G ROX LTD** instead of "Unknown publisher" | immediate |
| AppLocker / WDAC **publisher** rules become possible (IT allow-lists the publisher once, not per-release hashes) | immediate — the big corporate-deployment win |
| ESET / Defender heuristic false positives drop (the rc.28 "downloader trojan" quarantine class) | immediate-ish |
| macOS Gatekeeper accepts the `.pkg` / wizard | immediate |
| SmartScreen prompt disappears | accrues with downloads — but now accrues *per publisher across releases* instead of restarting from zero every release |

## 2. Provider: Azure Artifact Signing (formerly "Trusted Signing")

- ~$10/month Basic tier (5 000 signatures/month; a full agent release signs
  ~7 files).
- Certificates chain to the **Microsoft Identity Verification Root CA 2020**
  (in the Windows root program back to Win7 SP1 with automatic root updates).
- Leaf certificates auto-rotate on a ~3-day lifetime — which is why **RFC3161
  timestamping is mandatory**, and why the CI verify step hard-fails on a
  signature with no countersignature. A timestamped signature stays valid
  after the leaf expires; an untimestamped one dies in days.
- **Zero key material exists in GitHub.** CI authenticates with GitHub OIDC
  (`azure/login` + a federated credential on an Entra app registration); the
  key never leaves Microsoft's HSM. The six `AZURE_*` values in repo
  configuration are **variables**, not secrets — none are confidential.
- EU **organizations** are eligible (G ROX LTD is Bulgarian — qualifies).
  EU *individuals* are not (US/Canada only), which is why signing runs under
  the company identity.

## 2b. Live status (2026-08-22) — the Azure half is DONE

- Identity validation **Completed** for G ROX LTD (id
  `2390bcea-a3d5-4919-a624-cb209c875bf7`, valid to 2028-11-21).
- Account `roomlersigning` (Basic) in **polandcentral**; profile
  `roomler-public-trust`; subscription `cb6a5135-9bee-4a35-890f-a3f38f867e88`
  under goran.jovanov@roomler.live.
- Field-proven: a roomler binary signed from the dev box verifies with
  `signtool verify /pa` as `CN=G ROX LTD, O=G ROX LTD, L=Pazardzhik, C=BG`.
- GitHub OIDC wired: app `roomler-ai-github-signing`
  (`9a452e59-1312-49d8-bc4a-8f443be19c5f`), signer role scoped to the
  profile, all seven repo variables set.
- **Federated-credential caveat**: this tenant rejects wildcard
  (`refs/tags/*`) subjects — Graph's GitHub-issuer rules refused the
  flexible expression. The working credentials are exact-subject
  `ref:refs/heads/master` (dispatch rehearsals) and
  **`environment:release`** — which is why every job that calls
  `sign-windows` in azure mode carries `environment: release`. Removing
  that line breaks the OIDC exchange for tag builds.
- Still pending: Apple D-U-N-S → `60-apple-setup.sh`; GPG key →
  `70-gpg-release-key.sh`.

## 3. Credential setup — `scripts/signing/`

Idempotent operator scripts, in order. State (public identifiers only) is
shared through `scripts/signing/.roomler-signing.json` so each value is typed
once. Full per-script docs in `scripts/signing/README.md`.

```text
00-preflight.ps1              tooling, subscription, region, billing type, country
10-azure-provision.ps1        Artifact Signing account + operator roles
                              -> then submit identity validation IN THE PORTAL
                                 (portal-only; 1 to 20 business days; the script
                                 prints a filled-in field sheet for G ROX LTD)
15-azure-identity-status.ps1  status guide; -SetId <guid> records the portal result
                              (validation status is PORTAL-ONLY -- no ARM/CLI surface)
20-azure-cert-profile.ps1     PublicTrust certificate profile
30-github-oidc.ps1            app registration + federated credentials
                              + "Artifact Signing Certificate Profile Signer"
                              scoped to the PROFILE + the six repo variables
40-smoke-sign.ps1             sign a throwaway PE from the dev box (proves
                              profile + RBAC before any CI run)
```

Repository variables written by `30-github-oidc.ps1`:

```text
AZURE_SIGNING_ENDPOINT        https://plc.codesigning.azure.net (polandcentral —
                              westeurope refused new-subscription resources)
AZURE_SIGNING_ACCOUNT         Artifact Signing account name
AZURE_SIGNING_PROFILE         certificate profile name
AZURE_CLIENT_ID               app registration (federated, secret-less)
AZURE_TENANT_ID               Entra tenant
AZURE_SUBSCRIPTION_ID         subscription
AZURE_SIGNING_EXPECT_SUBJECT  expected CN — locks the publisher; a profile swap
                              that changes the subject fails CI verification
```

Apple (`60-apple-setup.sh csr|p12|secrets|check`): Developer ID Application +
Installer certs and the notarytool API key → six `APPLE_*` secrets. Org
enrolment needs a **D-U-N-S number** for G ROX LTD (free, ~1–2 weeks — start
it the same day as Azure identity validation; both run in parallel with all CI
work). GPG (`70-gpg-release-key.sh create|export`): ed25519 signing subkey →
`GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` / `GPG_KEY_ID`; the primary key stays
offline.

## 4. What gets signed, where

| Artifact | Workflow | Mechanism |
|---|---|---|
| `roomlerd.exe`, `roomler.exe` **inside** both MSIs | release-agent | sign-windows, *before* `cargo wix` harvests |
| perUser + perMachine `.msi` | release-agent | sign-windows, before the size-budget/sha256 steps |
| `roomler-desktop-<v>….exe` | release-agent | sign-windows |
| `roomler-agent-<v>-aarch64-apple-darwin.pkg` | release-agent | inline codesign → productbuild → notarytool → staple → **verify** |
| `roomler-setup.exe` (in the zip) | release-setup | sign-windows, before zipping |
| `Roomler Setup.app` (in the macOS tarball) | release-setup | sign-macos: bundle → notarise → **staple** (ticket survives tar) |
| `roomler.exe` (tunnel zip) | release-tunnel | sign-windows (was never signed before) |
| `roomler` (macOS universal, tunnel tarball) | release-tunnel | sign-macos: bare binary → notarise, **no staple** (Apple can't staple a loose Mach-O; curl\|tar installs never set quarantine, so unaffected) |
| every published asset | all three publish jobs | GPG `.asc` (when key configured) + SLSA provenance attestation (always) |
| `wintun.dll` | — | **never re-signed** — WireGuard LLC's Authenticode signature must survive; CI asserts it still says WireGuard |
| `.deb` contents | — | not signed (dpkg doesn't verify; apt-repo signing deferred until an apt channel exists) |

Payload-before-wrapper ordering matters: the on-disk `roomlerd.exe` is what AV
scans, what Task Manager attributes, and what an AppLocker/WDAC publisher rule
evaluates — an MSI-only signature covers none of that. The payload signing
step sits after `encoder-smoke` (fail before spending a signature) and before
the wintun staging (so a folder sweep structurally *cannot* touch the
third-party DLL). **Invariant: no step after payload signing may run
`cargo build` in that job** — a relink silently strips the signatures.

## 5. The composite actions

### `.github/actions/sign-windows`

Modes: `auto` (azure if the six variables are set, else local PFX, else none)
· `azure` · `local` · `off`. After signing it **verifies**: `signtool verify
/pa /all` on every file, RFC3161 countersignature present,
`expect-subject-contains` match. `require` (default: true on tag pushes)
turns "unsigned" into a job failure — a release tag can no longer silently
publish unsigned artifacts, which is also why the `-unsigned` filename suffix
is gone (it was computed from *secret presence*, before signtool even ran).
Every asset-name consumer (`agent_release.rs`, `updater.rs`,
`tunnel_release.rs`, `setup_release.rs`, `install.sh`, `install.ps1`) is
suffix-agnostic in both directions, so old published releases keep resolving.

`local` mode is the same code path with a different key: it imports the
self-signed test cert into the runner's `Root` + `TrustedPublisher` stores so
the identical strict verification runs. `azure/login` and
`azure/artifact-signing-action` are pinned to commit SHAs — they are the
closest thing to a signing key in this pipeline.

### `.github/actions/sign-macos`

All-or-nothing on the six Apple secrets (a partial set is a hard error —
half-configured used to be able to publish a signed-but-unnotarised `.pkg`
with **no warning**, which Gatekeeper rejects just as hard as unsigned).
`bundle-path` → sign + notarise + staple; `binary-path` → sign + notarise
(stapling a loose executable is unsupported by Apple). Verification includes
`xcrun stapler validate` and `spctl -a` asserting
`source=Notarized Developer ID` — the exact Gatekeeper path an end user hits.
release-setup additionally re-extracts its own tarball and re-validates, so
"the ticket didn't survive tar" can never ship.

## 6. Rehearsal — prove everything without spending a cent

```powershell
# once: self-signed cert -> WIN_TEST_PFX_* secrets
pwsh scripts/signing/50-selfsigned-dev-cert.ps1 -PushSecrets
```

```bash
# artifacts-only runs (publish_release=false publishes nothing)
gh workflow run release-agent.yml  -f version=<v> -f publish_release=false -f signing_mode=local -f require_signing=true
gh workflow run release-setup.yml  -f version=<v> -f publish_release=false -f signing_mode=local
gh workflow run release-tunnel.yml -f version=<v> -f publish_release=false -f signing_mode=local

# negative test — the gate must FAIL, not warn
gh workflow run release-agent.yml -f version=<v> -f publish_release=false -f signing_mode=off -f require_signing=true
```

Do **not** rehearse with throwaway `agent-v*` tags: a tag push publishes a
non-prerelease Release that the field fleet's 6-hourly `/releases/latest`
poll picks up immediately. `installer-smoke.yml` additionally runs the
payload-signing step in `local` mode on every master push once the
`WIN_TEST_PFX_*` secrets exist — free permanent regression coverage.

## 7. Verifying a release

```powershell
pwsh scripts/signing/90-verify-release.ps1 -Tag agent-v<version>   # Windows half
```
```bash
./scripts/signing/90-verify-release.sh --tag agent-v<version>      # macOS/Linux half
```

The PowerShell verifier also does an administrative extract of each MSI
(`msiexec /a`) and Authenticode-checks the **payload**, and asserts
`wintun.dll` still says WireGuard LLC. Manual user-level checks:

```powershell
Get-AuthenticodeSignature "$env:ProgramFiles\Roomler\roomlerd.exe"   # Valid, G ROX LTD
```
```bash
spctl -a -vvv -t install roomler-agent-<v>-aarch64-apple-darwin.pkg  # source=Notarized Developer ID
gh attestation verify roomler-agent-<v>-x86_64-unknown-linux-gnu.deb --repo gjovanov/roomler-ai
curl -fsSL https://github.com/gjovanov/roomler-ai/releases/latest/download/roomler-release-pubkey.asc | gpg --import
gpg --verify <asset>.asc <asset>
```

## 8. Enterprise pilot path (before/without the public cert)

`50-selfsigned-dev-cert.ps1` + `51-trust-dev-cert.ps1` produce a self-signed
cert and the GPO / Intune recipes to trust it on a managed fleet — import the
`.cer` into **both** *Trusted Root Certification Authorities* and *Trusted
Publishers* (Intune needs a Custom OMA-URI profile for the TrustedPublisher
store; the built-in template only targets Root). This unblocks
AppLocker/WDAC-gated pilots (ÖBB/ORF-style) today. Remove the trust entries
when the pilot ends; artifacts signed with the dev cert must never be
published.

## 9. Identity alignment

The cert subject and every packaging surface must agree, or Windows shows one
name in the UAC prompt and another in Add/Remove Programs. Current sweep
(all `G ROX LTD`):

- `agents/roomler-agent/wix/main.wxs` + `wix-perMachine/main.wxs` — `Manufacturer` (2× each)
- `agents/roomler-agent/build.rs` + `agents/roomler-tunnel/build.rs` — `CompanyName`, `LegalCopyright`
- `agents/roomler-agent-tray/tauri.conf.json` + `agents/roomler-setup/tauri.conf.json` — `bundle.publisher`, `bundle.copyright`
- both `[package.metadata.deb]` blocks — `maintainer`, `copyright`

The `AZURE_SIGNING_EXPECT_SUBJECT` variable enforces the cert side: if the
issued subject ever stops containing the expected CN, every signing call fails
verification instead of quietly shipping a different publisher.

`LICENSE` deliberately still names Goran Jovanov — transferring the MIT
copyright grant to the company is a legal decision, not a packaging one.

## 10. Version info

`roomlerd.exe` and `roomler.exe` embed a full `VS_VERSION_INFO` via
`build.rs` + `embed-resource` (`compile_for` → per-binary
`rustc-link-arg-bin`, chosen because `winres`/`winresource` link-lib
directives would propagate a *second* version resource into the Tauri EXEs
that depend on these crates). `FILEVERSION` uses the same `MAJOR.MINOR.RC`
remap as the MSI ProductVersion (`0.3.0-rc.238` → `0.3.238.0`), so file
version and installer version always agree. The Tauri configs inherit the
workspace version (the hardcoded `rc.30`/`rc.197` pins are gone).

## 11. Renewal / rotation

- **Azure**: nothing to renew — leaves rotate automatically every ~3 days;
  the *identity validation* itself expires per CA policy and the profile
  stops issuing when it does. If signing starts failing with a validation
  error, re-run the portal validation (`15-azure-identity-status.ps1` to
  watch) — account, profile name, OIDC wiring and repo variables all stay.
- **Apple**: Developer ID certs live 5 years; the ASC API key doesn't expire
  but can be revoked. Re-run `60-apple-setup.sh` for either.
- **GPG**: subkey expires per `GPG_EXPIRE` (default 2y); extend or mint a new
  subkey with the offline primary, then re-run `70-gpg-release-key.sh export
  --push`.
- If the Azure *subject* ever changes (rename, re-registration), update
  `AZURE_SIGNING_EXPECT_SUBJECT` **and** do the §9 sweep in the same PR.
