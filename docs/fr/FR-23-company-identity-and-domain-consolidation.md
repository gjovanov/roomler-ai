# FR-23: Company identity on the site, and one product on both domains

**Issue:** [#827](https://github.com/gjovanov/roomler-ai/issues/827)
**Status:** in progress
**Related:** [FR-7](FR-7-signed-releases.md) (signed releases — the Apple half is what this unblocks)

## Goal

Make the legal entity behind Roomler **publicly verifiable**, correct the company
phone number everywhere it was recorded wrong, and retire the obsolete
`roomler.live` deployment so that both company domains serve the current product.

The immediate driver is Apple Developer Program enrolment **5XS5WN8R99**, which
has sat in review since 2026-08-19. But the imprint is not merely an Apple
workaround — an EU company operating a public service is expected to publish one,
and the phone number is wrong in systems that matter independently of Apple.

## Root cause / field evidence

### 1. The phone number was wrong, and it is my error

The Azure billing profile displayed `0035987711888`. Reformatting that into the
Apple enrolment form and the signed letter of authority produced
**`+359 87 771 1888`** — digits transposed *and* one added. The correct number is
**`+359 87 711 8883`** (national `877118883`).

Apple's organisation verification commonly includes a **call to the company
number on file**. A dead number is the single strongest available explanation for
a review that received its documents (confirmed 2026-08-22 21:47) and then went
silent for five business days.

⚠️ This is not a substring edit: `877711888` → `877118883`. A
`sed 's/7711888/7118883/'` leaves a stray `7` and produces a *different* wrong
number. The whole national part must be replaced.

Measured spread (2026-08-28, `rg` over the working tree):

| Location | Occurrences |
|---|---|
| `scripts/signing/55-apple-duns.sh` | 2 — line 48 (comment), line 63 (field sheet) |
| `apple-authority-letter.md` in the **repo root** | 2 — untracked, **not gitignored** |
| `Dropbox/GROX/Apple enroll/*.md` | copies of record |
| Apple enrolment record 5XS5WN8R99 | submitted — operator action |
| Azure billing profile | `0035987711888`, also wrong (8 digits; a BG mobile is 9) |

The repo-root `apple-authority-letter.md` is a second, unrelated defect: an
untracked file that `git add -A` would have committed company and personal
documents into a public repository.

### 2. Nothing publicly linked either domain to the company

Apple's enrolment requirements state that the organisation's website must be
publicly available and **the domain name must be associated with the
organisation**, with a work email on that domain. The enrolment declares
`goran.jovanov@roomler.live`.

Checked 2026-08-28 — `G ROX`, `EOOD` and `205174895` appear **nowhere** in `ui/`.
`TermsView.vue` and `PrivacyPolicyView.vue` name only the product and give
`legal@roomler.ai` / `privacy@roomler.ai`. There is no imprint, no legal-notice
block, and no company registration anywhere on either site.

So a reviewer tracing **Goran Jovanov → @roomler.live → G ROX EOOD** finds no
public evidence for the last hop. D&B proves the company exists; the register
excerpt proves he owns and manages it; nothing proved the *domains* are its.

Compounding it: the work-email domain (`roomler.live`) served the **legacy**
product (`<title>roomler.ui</title>`), while the current product and every
published contact address live on `roomler.ai`.

### 3. Three-way name reconciliation

The register keeps the name in Cyrillic as „Г РОКС" ЕООД and records **G ROX** as
the registered Latin spelling; D&B and the enrolment say **G ROX EOOD**. A
reviewer without Bulgarian has three strings and no stated relationship between
them. The imprint states it explicitly.

⚠️ Separately and deliberately **out of scope**: the Windows code-signing
certificate subject reads **G ROX LTD**. Same entity, a fourth rendering. Aligning
it would require a new Azure identity validation (the current one is valid to
2028-11-21), so it stays as-is and is recorded here rather than silently endured.

## Key design

### Phase 0 — phone

Replace the whole national part in `scripts/signing/55-apple-duns.sh:48,63`;
delete the untracked repo-root letter (the copy of record is in Dropbox, verified
byte-identical before deletion).

### Phase 1 — imprint

`ui/src/views/legal/ImprintView.vue`, following the existing legal-page pattern
exactly — `<v-theme-provider theme="light">` → `.legal-page` → the 800px
`.legal-container`, `text-h5` section headings, `legal-muted` body, the identical
scoped style block, and an empty `<script setup>` with no `lang="ts"`. These pages
are deliberately **not** i18n'd; `en.json` has no legal namespace.

Route at `ui/src/plugins/router.ts` beside `/privacy` and `/terms`, carrying **no
`meta`** — which is what makes those two unconditionally public, and the imprint
must be reachable signed-out.

Reach: `LandingView.vue`'s footer is the **only** footer in the app (`App.vue` is
just `<v-app><router-view/></v-app>`), so its Legal column gets the link and its
copyright line names the operating company. Terms §16 and Privacy §11 each gain a
company block; on Privacy that block also names the **data controller**, which
GDPR requires and which was missing.

### Phase 5 — roomler.live serves the current app

Mostly free. `files/nginx-pod.conf:3` is `server_name _` (a catch-all), the SPA is
entirely origin-relative, and the API never reads the `Host` header.

**The one dangerous change is `app.cors_origins`.** `crates/api/src/origin.rs:35`
resolves the origin policy, and `:44-49` adds `frontend_url` **only when the
configured list is empty** — an explicit list *replaces* the default rather than
extending it. That is locked by
`an_explicit_list_replaces_the_frontend_default` (`origin.rs:157`), whose comment
states the intent: "an operator who enumerates origins is stating the whole set."

This gates **two** things, which is why `origin.rs` exists at all: the CORS layer,
and the `/ws` cookie handshake (`crates/api/src/ws/handler.rs:109`, via
`origin_is_ours` at `:146-154`). A WebSocket handshake is not subject to CORS, so
this is the check standing between an attacker's page and an authenticated socket.

⇒ the prod configmap must list **both** origins. Listing only `roomler.live`
breaks `roomler.ai` — for browsers *and* for every WS client. `cors_origins` is
`Vec<String>` (`crates/config/src/settings.rs:291`); a scalar env var fails to
deserialize.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| 0 | Phone rectification | revert one commit | ✅ shipped |
| 1 | Imprint page + company blocks + footer | revert; route is additive | ✅ shipped |
| 2 | FR + PR | — | ✅ shipped |
| 3 | Deploy, then send Apple the correction | previous image tag | ⬜ |
| 4 | Delete the obsolete `roomler-old` app | git revert in the deploy repo | ⬜ blocked — no cluster access |
| 5 | `roomler.live` serves the current app | drop the `server_name`; revert the configmap | ⬜ |

## Acceptance criteria

- [x] The wrong number survives **nowhere** in the repo, and the correct one is present
- [x] The repo-root `apple-authority-letter.md` is gone
- [x] `/imprint` renders signed-out, naming G ROX EOOD, UIC 205174895, the registered office, the representative and the corrected phone
- [x] Terms and Privacy name the entity; Privacy names the **data controller**
- [x] The landing footer links the imprint and names the operating company
- [x] `bun run build` (incl. `vue-tsc --noEmit`) and `bun run test:unit` pass
- [ ] `https://roomler.ai/imprint` is live and renders in a browser
- [ ] Apple has the corrected number and the imprint URL
- [ ] The `roomler-old` namespace, its PVCs, PVs and node-local backing directory are gone
- [ ] `https://roomler.live` serves the current app; **`roomler.ai` is unaffected**
- [ ] Apple enrolment 5XS5WN8R99 resolves (this FR closes on that, or on a documented unrelated cause)

## Open decisions

- **OAuth on `roomler.live` will fail** and is shipped that way unless mitigated.
  `oauth.base_url` is a single string, so the callback lands on `roomler.ai` while
  the `oauth_state` CSRF cookie was set **host-only** on `roomler.live` — the
  double-submit check refuses, correctly. Password login is unaffected. Cheapest
  mitigation is an edge redirect of `/oauth/*` on `.live` to `.ai`.
- **Analytics mis-attribute on `.live`** — `ui/index.html:8` hardcodes
  `data-domain="roomler.ai"`.
- Whether `roomler.live` should serve the app at all, or simply **301 to
  `roomler.ai`**. A redirect would satisfy Apple equally (the imprint names both
  domains), costs nothing, and dodges both issues above. Serving the app was the
  operator's stated preference; this is recorded so the cheaper option stays
  visible.

## Out of scope

- Aligning the code-signing certificate subject (`G ROX LTD`) with the register
  form — needs a fresh Azure identity validation.
- i18n for the legal pages — none of the three is translated today.
- **Enrolling agents against `roomler.live`.** Agent identity is
  `(server_url, tenant_id)`, so the same tenant reached via `.live` registers as a
  *second org* with its own WireGuard identity. The installer scripts default to
  `https://roomler.ai` and the dashboard passes `--server ${origin}` — those
  defaults stay untouched.

## Field-verification log

| Date | What | Result |
|---|---|---|
| 2026-08-28 | `rg` for the wrong number across the tree | 4 hits: 2 tracked (`55-apple-duns.sh`), 2 in an untracked repo-root letter |
| 2026-08-28 | `git check-ignore apple-authority-letter.md` | exit 1 — **not** ignored; `git add -A` would have published it |
| 2026-08-28 | `diff` repo-root letter vs the Dropbox copy | identical — safe to delete |
| 2026-08-28 | `rg 'G ROX\|EOOD\|205174895' ui/` | **zero hits** — the gap Apple cannot close |
| 2026-08-28 | Ledger vs GitHub issues | FR-1…FR-22 taken (FR-3 vacated); FR-20/#807 and FR-21/#809 have issues but no rows on master ⇒ claimed FR-23 |
