# FR-88: Measure what promotion brings — attribution to an activated device, a link hub, and video on the site

**Issue:** [#1790](https://github.com/gjovanov/roomler-ai/issues/1790) · **Status:** in progress. P0–P3 merged
and promoted; P4's docs written. 9 of 12 criteria met; AC3, AC8 and AC10 are open (§5) · **Owner:**
web / public site + auth · **Anchors:** master `9ddf0aa6b` ·
**Builds on:** [FR-87](FR-87-blog-and-google-indexing.md) (the static site, purestat on every page),
[FR-60](FR-60-public-docs-site.md) (the generator), [FR-39](FR-39-launch-readiness.md) (`subscribers.source`)

## 1. Goal

Roomler is about to publish short and long videos on YouTube, TikTok, Instagram and Facebook on a
fixed cadence. Today nothing connects any of it to a user: purestat records `utm_*` on the visit,
and the chain stops there. This FR makes one campaign id travel from a post's link to the account
it produced and to that account's first enrolled device, **without storing anything on the
visitor's device**, and gives the videos a home on the site.

1. **Attribution**: the landing page's UTM parameters ride on the site's own links into sign-up
   (email and OAuth) and the newsletter form. A new account records where it came from, plus an
   optional answer to "How did you hear about Roomler?".
2. **Conversions**: purestat goals for `signup`, `subscribe`, `install-copy` and `github-outbound`.
3. **Activation**: a platform-admin view of signups, and of signups whose tenant enrolled a device
   within 7 days, by source and campaign.
4. **A link hub**: `/links/` plus short paths (`/yt`, `/tt`, `/x`, `/fb`) that 302 there with UTM,
   for places a link cannot carry parameters: a profile bio, a URL said out loud in a video.
5. **Video on the site**: a `video:` key on blog posts and a `:::video` container on docs pages,
   rendered as a click-to-load youtube-nocookie facade with `VideoObject` structured data.

The channel program itself (what is posted, where, when, and why) is not engineering and is not in
this spec. Nothing here depends on it.

## 2. Evidence (measured 2026-09-29, production and master `9ddf0aa6b`)

- **purestat is already everywhere.** It loads on the SPA (`ui/index.html:42`) and on every static
  page through the shared shell (`ui/docs/site.ts:152-157`). Its event schema stores
  `utm_source`/`utm_medium`/`utm_campaign`/`utm_content`/`utm_term`, and it has goals and a stats API
  keyed by an `X-API-Key` (purestat's `docs/api.md` and `docs/tracker.md`).
- **Nothing in roomler reads a source.** No hit for `utm_`, `document.referrer` or `ref=` in `ui/`,
  `crates/` or `files/` outside a vendored README. `RegisterRequest`
  (`crates/api/src/routes/auth.rs:16`) and `User` (`crates/db/src/models/user.rs:7`) have no source
  field. The only hook is `subscribers.source` (`crates/db/src/models/subscriber.rs:33`), clamped to
  32 characters by the saas module's subscribe route.
- **OAuth drops everything but its CSRF state.** `oauth_redirect`
  (`crates/api/src/routes/oauth.rs:39-67`, mounted at `GET /api/oauth/{provider}`,
  `crates/api/src/lib.rs:226`) mints a random `state` and binds it with a 10-minute HttpOnly
  `oauth_state` cookie. Nothing else survives the round trip to the provider.
- **No video can be embedded.** The CSP (`files/nginx-pod.conf:87`) has no `frame-src`, so frames
  fall back to `default-src 'self'`; the Permissions-Policy comment (`:47`) records that the site
  "embeds no iframes at all". `ui/docs/theme/structured.ts` emits no `VideoObject`.
- **The accounts that will link here start from zero:** YouTube 0 videos, TikTok 0 posts, Instagram
  0 posts, Facebook 79 followers (a page last used in 2020); GitHub 3 stars.

## 3. Design

### 3a. Attribution: carry, don't store

Nothing is written to the visitor's device for attribution: no cookie, no localStorage, no
sessionStorage. Storing on a terminal for a non-essential purpose needs consent under ePrivacy
Art. 5(3). The site has no consent banner and should not need one, which is also why purestat is
cookie-free. So the parameters travel on links instead:

```mermaid
sequenceDiagram
    participant V as Visitor
    participant S as Static page (shell)
    participant A as SPA register view
    participant API as /api
    participant R as Redis
    participant P as OAuth provider
    V->>S: GET /blog/x/?utm_source=youtube&utm_campaign=c1
    S->>S: append utm_* to own CTA links (allowlist)
    V->>A: click "Sign up" → /register?utm_…
    alt email sign-up
        A->>API: POST /api/auth/register {…, attribution}
    else OAuth sign-up
        A->>API: GET /api/oauth/google?utm_…
        API->>R: SET oauth_attr:<csrf_state> (TTL 600 s)
        API->>P: 307 to provider (state = csrf_state)
        P->>API: GET /api/oauth/callback/google?code&state
        API->>R: GET + DEL oauth_attr:<state>
    end
    API->>API: create user with signup_attribution (once)
```

- **The shell** (`ui/docs/theme/shell.ts`) gets a small first-party script. It is served from the
  same origin, so `script-src 'self'` already allows it, and inline scripts stay blocked. When the
  page URL carries `utm_*` or `ref`, it copies those keys onto the page's own sign-up, install and
  download links, found through an allowlist of selectors, and onto nothing else. *(P1b: the
  allowlist is `/register`, `/login`, the installer downloads, and `INSTALL_PAGES` in
  `ui/docs/site.ts`, which the build writes into the script. The build fails when a page shows an
  install command but is not listed, so where `install-copy` can fire and where a campaign reaches
  are one list.)*
- **The SPA register view** reads the same keys from its own URL and sends them as an optional
  `attribution` object on `POST /api/auth/register` (`crates/api/src/routes/auth.rs:137`).
- **OAuth:** the register view's provider buttons pass the keys to `GET /api/oauth/{provider}`.
  `oauth_redirect` stores them in Redis under the CSRF state it already mints, with the cookie's
  600 s TTL. `oauth_callback` reads and deletes the key when it creates the user. No new cookie.
  ⚠️ The attribution must never influence the CSRF check: a missing or expired key means "no
  attribution", never a failed login.
- **The newsletter form** sends the campaign as the existing `source` (32 characters).
- **Storage:** `users.signup_attribution: Option<SignupAttribution>` with `source`, `medium`,
  `campaign`, `content`, `term`, `referrer_host`, `landing_path` and `self_reported`.
  - Every value is clamped to 64 characters of printable ASCII. Unknown keys are dropped. The
    referrer is a host, never a full URL.
  - ⚠️ `referrer_host` and `landing_path` that ARRIVE on a URL are taken as given, like any
    `utm_*` value: any link can set them, the server only clamps them, and nothing checks them.
    That is a data-quality limit, not a security one. Read the activation view (§3c) with it in
    mind: a count by source is what the links said, not a measurement.
  - It is set once, at creation, and never updated. It is never returned by a user-facing endpoint
    and never logged at `info`.
  - `#[serde(default, skip_serializing_if = "Option::is_none")]`. ⚠️ If any insert path builds its
    BSON by hand with `doc!{}`, it will silently drop the new field; the insert must serialise the
    struct, and a test asserts the round trip.
- **"How did you hear about Roomler?"** is an optional select on the register form: search,
  YouTube, TikTok, Instagram, Facebook, Reddit, Hacker News, a friend or colleague, other. It is
  stored as `self_reported`. It covers what links cannot: a URL said out loud, a screenshot shared
  in a chat, a conversation.
- **Privacy policy:** a section naming the fields, the purpose (which channels bring users), the
  retention (with the account), the legal basis (legitimate interest), and the fact that nothing is
  stored on the device.

### 3b. Conversion goals (purestat)

- `purestat('signup')` after a successful register or OAuth sign-up, `purestat('subscribe')` after
  the newsletter form's 202, `purestat('install-copy')` on the install/enroll command's copy button,
  and `purestat('github-outbound')` on the links to the repository. Each has a goal in purestat.
- purestat attributes a goal to the visit's source. The account-level record (3a) covers a visitor
  who converts on a later visit.
- **Kill switch:** FR-87's `ANALYTICS = null` disables all of it on the static pages; the SPA call
  is a no-op when `window.purestat` is absent.

### 3c. The activation view (platform admin)

- `GET /api/admin/stats/attribution?since=&until=` joins the other platform-admin stats
  (`crates/api/src/lib.rs:283-297`). It returns counts only, never a user list:
  - signups by `source`/`medium`/`campaign` and by `self_reported`;
  - activated signups, where the user's tenant enrolled at least one agent within 7 days of the
    user's `created_at`;
  - `pending` signups — no device yet and the 7-day window still open — reported beside
    `activated`, and `settled` (= `signups − pending`), the only valid denominator for an
    activation rate: a rate over all signups would count yesterday's as failures.
- It is the host's view, like its siblings: users are core, agents belong to `fleet`. With `fleet`
  unmounted it reports `activated: null`, never `0` (`pending` and `settled` likewise).
- Same gate as the siblings: 404 to anyone who is not a platform admin.
- ⚠️ Every bucket `key` is text chosen by whoever built the link — sanitised to ≤ 64 printable
  ASCII characters, but still attacker-chosen. An admin UI renders it as text (`{{ key }}`),
  never as markup (`v-html`).
- The new route changes the composition baseline
  (`crates/tests/fixtures/composition.baseline.json`). Re-record with `COMPOSITION_UPDATE=1` and
  say why in the commit.

### 3d. The link hub and short paths

- **`/links/`** (`ui/docs/theme/links-layout.ts`): a static page built by the same generator
  (`ui/docs/build.ts`) with the same shell, and held to the homepage's link gate.
  - It lists sign-up, install, the docs, the blog, the repository and the channel profiles. P3 adds
    the featured video, as the facade's first page.
  - Its sign-up and install links carry the incoming UTM like every other static page (3a).
  - `noindex, follow`: it repeats the homepage for people who arrive from a profile, so it is not a
    page to rank, and it is not in the sitemap.
- **Channels** (`CHANNELS` in `ui/docs/site.ts`): the one list of the profiles. The hub lists them,
  and `ORG.sameAs` names them to search engines next to the repository.
- **nginx** (`files/nginx-pod.conf`): `location = /yt { return 302 /links/?utm_source=youtube&utm_medium=bio&utm_campaign=profile; }`,
  and the same for `/tt`, `/x` and `/fb`. More short paths are added only by a PR.
  - **2026-10-11: X replaced Instagram.** Instagram disabled @roomler.ai on 2026-10-10, so its
    channel and its `/ig` path left together, and @roomler_ai on X took the slot as `/x`. A short
    path lives only as long as its channel: the spec's both-ways check keeps nginx and `CHANNELS`
    equal.
  - `utm_medium=bio&utm_campaign=profile` is what every profile link of the program carries, a
    profile's own website field included. So a visit through a bio is one source, whichever way it
    came. (This spec first said `utm_medium=profile`; P2 follows the program's convention instead.)
  - 302, not 301: a browser keeps a 301 for good, and where a short path points may change.
  - The redirect is relative, per FR-87's `absolute_redirect off`.
  - A short path must not shadow an SPA route. `ui/docs/__tests__/links-page.spec.ts` checks the
    router's route table for every one, and checks the nginx lines against `CHANNELS` both ways.
  - No `add_header` in these locations (§3e explains why).
- **Kill switch:** remove the locations.

### 3e. Video on the site

- **Authoring:** a `video:` key (a YouTube id) is added to `BLOG_FRONTMATTER_KEYS`
  (`ui/docs/site.ts:113`), and docs pages get a `:::video <id>` container.
- **Rendering:** both produce a facade, with no request to any YouTube host before the click.
  - It is a thumbnail (`https://i.ytimg.com/vi/<id>/hqdefault.jpg`, already allowed by
    `img-src https:`) with a play button.
  - A click swaps in `https://www.youtube-nocookie.com/embed/<id>?autoplay=1`.
- **Structured data:** `ui/docs/videos.json` holds each video's id, title, description, upload date,
  duration and thumbnail. It feeds a `VideoObject` in the page's JSON-LD graph
  (`ui/docs/theme/structured.ts`).
  - Per FR-60's rule (build gates, not lints), a `video:` or `:::video` id missing from the manifest
    fails the build.
- **CSP:** add `frame-src https://www.youtube-nocookie.com` to the **server-level** header
  (`files/nginx-pod.conf:87`), and update the Permissions-Policy comment (`:47`).
  - ⚠️ Never add it in a `location`: any `add_header` there drops every inherited security header
    (the FR-60 finding, `:112-118`).
  - Adding a source only widens the policy. The remote-control viewer is still exercised after the
    change, because the 2026-07-29 CSP regression broke the viewer's loopback probes and not the SPA.
- **Kill switch:** an empty manifest renders no facade; revert the `frame-src`.
- **Privacy policy:** the embed is click-to-load, and YouTube's terms apply after the click.

**As built (P3, 2026-10-15)**, where it differs from the design above:

- **The manifest is `ui/src/utils/videos.ts`, not `ui/docs/videos.json`.** The SPA embeds the
  same videos (the tutorial's first chapter and `LandingView.vue`, Goran's ask alongside the
  homepage and the docs page), so the list is a module both builds import, as `landing.ts` is.
- **The poster is our own copy, not `i.ytimg.com`.** A thumbnail from `i.ytimg.com` is a
  request to a Google host on page load, which the first bullet rules out. The poster lives in
  `ui/src/assets/tutorial/`, which the docs build already searches and the SPA imports from.
- **Docs only** get `:::video <id>`; the blog's `video:` front-matter key is not built. A post
  can use the container, but adds no `VideoObject` yet.
- **`/links/` got no facade.** §3d's featured video on the hub was not built: the hub stays a
  list of links.
- **Proof:** the built site served locally, YouTube blocked at the browser: 0 requests to a
  YouTube host before the click, then `youtube-nocookie.com/embed/<id>` on the click, on the
  homepage and the docs page (`ui/docs/__tests__/video.spec.ts` runs `docs.js` the same way).

## 4. Phases

| P | What | Kill switch | Status |
|---|---|---|---|
| P0 | claim: issue #1790, this spec, the ledger row | docs only | **merged** #1791 `91c02f6f5` |
| P1a | the server half of §3a — `RegisterRequest.attribution`, the `utm_*` query on `GET /api/oauth/{provider}` and its Redis parking spot, `users.signup_attribution` — and the §3c view | the SPA omits `attribution`; the view is platform-admin only | **merged** #1800 `ca755310e`, promoted `hosted-20260930-60fe7a7` |
| P1b | the site and SPA half of §3a (the shell script, the register view and its select, the provider buttons, the newsletter form), the goals (§3b), the privacy-policy section, and the `/landing` and `/pricing` redirects in nginx that now keep the query | `ATTRIBUTION_ENABLED = false` in `ui/src/utils/attribution.ts`; `ANALYTICS = null` | **merged** #1798 `78a2ea096`, promoted `hosted-20260930-60fe7a7` |
| P2 | `/links/` and the short paths (§3d) | remove the locations and the page | **merged** #1803 `1dd1d08be`, promoted `hosted-20261001-37fa997`; X took Instagram's place in #2007 `405e5724d`, promoted `hosted-20261011-ffa860c` |
| P3 | video on the site (§3e, as built below it): the homepage, `/docs/start/what-is-roomler/`, the SPA landing and the tutorial's first chapter, with `docs/public-site.md` §7 | empty manifest; revert `frame-src` | **merged** #2262 `1c76d5fb4`, promoted `hosted-20261015-75a88bf` |
| P4 | docs: `docs/public-site.md` gains the attribution flow (§9) and the link hub (§10), beside the video facade (§7), with mermaid; the field log; and the privacy policy's click-to-load paragraph, which §3e asked of P3 | revert the paragraph | written; the paragraph reaches production with the next promote |

**Deploy order:** P1 is promoted before the first video is published, so the first posts are
measured from their first view. Each phase is its own promote.

## 5. Acceptance criteria

- [x] **AC1:** a production visit to `/?utm_source=youtube&utm_medium=video&utm_campaign=fr88-test`
  appears under that campaign in purestat's stats API. A visit without UTM appears as direct
  (control).
  **Met 2026-10-31 on real traffic instead of the `fr88-test` probe,** which would have added a
  fake YouTube visit to the data this FR is judged on: §8's 2026-10-31 row, after the data-level
  read in [the 2026-09-30 step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-5920437758).
- [x] **AC2:** an email sign-up on a local dev server, reached through a UTM landing, stores
  `signup_attribution` with those values. The same sign-up without UTM stores none (control). The
  field is absent from every user-facing response.
  **Met on production, which is stronger than local dev:** the operator's two tagged email sign-ups
  stored their values ([FR-102 §8](FR-102-growth-observability.md), 2026-10-18), and §8's
  2026-10-31 row reads the rest. The field's absence is
  `the_attribution_reaches_no_user_facing_response`, green on
  [master](https://github.com/gjovanov/roomler-ai/actions/runs/41193085304).
- [~] **AC3:** an OAuth sign-up on local dev (one provider), reached through a UTM landing, stores the
  same values. The Redis key is gone after the callback and expires after 600 s if the callback
  never comes. No new cookie is set (response-header diff against master). An expired key still
  logs the user in.
  **Proven in parts.** The parking, its TTL, the read-and-delete, the one cookie and a sign-in with
  no key are `oauth_redirect_parks_the_attribution_under_the_csrf_state_with_no_new_cookie`,
  `oauth_attribution_is_applied_only_when_the_account_is_created` and
  `oauth_completion_redirects_with_a_signup_marker_and_no_token`, green on
  [master](https://github.com/gjovanov/roomler-ai/actions/runs/41193085304). On production, tagged
  starts reach `/api/oauth/google` ([FR-102 §8](FR-102-growth-observability.md), 2026-10-18) and
  provider sign-ups create accounts. **Owed:** one run end to end, which takes a provider identity
  with no Roomler account, through a tagged link. An existing identity just signs in and stores
  nothing, which is correct and looks like a failure.
- [x] **AC4:** no attribution value is written to cookies, localStorage or sessionStorage on `/`, a
  docs page, a blog post, `/links/` or the register page (a Playwright check over all five).
  **Met 2026-10-30:** one Playwright pass over all five on production, §8.
- [x] **AC5:** `GET /api/admin/stats/attribution` returns signups and activated signups by source for
  a platform admin, and 404 for anyone else. With `fleet` unmounted, `activated` is `null`.
  **Met:** a platform admin's 200 with those fields on production
  ([step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-5915258939)). A
  signed-in non-admin's 404 and the `null` are `admin_attribution_view_counts_signups_and_activations`
  and `admin_attribution_view_reports_null_activation_without_fleet`, green on
  [master](https://github.com/gjovanov/roomler-ai/actions/runs/41193085304). Anonymous gets the auth
  layer's 401, like every sibling.
- [x] **AC6:** a scripted pass records exactly one event for each of the four purestat goals, and zero
  with `ANALYTICS = null` (control).
  **Met 2026-10-31:** a scripted pass over the served production pages, with and without the
  tracker, §8. That `ANALYTICS = null` leaves the tracker off every page is
  `ui/docs/__tests__/analytics-off.spec.ts`.
- [x] **AC7:** `curl -sI https://roomler.ai/yt` (and `/tt`, `/x`, `/fb`) returns a 302 whose relative
  `Location` is `/links/` with that channel's `utm_source`, `utm_medium=bio` and
  `utm_campaign=profile`, and no SPA route answers any of those paths. `/links/` answers 200 with the
  hub and `/links` a 301 to it. Checked by `scripts/public-site-smoke.sh` §5c, shown failing on the
  deploy before P2 (§8).
  **Met:** red then green on production for P2
  ([step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-5925427841)) and for
  X (§8, 2026-10-11 and -12), and still green on 2026-10-30 (§8).
- [~] **AC8:** a blog post with `video:` makes no request to a YouTube host before the click (network
  log), plays after it, passes the Rich Results Test with a `VideoObject`, and keeps Lighthouse at
  100 in all four categories. A `video:` id missing from the manifest fails the build, shown failing
  and then passing.
  **Partly met, by the docs container only.** `:::video` on a docs page makes no request to a
  YouTube host before the click and plays after it
  ([step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-6203128624)). The
  blog's `video:` key is not built (§3e, as built), and the Rich Results Test, Lighthouse and the
  build gate's red have not been read. What happens to this criterion is the operator's to decide.
- [x] **AC9:** the served CSP carries `frame-src https://www.youtube-nocookie.com` on the SPA, a docs
  page and a post, every other security header is unchanged (header diff), and a remote-control
  session still connects with its loopback probes unblocked.
  **Met:** `frame-src` before and after, and `connect-src` byte-identical on the remote-control page
  ([step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-6203128624)). The SPA,
  a docs page, a post and the header diff are §8's 2026-10-30 row. A real remote-control session
  under this policy connected with 0 violations
  ([#1100](https://github.com/gjovanov/roomler-ai/issues/1100#issuecomment-6256710513)).
- [~] **AC10:** the privacy policy names the attribution fields, their purpose and retention, and the
  click-to-load embed.
  **Partly met.** Section 2.16 names the fields, the purpose and the retention, on production since
  P1b. The embed was missing (§8, red on 2026-10-30). P4 adds it to section 2.17; tick this when
  production serves it.
- [x] **AC11:** docs updated with mermaid diagrams (`docs/public-site.md`: the attribution flow, the
  link hub, the video facade) and indexed in `docs/README.md`.
  **Met:** [`docs/public-site.md`](../public-site.md) §9 (the attribution flow), §10 (the link hub)
  and §7 (the video facade), each with its diagram, and its row in [`docs/README.md`](../README.md).
- [x] **AC12:** the SPA's old marketing routes keep a campaign: on production,
  `/pricing?utm_source=x` answers `301` to `/?utm_source=x#pricing` and `/landing?…` to `/?…`,
  and without a query both answer exactly as before (`/#pricing`, `/`). Checked by
  `scripts/public-site-smoke.sh`, shown failing on the deploy before P1b (§8).
  **Met:** red then green on production
  ([step log](https://github.com/gjovanov/roomler-ai/issues/1790#issuecomment-5915258939)), and
  still green on 2026-10-30 (§8).

## 6. Open decisions

Decisions 1–3 shipped with their defaults in P1. Decision 4 is open.

1. **Where the OAuth attribution waits:** the existing Redis with a 600 s TTL (default), or a Mongo
   collection with a TTL index.
2. **When the self-reported question is asked:** on the register form, optional (default), or after
   the first login.
3. **The option list** for the self-reported question (the default is in §3a).
4. **What "the user's tenant" means for activation** (raised by the P1a review). P1a credits a
   signup when ANY org the person belongs to at query time enrolled a device within the window —
   including an org they were invited into, and regardless of when they joined it. The
   alternatives: only memberships joined within the window, or only the org created at
   registration. The definition is unchanged until the operator decides; the join is one
   function (`tally` in `crates/api/src/routes/attribution.rs`).

## 7. Out of scope

- **The channel program** (what is posted, where, when, keyword targets). It is not engineering and
  lives in a private annex. This spec stands alone without it.
- **Ads, ad pixels and any third-party tracking script;** cookies of any kind for attribution;
  fingerprinting; multi-touch attribution; server-side events to purestat.
- **Conference-call recording.** `ui/docs/content/collaboration/video-conferencing.md:21` claims
  "Record a call", but `crates/modules/conference/src/recording.rs:84-107` only inserts a metadata
  row (`url` empty, `size` 0). The doc is corrected in a separate PR.

## 8. Field-verification log

| Date | Build | What was checked | Result |
|---|---|---|---|
| 2026-09-30 | production, before P1b (`/health` 0.4.114) | AC12's red run: `curl -sI` on `/pricing?utm_source=smoke` and `/landing?utm_source=smoke&utm_campaign=fr88`; the smoke's new redirect block | **fails as expected**: `Location: /#pricing` and `Location: /`, the query dropped; the smoke block reports 2 of 4 red. The same block against `files/nginx-pod.conf` in `nginx:stable` (Docker): master's file 2 of 4 red, PR #1798's 4 of 4 green, `nginx -t` clean, and no query still gives exactly `/` and `/#pricing` |
| 2026-09-30 | production, before P2 (`hosted-20260930-e7e7fad`, `/health` 0.4.114) | AC7's red run: `scripts/public-site-smoke.sh https://roomler.ai` with the §5c block P2 adds | **fails as expected**: `/yt`, `/tt`, `/ig` and `/fb` each answer **200 with the SPA shell** (a soft 404), `/links` answers 200 instead of a 301, `/links/` is not the hub, and `/links/fr88-smoke-missing/` answers 200. The same smoke against the P2 build, served by `files/nginx-pod.conf` in `nginx:stable` (Docker): every check green, `nginx -t` clean, and each short path answers `302 Location: /links/?utm_source=<channel>&utm_medium=bio&utm_campaign=profile` |
| 2026-10-11 | production, before #2007 (`hosted-20261011-9443077`, `/health` 0.4.126) | the red run for X taking Instagram's place (`/ig` → `/x`): `scripts/public-site-smoke.sh https://roomler.ai` with #2007's §5c pairs | **fails as expected**: `/x -> 200`, the SPA shell answering; `/yt`, `/tt` and `/fb` 302 to `/links/`; no other check red |
| 2026-10-12 | production, after #2007 (`hosted-20261011-ffa860c`, promoted 2026-10-11 20:48Z, `/health` 0.4.127) | the same smoke, from master | **passes**, every check: `/x` → `302 Location: /links/?utm_source=x&utm_medium=bio&utm_campaign=profile`. `/ig` now answers 200 with no redirect, as the both-ways rule requires. `/links/` lists YouTube, TikTok, X (@roomler_ai) and Facebook, with no instagram.com link left |
| 2026-10-30 23:10–23:30Z | production `hosted-20261030-9336d4b` (`/health` 0.4.189) | the smoke (AC7, AC12); the served headers (AC9); a Playwright pass over the five pages (AC4); the served privacy policy (AC10) | **Green, and one red.**<br>• **Smoke:** `scripts/public-site-smoke.sh https://roomler.ai` passes every check.<br>• **AC9:** `/login`, `/docs/start/what-is-roomler/`, a post and `/` each serve `frame-src https://www.youtube-nocookie.com`, with the same security headers on all four. The pod's seven headers are byte-identical to master's `files/security-headers.conf`, and P3's diff (`1c76d5fb4`) changed only the CSP line and its comments.<br>• **AC4:** six tagged parameters on all five pages in one browser: 0 attribution values, no storage key and no cookie at all. Each page carried the campaign onto its own links (14, 16, 9, 6 and 1), so its script saw the query. purestat's tracker was requested and aborted on each page, a planted value was seen in all three stores, and the tracker's source only reads one opt-out flag.<br>• **AC10's red:** the served `PrivacyPolicyView` chunk holds 2.16's phrases and nothing about the embed (`youtube-nocookie`, `press play` and `embedded`, 0 each) |
| 2026-10-31 17:39–17:55Z | production `hosted-20261031-28dfd72` (`/health` 0.4.190); read-only, bounded Mongo | AC1, AC2, AC3 and AC6; whether an attributed account has reached a device | **Green on the mechanism; the goal has no field datum yet.**<br>• **AC1:** `growth_digests` holds the server's own reads of purestat's query API for 2026-10-13…10-30. On 17 of those 18 days the campaign rows are measured: real visits under `youtube/video/<campaign>`, and under LinkedIn, Facebook, X and `chatgpt.com`. Every day has a `direct` channel (the control).<br>• **AC2:** among accounts created since P1's promote, the field exists only on those that came through a tagged link or answered the question, one of them an organic `utm_source=chatgpt.com` sign-up. Every other account has no key (the control).<br>• **AC3:** no provider account carries the field. That is what the carry predicts unless a tagged visitor picks a provider, so it proves nothing either way, and the criterion stays open.<br>• **AC6:** the real tracker on the served pages, every beacon caught in the browser, the two writes stubbed (no account, no subscriber, no purestat row): exactly 1 `install-copy`, `github-outbound`, `subscribe` and `signup`. With the tracker blocked: 0 of each, and all four flows still complete. The same digests show all four goals on real traffic, `subscribe` included.<br>• **The goal:** every account carrying `signup_attribution` was created before the activation link worked (#3029, fixed by #3041, on production from `hosted-20261029-2ae4ffd`), so none could sign in, make an org or enroll a device. The same join finds orgs for other accounts, so the zero is real. No email sign-up has happened since the fix, and one that misses the 5-minute link is still locked out (#3199).<br>• `mongodb-0` shows no new restart after the reads |

## 9. Related

- [FR-87](FR-87-blog-and-google-indexing.md) (#1776): the static site, the shell, purestat on every page.
- [FR-60](FR-60-public-docs-site.md): the generator and its build gates.
- [FR-39](FR-39-launch-readiness.md): `subscribers` and its `source`.
- [FR-69](FR-69-modular-monolith.md): the composition baseline and the host-owned views.
- [FR-85](FR-85-hq-screen-recording.md) (#1634): the recorder the videos are made with.
- [FR-91](FR-91-scheduled-blog-publishing.md) (#1880): §3e, the organic carry for a post reached from
  another site.
- [FR-102](FR-102-growth-observability.md) (#2127): the Campaigns tab, this account plane beside
  purestat's web plane.
- #3029 and #3199: why an email sign-up could not activate, and what still locks one out.
