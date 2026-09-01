<!-- SPDX-License-Identifier: MPL-2.0 -->
# Newsletter — the subscriber list and the sending program

FR-39 built the list; FR-58 built the publisher. This doc is the operator's view of both.
Spec: [`docs/fr/FR-58-newsletter-sending.md`](fr/FR-58-newsletter-sending.md).

## The list (public surface)

| Route | What it does |
|---|---|
| `POST /api/subscribe` `{email, source?}` | Always **202**, every outcome — a response that distinguished "new" from "already subscribed" would be a membership oracle for addresses that are usually also `users.email`. Stores the row unconfirmed, mails a confirm link (15-min per-address resend cooldown — the mail-bomb guard). |
| `GET /api/subscribe/confirm/{token}` | **Pure redirect** to `/newsletter/confirm/{token}` — never confirms. Mailbox link scanners follow GETs (Gmail's burned the first field subscriber's token before any human clicked), so the page button's **POST** is the deliberate click: single-use, `{confirmed: bool}`. |
| `GET /api/subscribe/unsubscribe/{token}` | Idempotent (mail clients prefetch). 303 → `/newsletter/unsubscribed?status=…`. The token never expires — a two-year-old email's link must still work. |
| `POST /api/subscribe/unsubscribe/{token}` | The RFC 8058 one-click target (`List-Unsubscribe-Post`). Plain 200 for every outcome, body unread. |

Unsubscribed rows are **kept and stamped**, never deleted — the suppression list is the
consent record, and re-subscribing after a withdrawal always requires confirming again.

## Issues and sending (platform-admin surface)

Everything under `/api/admin/newsletter/*` is gated by the existing
`ROOMLER__STATS__PLATFORM_ADMINS` ObjectId allowlist and answers **404** on missing
authority (never 403 — the web client force-logs-out on 403). Allowlist unset ⇒ the whole
surface does not exist; that inherent gate is the kill switch.

| Route | What it does |
|---|---|
| `POST /issues` / `PUT /issues/{slug}` | Create a draft / edit **while draft**. Explicit create — a typo'd slug 404s, never upserts. Slug uniqueness is index-arbitrated (409). |
| `GET /issues` / `GET /issues/{slug}` | List / read back. |
| `GET /issues/{slug}/preview` | The **exact** send-path bytes (sample unsubscribe link substituted). Preview IS the sent artifact, because the fan-out pre-renders once and substitutes per recipient. |
| `POST /issues/{slug}/test-send` `{email}` | Render + send to one address with the real headers. No ledger, honest failures. |
| `POST /issues/{slug}/send` `{retry_stale?}` | Claim the issue (`draft` → `sending`, CAS; a stale claim >10 min may be re-claimed = resume) and fan out. 400 when no mailer is configured — refusing beats ledgering a whole list into `failed`. |
| `GET /issues/{slug}/status` | Live counts while sending, stored counts once `completed`, stale rows listed by address. |

Drive it with [`scripts/newsletter-send.py`](../scripts/newsletter-send.py) from the
issue's canonical `.md` source (frontmatter + markdown body).

### Send semantics worth knowing before the first real send

- **Per-recipient ledger** (`newsletter_sends`, unique `{issue_id, subscriber_id}`):
  claim-first, so re-POSTing `send` never double-sends — the unique index is the
  invariant, the issue claim only an efficiency gate.
- **Stuck `claimed` rows are reported, never auto-retried** — a crash between the mail
  backend's accept and our mark is genuinely ambiguous. `{"retry_stale": true}` is the
  explicit operator decision to re-attempt them.
- **Terminal status is `completed`, never "sent"** — counts
  `{total, sent, failed, suppressed, stale}` carry the truth, and ledger `sent` means
  "accepted by the backend", not delivered.
- The recipient set is a **snapshot at send time**; mid-send *unsubscribes* are honored
  (re-checked per recipient), mid-send *confirms* wait for the next issue.
- Markdown bodies render with **raw HTML dropped structurally** — anything not
  expressible in markdown silently vanishes, by design.
- A rolling deploy mid-send kills the fan-out task silently; status shows the stale
  heartbeat and recovery is re-POSTing `send`. Don't deploy mid-send.

## Configuration

| Key | Meaning |
|---|---|
| `ROOMLER__NEWSLETTER__FROM_EMAIL` / `__FROM_NAME` | The campaign From (falls back per-field to `ROOMLER__EMAIL__FROM_*`). Kept separate so campaign reputation can't drag the transactional From down. |
| `ROOMLER__EMAIL__API_KEY` or `__SMTP_HOST`+`__SMTP_PORT` | The mail backend (SendGrid / SMTP). Absent ⇒ subscribe still stores rows (unconfirmed), send refuses. |
| `ROOMLER__STATS__PLATFORM_ADMINS` | The admin allowlist (ObjectIds, comma-separated — ids, not emails). |

## Ops prerequisites for a real campaign

- ⚠️ **SPF/DKIM domain authentication for the newsletter From** before the first real
  send — or the campaign lands in spam and drags the transactional From's reputation
  down with it.
- ⚠️ If SendGrid's account-level **subscription tracking** is on, it injects its own
  List-Unsubscribe next to ours — turn it off.
- Hero images ship as versioned files under `ui/public/newsletter-img/<slug>-vN.png`
  (the static-extension nginx location serves them with a 1-year immutable cache —
  never edit one in place, bump the version).
- **No tracking pixels, no open/click analytics** — `source` on the subscriber row stays
  the only analytics, and the email footer says so. This is a product stance (FR-39),
  not a missing feature.
