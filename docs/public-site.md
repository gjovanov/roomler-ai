# The public site: `/`, `/docs`, `/blog` and what a crawler reads

**Audience:** whoever changes `ui/docs/`, `ui/blog/`, the landing copy, the pod's nginx config,
or anything a search engine sees. Built by [FR-60](fr/FR-60-public-docs-site.md) (#1165, the
docs generator) and [FR-87](fr/FR-87-blog-and-google-indexing.md) (#1776, the blog, honest
dates, hashed assets, the head and the static homepage). What promotion brings is measured
here too: [FR-88](fr/FR-88-promotion-attribution-and-site-video.md) (#1790) added the video
facade (§7), the campaign's carry (§9) and the link hub (§10).

The product is a single-page app that a crawler reads as one empty `<div id="app">`.
Everything a search engine should find (the homepage, the user documentation, the blog) is
**static HTML** built by one in-repo generator into `ui/dist/`, next to the SPA, and served by
the same pod nginx. No dependency was added for any of it: the generator is `markdown-it`
(already a UI dependency) plus hand-written TypeScript.

> ⚠️ **CI green is not "Google can read it".** Every defect FR-87 fixed was invisible from
> inside the build: dates that changed on every deploy, a redirect that downgraded to
> `http://`, soft 404s, a year of `immutable` cache on files that changed. They are caught now
> by [`scripts/public-site-smoke.sh`](../scripts/public-site-smoke.sh) against the image in CI
> **and** against production after a promote (§6).

## 1. One generator, two collections and a homepage

```mermaid
flowchart LR
    subgraph src["ui/ (all the Docker UI stage sees: COPY ui/ .)"]
        C["docs/content/**/*.md<br/>65 pages, 10 sections"]
        P["blog/posts/*.md<br/>published posts only"]
        L["src/utils/landing.ts<br/>src/utils/enrollCommands.ts<br/>(shared with the SPA)"]
        A["docs/assets · blog/assets<br/>src/assets/tutorial"]
        M["docs/content-dates.json<br/>(generated, gitignored)"]
    end
    B["ui/docs/build.ts<br/>load → gates → emit"]
    subgraph dist["ui/dist/ (next to the SPA)"]
        H["home/index.html<br/>(served at /, §5)"]
        D["docs/**/index.html<br/>docs/404.html"]
        BL["blog/**/index.html<br/>blog/feed.xml"]
        AS["docs/assets/name.&lt;hash&gt;.ext"]
        X["sitemap.xml (index)<br/>sitemap-docs.xml · sitemap-blog.xml<br/>robots.txt"]
    end
    C & P & L & A & M --> B --> H & D & BL & AS & X
```

`bun run build` is `vue-tsc && vite build && bun docs/build.ts`. **The generator must run
after `vite build`**, which empties `dist/`. CI runs the three separately so each failure gets
its own annotation ([`ci.yml:2357`](../.github/workflows/ci.yml)).

| Collection | Source | URL | Dated from |
|---|---|---|---|
| Docs | `ui/docs/content/<section>/<page>.md` | `/docs/<section>/<page>/` | git (§2) |
| Blog | `ui/blog/posts/<slug>.md` | `/blog/<slug>/` | front-matter only: editorial dates, because a typo fix is not a new post |
| Homepage | `ui/src/utils/landing.ts` (the SPA's `LandingView.vue` imports the same module) | `/`, for requests without a session cookie (§5) | undated |

Both share the page shell ([`theme/shell.ts`](../ui/docs/theme/shell.ts)), the link checker,
the search index, the asset pipeline and the sitemaps. A second builder was rejected in the
FR-87 plan: two writers of `sitemap.xml`, and a link checker that could not see a blog → docs
link.

**Every gate fails the build**, with every problem listed in one run
(`build.ts:main`, [`build.ts:498`](../ui/docs/build.ts)):

| Gate | Where |
|---|---|
| a link to a page nobody generates, or to a heading a page does not have (same-page `#anchor`s included) | [`theme/links.ts:31`](../ui/docs/theme/links.ts) |
| front-matter: a missing key, an unknown key (strict), a description over 160, a `<title>` that does not fit 60 | `build.ts:loadPage` ([`:259`](../ui/docs/build.ts)) |
| the homepage: a link to a page nobody generates; an SPA icon with no inline SVG mapped | `build.ts:main`, [`theme/home-layout.ts:63`](../ui/docs/theme/home-layout.ts) |
| images: no alt text, a remote or `data:` src, a raw `<img>`, over 3 MB, a format nobody can size | [`render.ts:475`](../ui/docs/theme/render.ts), [`theme/images.ts:139`](../ui/docs/theme/images.ts) |
| posts: no `draft` (a file there IS published), no future date, `updated` before `date`, a share image under 1200 px or not raster | [`theme/posts.ts:81`](../ui/docs/theme/posts.ts) |
| the search index over 150 KB gzipped | `build.ts:main` |

## 2. Dates: honest, or absent

Crawlers read `<lastmod>` and `dateModified` as "when the content changed". Before FR-87 every
production page claimed the day of the build: `.dockerignore` drops `.git` and CI clones one
commit, so the git lookup always fell back to "today" (**70 of 70** sitemap URLs, measured
2026-09-28, against content last changed 2026-09-01). A crawler that catches `lastmod` lying
stops trusting it for the whole site.

```mermaid
flowchart TD
    FM{"front-matter<br/>updated:?"} -->|yes| USE["use it"]
    FM -->|no| G{"git history here,<br/>and NOT shallow?"}
    G -->|yes| GIT["git log %cI<br/>(committer time + zone)"]
    G -->|"no (Docker, a depth-1 clone)"| MAN{"ui/docs/content-dates.json?"}
    MAN -->|yes| MF["the manifest"]
    MAN -->|no| NONE["NO date:<br/>no lastmod · no dateModified<br/>no 'Last updated'"]
```

- **Where the manifest comes from:** `hosted-image.yml` checks out full history and runs
  `bun docs/dates.ts --write` before the Docker build
  ([`hosted-image.yml:113`](../.github/workflows/hosted-image.yml)); the build context is a
  path, so the untracked file reaches `COPY ui/ .`. The CLI **refuses** to write from a shallow
  clone ([`dates.ts:106`](../ui/docs/dates.ts)): a depth-1 clone's one commit "touches" every
  file, which would stamp HEAD's date everywhere, the same lie one step removed.
- **Timestamps, not dates.** JSON-LD and `article:*_time` carry git's `%cI`
  (`2026-09-01T23:58:15+02:00`). Google's Rich Results Test flags a bare `YYYY-MM-DD` as an
  "invalid datetime … missing a timezone". The sitemap and the page footer use the date part,
  which is exactly `git log -1 --format=%cs`.
- **Generated pages** (section and tag indexes) have no source file, so they have no date.

> ⚠️ **The break-glass build-host path must run the manifest step too** (`ship-it` §4,
> [deployment.md](deployment.md)). Without it the image publishes no dates at all. That is
> honest, but it is a regression, and the smoke's `lastmod` check reports it.

## 3. Assets: names that change when the bytes do

nginx serves every `.css`/`.js`/image with `Cache-Control: public, immutable` and a one-year
expiry (the `\.(?:js|css|…)$` location in `files/nginx-pod.conf`). That promise only holds for
a name that changes with its content, so every file the generator publishes is
`name.<sha256:10>.ext` ([`theme/assets.ts:43`](../ui/docs/theme/assets.ts)). The HTML is
revalidated on every load (`expires -1`, §5), and it is what moves a reader to new bytes.

- The search index is hashed too; its URL reaches `search.js` through `data-search-index`.
- The top bar's logo is hashed the same way: the generator publishes the SPA's own
  `ui/src/assets/brand/roomler-logo.svg` (`logo` in `SiteAssets`), so the two never drift.
- `social-preview.png` keeps a stable name: `ui/index.html` names it as the SPA's `og:image`.
  It is final artwork from the brand kit, as is the repo root's copy that GitHub shows; there
  is no source to re-render it from.
- **The favicon set keeps root paths** (`ui/public/`: `favicon.svg`, `favicon.ico`,
  `apple-touch-icon.png`, the manifest and its icons), because browsers ask for
  `/favicon.ico` and `/apple-touch-icon.png` on their own. ⚠️ Those names fall under the same
  year-long `immutable` rule, so a returning visitor keeps an old icon after its bytes change.
  Every link to one of them carries a `?v=`, and a file whose bytes change bumps its `v` wherever
  it is linked. Since the second logo (2026-10-12): `favicon.svg?v=3`; `favicon.ico`,
  `apple-touch-icon.png` and the manifest's four icons `?v=2` (`ui/index.html`,
  `theme/shell.ts`, `ui/public/site.webmanifest`); the social card `?v=2` (`OG_IMAGE` in
  `site.ts`, the blog card in `build.ts`, the SPA's og/twitter tags); `logo.svg?v=2` (the
  JSON-LD Organization logo). The manifest itself is not under the rule, so its link has none.
- `LEGACY_UNHASHED_ASSETS` ([`site.ts:38`](../ui/docs/site.ts)), when on, also publishes each
  file under its old plain name, for HTML a browser cached before revalidation began. Pre-FR-87
  HTML carried no `Cache-Control` at all, so a browser could hold it under heuristic caching
  (about 2.8 days). The flag has been **off since 2026-10-04** (FR-87 §4c): only hashed names
  are published.
- **Images carry their real size**, read from the file's own header (PNG, GIF, JPEG, WebP, SVG
  parsed by hand), so the browser reserves the right box. FR-60 hard-coded 960×420 on heroes
  that are 760×400 or 600×540. An image alone in its paragraph becomes a `<figure>`, its
  markdown title the caption.

## 4. What the `<head>` says

Built once, in [`theme/shell.ts:91`](../ui/docs/theme/shell.ts), for every collection.

| | Docs page | Docs listing (home, section, tag) | Post | Blog index | Homepage `/` |
|---|---|---|---|---|---|
| `<title>` | ≤60: the author's `seoTitle`, else the first default that fits (with the section, without it, bare) — [`layout.ts:114`](../ui/docs/theme/layout.ts) | same | `seoTitle`, else `title — Roomler blog` if it fits | fixed, ≤60 | fixed, ≤60 (a unit test) |
| `og:type` | `article` | `website` | `article` | `website` | `website` |
| JSON-LD (one `@graph`) | Organization + `TechArticle` + BreadcrumbList (+ FAQPage) | Organization + `CollectionPage` | Organization + `BlogPosting` (a **Person** author) + BreadcrumbList | Organization + `Blog` | Organization + `WebSite` |
| share image | the social card | the social card | the post's own, raster, ≥1200 px | the social card | the social card |

> ⚠️ **No `SoftwareApplication` on the homepage.** Its rich result requires `aggregateRating`
> or `review`, and a page without them reports the markup as an error. Ratings are never
> invented to make it pass; `home.spec.ts` fails if the type appears.

- **One Organization** (`@id https://roomler.ai/#organization`, legalName **G ROX EOOD**) is a
  node of every graph, and every author and publisher points at it. FR-60 wrote "G ROX LTD".
- Indexable pages ask for `max-image-preview:large`; the rest get `noindex, follow`.
- `/sitemap.xml` is a **sitemap index** over `sitemap-docs.xml` and `sitemap-blog.xml`, so
  Search Console reports each collection on its own. FR-60's sitemap also listed five SPA
  routes whose served canonical is `/`, a contradiction Search Console reports as "Duplicate,
  submitted URL not selected as canonical". They are gone.
- **Analytics — on the hosted site only** ([FR-102](fr/FR-102-growth-observability.md) §3c,
  #2127). No page carries purestat's tag. Each carries a same-origin **loader**,
  [`theme/analytics.js`](../ui/docs/theme/analytics.js), content-hashed like every theme file
  (and into the blog lane's own `/blog/assets/`), whose configuration rides on its own
  `<script>` tag as `data-src` / `data-domain` / `data-hosts`, written by the shell from
  `ANALYTICS` ([`site.ts:215`](../ui/docs/site.ts)); there is no inline script, so the CSP is
  unchanged (`script-src 'self' https://purestat.ai` already covers both). The loader injects
  the tracker only when **all three** hold: the page's hostname is on the allowlist exactly
  (`roomler.ai`, `www.roomler.ai`; no subdomain, no fork, no preview origin), the page is
  `https:`, and `navigator.webdriver` is not set (a Playwright run is not a visit). The SPA
  applies the same rule from `main.ts` through
  [`ui/src/utils/analytics.ts`](../ui/src/utils/analytics.ts), in place of the tag
  `ui/index.html` used to carry, and `site.ts` takes its values from that module so the two
  cannot drift. The reason: the product promises no telemetry from self-hosted deployments
  (`LICENSING.md`), and before this every self-hosted install, fork and `localhost` run beaconed
  as `roomler.ai` — 42 % of the site's events over 90 days. The loader reads and writes nothing
  on the visitor's device. `ANALYTICS = null` still turns it all off: no loader, no tag.

## 5. How nginx serves it

```mermaid
flowchart TD
    R["request"] --> L1{"path"}
    L1 -->|"/ (exactly)"| CK{"access_token<br/>cookie?"}
    CK -->|"none: guests, every crawler"| HOME["home/index.html<br/>expires -1"]
    CK -->|present| APP["the SPA (index.html)<br/>expires -1"]
    L1 -->|"/home/…"| INT["internal: 404<br/>(one URL for the homepage)"]
    L1 -->|"/landing, /pricing"| MV["301 → /, /#pricing"]
    L1 -->|"/docs/…, /blog/…"| S["static handler<br/>expires -1 (no-cache HTML)"]
    S -->|file exists| OK["200 + the server's security headers"]
    S -->|"a slashless directory"| RD["301 Location: /docs/x/<br/>(relative: absolute_redirect off)"]
    S -->|missing| NF["404 + /docs/404.html"]
    L1 -->|"*.css *.js *.svg *.png …"| IM["expires 1y, immutable<br/>(content-hashed names)"]
    L1 -->|"/blog/feed.xml"| F["application/atom+xml"]
    L1 -->|"anything else"| SPA["the SPA (index.html)"]
```

Config: the cookie `map` at [`files/nginx-pod.conf:12`](../files/nginx-pod.conf) (it must
live at `http` level, which is where `conf.d/` files land), `absolute_redirect off` at
[`:32`](../files/nginx-pod.conf), and the locations from [`:129`](../files/nginx-pod.conf)
(`/docs/`) to [`:165`](../files/nginx-pod.conf) (`/landing`).

**The homepage is chosen by the session cookie.** `map $cookie_access_token $root_doc` picks
`/home/index.html` when the HttpOnly session cookie is absent and the SPA's `index.html` when
it is present, and `location = / { expires -1; try_files $root_doc =404; }` serves it.
`try_files` serves the chosen file *inside* that location, so both branches get `no-cache` and
the server's security headers, and `internal` on `/home/` does not block it.

> ⚠️ **The access cookie is not the whole session.** It lives 7 days. The refresh cookie lives
> 30, but it is scoped to `Path=/api/auth/refresh`, so nginx never sees it at `/`: a returning
> user on day 8 would get the sign-up page from a site that could still sign them in.
> [`home.js`](../ui/docs/theme/home.js) closes the gap. It loads in `<head>`, without
> `defer`, and when the SPA's localStorage hint (`roomler-signed-in`, from
> `ui/src/api/session.ts`) says signed in, it replaces the page with `/login`. The SPA's
> guest guard sends a signed-in visitor on to the dashboard, whose first 401 refreshes the
> session.
> - **Never `/`:** without the cookie, nginx would serve the static page again.
> - **A stale hint cannot loop:** a failed refresh clears it and shows the login page.
> - **A crawler has no localStorage**, so it stays on the page.
>
> `home.spec.ts` locks the key to the one the SPA writes by *running* `home.js` after the
> SPA's own `markSignedIn()`.

> ⚠️ **Never navigate the SPA fully to `/` to "show the homepage".** The SPA only runs at `/`
> when the request carried a cookie, so for a cookie the server refuses, a full navigation to
> `/` returns the SPA, which navigates to `/` again, and so on. The router keeps its
> client-side push to `/landing` (`LandingView`, the same copy), which cannot loop. Logout
> already expires both cookies on the server.

> ⚠️ **`expires` in these locations, never `add_header`.** nginx inherits the server-level
> `add_header` list only when a location declares none, so one `add_header` would silently drop
> CSP, HSTS, X-Frame-Options and the rest for every page. `expires` is merged separately.
> The smoke compares each page's security headers with `/`; a negative control (one
> `add_header` in `/docs/`) fails it.

> ⚠️ **No `try_files` in `/docs/` or `/blog/`.** FR-60's `try_files $uri $uri/index.html =404`
> made `/docs/start` a 200 duplicate of `/docs/start/`: its `$uri/index.html` test matches the
> file, so nothing ever redirects. Without `try_files`, the static handler 301s the slashless
> form. (`try_files $uri $uri/ =404` also 301s, measured, but adds nothing.)

## 6. Proving it: the public-site smoke

[`scripts/public-site-smoke.sh`](../scripts/public-site-smoke.sh) `<base-url> [<repo> [<rev>]]`
checks the **served** site. It runs against every image in `hosted-image.yml`
([`:209`](../.github/workflows/hosted-image.yml)) and belongs against production after a promote:

```bash
bash scripts/public-site-smoke.sh https://roomler.ai . <deployed-sha>
```

| Check | Catches |
|---|---|
| `/docs`, `/docs/start` → 301 to the slash form, relative | the `http://` downgrade; duplicate URLs |
| missing `/docs/…`, `/blog/…` → 404 with the site's 404 page | soft 404s |
| docs HTML `Cache-Control: no-cache` | HTML cached against hashed assets |
| security headers on `/docs/`, a leaf, `/blog/` == `/` | the `add_header` trap |
| with posts: the feed is `application/atom+xml`; without: `/blog/` is a real 404 | the Atom type; the kill switch |
| `/` without a cookie is the static homepage (its H1 + `WebSite` JSON-LD); with `access_token` it is the SPA | a crawler reading the SPA shell; signed-in users losing the app |
| `/`'s security headers are the same with and without the cookie, and it is `no-cache` | the `add_header` trap at `/` |
| `/home/` → 404; `/landing` → 301 `/`; `/pricing` → 301 `/#pricing` | a second URL for the homepage; the SPA's old routes |
| every `/docs/assets/` file `/`, `/docs/` and a leaf name is hashed and loads; every `<img>` has a size | stale caches; layout shift |
| given a full-history repo, every docs `<lastmod>` equals git's | dates from the build clock |
| given the repo, `/<key>.txt` serves the IndexNow key exactly as committed (§8) | a key file that stopped shipping: `location /` would answer with the SPA shell, and every engine would reject every submission |

Each check was shown failing before it passed. Against production before FR-87 it failed on
all of them, and an image built with no manifest fails the `lastmod` check. Production before
P6 fails the six homepage checks. Eight mutations of the P6 nginx lines each turn their
check red, among them one `add_header` in `location = /`, which strips every security header
from both branches (FR-87 §9).

## 7. Writing for it

- **A docs page:** a file under `ui/docs/content/<section>/`; the nav is derived from the files,
  never hand-listed. Keys: `title`, `description`, `tags`, `order`, `hero`, `heroAlt`,
  `noindex`, `faq`, `updated`, `seoTitle`.
- **A post:** `ui/blog/posts/<slug>.md`. The contract is [`ui/blog/README.md`](../ui/blog/README.md).
  **A file there is published**; there are no drafts. Unpublished copy lives in the private
  promo repo (FR-39: post copy stays out of this public repo).
- **A post also on Medium:** publish it here first; set `syndication:` to the Medium URL, and
  set Medium's canonical link (Story settings → Advanced → Customize canonical link) to the
  post's URL here, the same day, or the Medium copy competes with it in search.
- **Keywords:** each page owns one phrase (FR-87 §3). A post and a docs page chasing the same
  query compete with each other; give them different titles and link them to each other.
- **With no post, there is no blog:** no `/blog/` output, no Blog link, no feed, no sitemap
  entry. The blog's styles are `blog.css`, published only with a post, so an empty
  `ui/blog/posts/` ships byte-identical docs.
- **The homepage's copy:** `ui/src/utils/landing.ts`, one zero-import module that the SPA's
  `LandingView.vue` and [`theme/home-layout.ts`](../ui/docs/theme/home-layout.ts) both render,
  so the two pages cannot drift. The install commands come from `enrollCommands.ts`, as
  everywhere else. The static page draws the docs' inline SVG icons: a new SPA icon needs an
  entry in `home-layout.ts`'s map, or the build fails.
- **What `home.js` adds:** the signed-in hand-off (§5); the newsletter form, where the server
  mounts `saas` (failing open, as the SPA does); live prices from `/api/stripe/plans`,
  over the fallback table the page ships; and, from `GET /api/setup/features` (FR-129
  B1-s), the Linux and macOS "Roomler Setup" buttons — shipped hidden in the Windows card's
  markup, each naming its OS (`data-wizard-unix="linux"`), and shown when the server
  advertises that OS (`wizard_linux_daemon`, `wizard_macos_daemon`; an older server's
  `wizard_unix_daemon` shows both) — and the pairing lead when
  it advertises both `pairing` (this server answers `/api/pair/*`) and `wizard_pairing` (the
  released wizard uses it). That one fails **closed**: anything but a JSON `true` (an older
  server's 404, a server without the fleet module, a proxy's error page) leaves the page as
  built, because a button the released wizard cannot honour is the mistake. With JavaScript
  off, the page is complete and the form's place links to sign-up.
- **A video** (FR-88 P3): add it to [`ui/src/utils/videos.ts`](../ui/src/utils/videos.ts), a
  **public** video only (the player of a private or scheduled one says "Video unavailable"),
  with its poster, 1280x720, in `ui/src/assets/tutorial/`, which both builds publish. Then
  `:::video <id>` on a docs page, its body an optional caption. `FEATURED_VIDEO_ID` is the one
  the homepage, the SPA landing and the tutorial's first chapter show. An id the manifest does
  not hold fails the build.

The video facade loads nothing from YouTube until someone presses play:

```mermaid
sequenceDiagram
  participant B as Browser
  participant R as roomler.ai
  participant Y as youtube-nocookie.com
  B->>R: the page, docs.css, docs.js and the poster (our own copy)
  Note over B: a poster, a play button and a link to YouTube. No iframe.
  B->>B: the click: docs.js (or VideoFacade.vue) swaps in the player
  B->>Y: /embed/<id>?autoplay=1, the first request to any YouTube host
```

- **The CSP** allows that one frame host, `frame-src https://www.youtube-nocookie.com`, at
  server level in [`files/security-headers.conf`](../files/security-headers.conf); without it
  frames fall back to `default-src 'self'` and the player is blocked. Permissions-Policy still
  keeps camera, microphone and display capture to the top-level page.
- **Structured data:** each embed adds a `VideoObject` to the page's graph (§4), on the
  homepage and on a docs page, with our poster as its `thumbnailUrl`. A post may use
  `:::video` too, but adds no `VideoObject` yet.
- **Without JavaScript**, and on a modified or middle click, the poster is a plain link to the
  video on YouTube.

## 8. Telling search engines

Crawlers find the site through `/sitemap.xml` (named in `robots.txt`) at their own pace. Two
things shorten that, and neither needs server code.

**IndexNow, on every promote.** Bing, Yandex, Seznam, Naver and the other engines that share
`api.indexnow.org` accept a list of changed URLs, and recrawl them within minutes. Google does
not take part. `promote.yml` does it around the roll, with
[`scripts/indexnow.sh`](../scripts/indexnow.sh):

```mermaid
sequenceDiagram
    participant P as promote.yml
    participant S as roomler.ai
    participant I as api.indexnow.org
    P->>S: snapshot: every child of /sitemap.xml → loc⇥lastmod
    P->>P: bump the deploy repo · watch /health for 10 min (the roll)
    P->>S: snapshot again, now served by the new image
    P->>P: changed = new URLs + re-dated URLs
    P->>S: GET /<key>.txt: exactly the key? (else: warn, submit nothing)
    P->>I: POST {host, key, keyLocation, urlList}
    I-->>P: 200 / 202 accepted (403 · 422 · 429 → a warning)
    I->>S: an engine fetches /<key>.txt to verify
```

- **What counts as changed:** a URL that is new, or whose `lastmod` differs. The lastmods come
  from git (§2), so a release that only touches the theme submits nothing. An undated URL
  (`/`, generated listings) is submitted once, when it first appears. With no before-snapshot,
  nothing is submitted, never everything.
- **The key is public by design.** Engines verify a submission by fetching
  `https://roomler.ai/<key>.txt`, so the file in `ui/public/` is its only source: the promote
  job checks out just `scripts/indexnow.sh` and `ui/public/*.txt`, and the smoke (§6) checks
  the file is served as committed.
- **Off unless the repository variable `INDEXNOW_ENABLED` is `true`**, and warn-only: a missed
  hint to a crawler never fails a promote.

**Search Console** holds the Domain property `sc-domain:roomler.ai`. It sits under the
Workspace account that owns the domain, and it verified itself from the domain's existing
`google-site-verification` TXT record (2026-09-29). Two things live there:
- **the sitemap index** `https://roomler.ai/sitemap.xml`, read as "Success";
- **per-page reindexing**: URL Inspection → *Request indexing*, about 10 a day. Use it after a
  page's content changes in a way that matters for search: a new page, or a new title.

It takes that account's sign-in, so it is the operator's. **Bing Webmaster Tools** imports the
property from it. Review the coverage at +3, +14 and +28 days after a launch (FR-87 §4h).

> ⚠️ A sitemap just submitted can read **"Couldn't fetch"** for a few minutes before Google has
> read it. Check what a crawler gets (`curl -A Googlebot …` → 200, `text/xml`) before chasing
> it: on 2026-09-29 it turned to Success on its own.

> ⚠️ **Not used, on purpose:** the sitemap "ping" endpoints (retired by Google in 2023), the
> Indexing API (Google limits it to job postings and livestreams), and a `SoftwareApplication`
> rich result, which requires ratings.

## 9. What a campaign link carries

[FR-88](fr/FR-88-promotion-attribution-and-site-video.md) makes one campaign id travel from a
post's link to the account it produced, and on to that account's first enrolled device. The
rule is **carry, don't store**: nothing about it is written to the visitor's device, not a
cookie, not localStorage, not sessionStorage. Storing on a terminal for a non-essential purpose
needs consent under ePrivacy Art. 5(3), and the site has no consent banner and must not need
one. The keys live in links, and only in links.

```mermaid
sequenceDiagram
  participant V as Visitor
  participant S as A static page
  participant A as The SPA
  participant API as /api
  participant R as Redis
  participant P as OAuth provider
  V->>S: /blog/x/?utm_source=youtube&utm_campaign=c1
  S->>S: attribution.js copies the keys onto the page's own links
  V->>A: a carried link: /register?utm_source=youtube&utm_campaign=c1&landing_path=/blog/x/
  alt the email form
    A->>API: POST /api/auth/register with attribution
  else a provider button, on /register or /login
    A->>API: GET /api/oauth/google?utm_source=youtube&utm_campaign=c1
    API->>R: park under the CSRF state, 600 s
    API->>P: 307 to the provider, carrying none of it
    P->>API: GET /api/oauth/callback/google?code&state
    API->>R: read and delete
  end
  API->>API: a NEW account keeps signup_attribution, once
```

| Hop | What happens | Where |
|---|---|---|
| A static page | With `utm_source`, `utm_medium`, `utm_campaign`, `utm_content`, `utm_term` or `ref` on its URL, it copies those keys onto its own links to `/register`, `/login`, the installer downloads and the install pages, and adds `landing_path` and the referring site's host. With none of them it changes nothing. The homepage's newsletter form gets the campaign as its `source` | [`attribution.js`](../ui/docs/theme/attribution.js), with the install pages written in from `INSTALL_PAGES` by [`carry.ts`](../ui/docs/theme/carry.ts) |
| The SPA | `/register` reads its own URL at the moment of sending, asks "How did you hear about Roomler?" (optional), and sends one `attribution` object. The provider buttons on `/register` and `/login` put the same keys on the OAuth start URL | [`attribution.ts`](../ui/src/utils/attribution.ts), [`RegisterView.vue`](../ui/src/views/auth/RegisterView.vue), [`LoginView.vue`](../ui/src/views/auth/LoginView.vue) |
| OAuth | `oauth_redirect` parks the keys in Redis under the CSRF state it already mints, for that cookie's 600 s, and sets no cookie of its own. `complete_sign_in` reads and deletes them. ⚠️ A missing or expired key means "no attribution", never a failed sign-in | [`routes/oauth.rs`](../crates/api/src/routes/oauth.rs), [`core/attribution.rs`](../crates/core/src/attribution.rs) |
| The account | `sanitize` keeps printable ASCII up to 64 characters, drops anything else and any unknown key, keeps a referrer's bare host and a landing page's path, and normalises `source` and `medium`. The result is stored once, at creation, as `users.signup_attribution`. No user-facing endpoint returns it, and it is not logged at `info` | [`core/attribution.rs`](../crates/core/src/attribution.rs), [`routes/auth.rs`](../crates/api/src/routes/auth.rs) |

**Conversions** are purestat goals. Each fires once, `signup` and `subscribe` only after the
server said yes, and none does anything when `window.purestat` is absent:

| Goal | Fires on | Where |
|---|---|---|
| `signup` | a successful register, or an OAuth callback whose redirect says `#signup=1` (this sign-in created the account) | [`RegisterView.vue`](../ui/src/views/auth/RegisterView.vue), [`OAuthCallbackView.vue`](../ui/src/views/auth/OAuthCallbackView.vue) |
| `subscribe` | the newsletter form's 202 | [`home.js`](../ui/docs/theme/home.js), [`StayInTouch.vue`](../ui/src/components/landing/StayInTouch.vue) |
| `install-copy` | the copy button on an install or enroll command | [`docs.js`](../ui/docs/theme/docs.js), [`EnrollmentDialog.vue`](../ui/src/components/enroll/EnrollmentDialog.vue) |
| `github-outbound` | a click or a middle click on a link into the repository | [`docs.js`](../ui/docs/theme/docs.js) |

**The activation view** is `GET /api/admin/stats/attribution?since=&until=`
([`routes/attribution.rs`](../crates/api/src/routes/attribution.rs)), platform admin only and a
404 to anyone else. It returns counts, never a user list: sign-ups by source, medium, campaign
and self-reported answer, and how many *activated*, meaning an org the person belongs to
enrolled a device within 7 days of the account's creation. The Campaigns tab of
[growth observability](growth-observability.md) shows the same account plane beside purestat's
web plane. Read either with these in mind:

> ⚠️ **`activated` is meaningful for an email sign-up only from 2026-10-29.** Until
> `hosted-20261029-2ae4ffd` took #3041 to production, the activation link had no page (#3029), so
> an email sign-up could never sign in, make an org or enroll a device. Before that date only an
> OAuth sign-up could activate. An email sign-up that misses its 5-minute link is still locked
> out: [#3199](https://github.com/gjovanov/roomler-ai/issues/3199).

- **`pending` is not a failure.** A sign-up with no device whose 7 days are still running is
  `pending`, and `settled` (`signups − pending`) is the only valid denominator for a rate.
  Without `fleet`, all three are `null`, never `0`: "cannot see devices" is a different answer
  from "nobody enrolled one".
- **A count by source is what the links said, not a measurement.** Any link can set any value,
  `referrer_host` and `landing_path` included. The server clamps them, and nothing checks them.
- **The two planes can disagree, by design.** purestat credits a visit to the key it landed with
  ([growth-observability.md](growth-observability.md)), so a visitor who lands untagged and then
  follows a tagged link counts as untagged there and under the campaign on the account.
- **"An org the person belongs to" is any org at query time**, one they were invited into
  included. That definition is FR-88's open decision 4.
- ⚠️ **Every bucket key is text chosen by whoever built the link.** An admin UI renders it as text
  (`{{ key }}`), never as markup (`v-html`).

**An organic landing** carries nothing: no campaign key, no change to any link, so that sign-up
has no source at all. [FR-91 §3e](fr/FR-91-scheduled-blog-publishing.md) extends the carry to a
post reached from another site.

**Kill switches:** `ATTRIBUTION_ENABLED = false` in
[`attribution.ts`](../ui/src/utils/attribution.ts) stops the carry everywhere: the register view
sends nothing and hides its question, the provider buttons carry nothing, the newsletter keeps
its own `source`, and the build leaves out the carry script. `ANALYTICS = null` in
[`site.ts`](../ui/docs/site.ts) leaves the tracker off every static page, which silences the
goals there.

## 10. The link hub and the short paths

A profile bio, or a URL said out loud in a video, cannot carry parameters. So each channel has a
short path, and every short path lands on `/links/` with the campaign already on it:

```mermaid
flowchart LR
    BIO["A profile bio,<br/>or a URL said in a video"] --> SHORT["/yt /tt /x /fb"]
    SHORT -->|"302 from nginx, relative"| HUB["/links/ with utm_source=youtube,<br/>utm_medium=bio, utm_campaign=profile"]
    HUB -->|"attribution.js (§9)"| CTA["/register, the install pages,<br/>carrying the campaign"]
    HUB --> REST["the docs, the blog, the repository,<br/>the channel profiles"]
```

- **`/links/`** is a static page from the same generator
  ([`links-layout.ts`](../ui/docs/theme/links-layout.ts)), held to the homepage's link gate. It is
  `noindex, follow` and left out of the sitemap: it repeats the homepage for people who arrive
  from a profile, so it is not a page to rank.
- **`CHANNELS`** in [`site.ts`](../ui/docs/site.ts) is the one list of profiles. The hub lists
  them, and `ORG.sameAs` names them to search engines next to the repository.
- **The short paths** are one `location = /yt { return 302 … }` line each in
  [`files/nginx-pod.conf`](../files/nginx-pod.conf). `utm_medium=bio&utm_campaign=profile` is what
  every profile link of the program carries, so a visit through a bio is one source whichever way
  it came.
- **302, not 301:** a browser keeps a 301 for good, and where a short path points may change.
- **No `add_header` in these locations** (§5), and the redirect stays relative
  (`absolute_redirect off`).
- ⚠️ **A short path lives only as long as its channel.**
  [`links-page.spec.ts`](../ui/docs/__tests__/links-page.spec.ts) checks the nginx lines against
  `CHANNELS` both ways, and the SPA's route table for a collision, so `/ig` left when X took
  Instagram's place on 2026-10-11. To add a channel, add its `CHANNELS` row and its `location`
  line in one PR.
- **The SPA's old marketing routes keep a campaign too.** `/pricing?utm_source=x` is a 301 to
  `/?utm_source=x#pricing`, `/landing?…` to `/?…`, and without a query both answer as they always
  did (§5).

The smoke's redirect block and its §5c check the redirects and the hub on the served site, after
every promote (§6). Removing the locations and the page is the kill switch.

## Code map

| File | What it owns |
|---|---|
| [`ui/docs/build.ts`](../ui/docs/build.ts) | discovery, loading, the gates, emission, sitemaps, robots, the feed |
| [`ui/docs/dates.ts`](../ui/docs/dates.ts) | git dates, the manifest, `--write` |
| [`ui/docs/site.ts`](../ui/docs/site.ts) | sections, limits, keys, the Organization, authors, the blog's constants |
| [`ui/docs/theme/render.ts`](../ui/docs/theme/render.ts) | markdown → HTML: containers, images, headings, code |
| [`ui/docs/theme/shell.ts`](../ui/docs/theme/shell.ts) | the `<head>`, top bar, footer, search dialog |
| [`ui/docs/theme/analytics.js`](../ui/docs/theme/analytics.js) · [`ui/src/utils/analytics.ts`](../ui/src/utils/analytics.ts) | the hosted-only analytics loader (FR-102), and the SPA's rule it mirrors |
| [`ui/docs/theme/attribution.js`](../ui/docs/theme/attribution.js) · [`carry.ts`](../ui/docs/theme/carry.ts) · [`ui/src/utils/attribution.ts`](../ui/src/utils/attribution.ts) · [`ui/src/utils/goals.ts`](../ui/src/utils/goals.ts) | the campaign's carry (FR-88): the static pages, the install list, the SPA, and the purestat goals |
| [`crates/core/src/attribution.rs`](../crates/core/src/attribution.rs) · [`crates/api/src/routes/attribution.rs`](../crates/api/src/routes/attribution.rs) | the sanitiser and the OAuth parking spot · the activation view |
| [`ui/docs/theme/links-layout.ts`](../ui/docs/theme/links-layout.ts) | the link hub (FR-88 P2) |
| [`ui/docs/theme/layout.ts`](../ui/docs/theme/layout.ts) | a docs page |
| [`ui/docs/theme/blog-layout.ts`](../ui/docs/theme/blog-layout.ts) | a post, the index, tag pages |
| [`ui/docs/theme/posts.ts`](../ui/docs/theme/posts.ts) | the post contract, ordering, backlinks |
| [`ui/docs/theme/home-layout.ts`](../ui/docs/theme/home-layout.ts) · [`home.js`](../ui/docs/theme/home.js) · [`home.css`](../ui/docs/theme/home.css) | the static homepage, its enhancement (the hand-off first), its styles |
| [`ui/src/utils/landing.ts`](../ui/src/utils/landing.ts) | the homepage's copy and fallback plans, shared with the SPA |
| [`ui/src/utils/videos.ts`](../ui/src/utils/videos.ts) · [`ui/docs/theme/video.ts`](../ui/docs/theme/video.ts) · [`VideoFacade.vue`](../ui/src/components/common/VideoFacade.vue) | the embedded videos (FR-88 P3), the static facade, the app's facade |
| [`files/security-headers.conf`](../files/security-headers.conf) | every page's security headers: the CSP, its `frame-src` for the player |
| [`ui/docs/theme/structured.ts`](../ui/docs/theme/structured.ts) | JSON-LD |
| [`ui/docs/theme/xml.ts`](../ui/docs/theme/xml.ts) | sitemaps, robots, the Atom feed |
| [`ui/docs/theme/assets.ts`](../ui/docs/theme/assets.ts) · [`images.ts`](../ui/docs/theme/images.ts) · [`links.ts`](../ui/docs/theme/links.ts) | hashing · image sizes · the link gate |
| [`files/nginx-pod.conf`](../files/nginx-pod.conf) | how it is served |
| [`scripts/public-site-smoke.sh`](../scripts/public-site-smoke.sh) | how it is proven |
| [`scripts/indexnow.sh`](../scripts/indexnow.sh) · `ui/public/<key>.txt` · [`promote.yml`](../.github/workflows/promote.yml) | telling search engines what a roll changed |
