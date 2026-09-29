# FR-87: A blog at roomler.ai/blog, a crawlable homepage, and a site Google can index

**Issue:** [#1776](https://github.com/gjovanov/roomler-ai/issues/1776) · **Status:** proposed
2026-09-29 (claimed; nothing built) · **Owner:** web / public site · **Builds on:**
[FR-60](FR-60-public-docs-site.md) (the static docs generator)

## 1. Goal

Make roomler.ai a site that Google can read and that earns its own search traffic.

1. **A blog at `/blog/`**: static, SEO-complete pages built by the same in-repo generator as
   `/docs`. The first post is the operator's Medium article
   ([*My wife's swearing at TeamViewer made me build my own remote desktop tool with extras*](https://medium.com/@gjovanov/my-wifes-swearing-at-teamviewer-made-me-build-my-own-remote-desktop-tool-with-extras-e48f1f7c5e35)),
   republished at `/blog/self-hosted-teamviewer-alternative/`. **The blog is the canonical
   copy**; Medium's canonical link points at it (operator decision, 2026-09-29).
2. **A crawlable homepage.** `/` serves guests and crawlers static HTML, and serves the SPA to
   signed-in users exactly as today (operator decision, 2026-09-29).
3. **The docs' SEO defects fixed** (§2).
4. **Indexing:** a Search Console domain property, the sitemap submitted, per-URL indexing
   requests, Bing Webmaster Tools and IndexNow.

## 2. Evidence: the failing baselines (measured 2026-09-28, production)

Each row is also an acceptance criterion's "shown failing" run.

| Probe | Today | Cause |
|---|---|---|
| `/sitemap.xml` `<lastmod>` | **70 of 70 URLs show the build date**; the content last changed 2026-09-01 | `build.ts:99-122` reads `git log` and falls back to today. `.git` is excluded by `.dockerignore:4` and CI checks out at depth 1. The same value feeds JSON-LD `dateModified` and the "Last updated" footer |
| `/docs/assets/docs.css`, `search.js` | unhashed names served `Cache-Control: public, immutable` for a year, **without CSP** | the regex location `nginx-pod.conf:64-69` takes precedence over `location /docs/` (`:92-94`) and declares `add_header`, which drops the inherited security headers |
| `curl -sI https://roomler.ai/docs` | `301 Location: http://roomler.ai/docs/`, a **scheme downgrade** | absolute redirect built from the pod's `:80` listener behind the TLS edge |
| `/blog/`, `/this-does-not-exist` | **200** with the SPA shell (soft 404) | SPA fallback `nginx-pod.conf:97-99` |
| `/docs/start/install/windows` (no slash) | 200, duplicate of the slash form | `try_files $uri $uri/index.html` serves both |
| `/` | a client-rendered shell: an empty `<div id="app">` for any crawler that doesn't run JS | `/` is `meta.auth` (`ui/src/plugins/router.ts:119`); guests are redirected to `/landing` client-side (`:326`) |
| sitemap | lists `/landing /pricing /privacy /terms /imprint` (`site.ts:150`), all of which declare canonical `/` (`ui/index.html:24`) | contradictory signals |
| docs JSON-LD | author `"G ROX LTD"` (`layout.ts:221`), but the legal name everywhere else is G ROX EOOD; no `datePublished`, `image` or `mainEntityOfPage`; the `WebSite`/`SoftwareApplication` markup spec'd by FR-60 was never built (`LayoutCtx.extraJsonLd` is never passed, `build.ts:515-523`) | — |
| docs `<title>` | up to 71 chars (e.g. `/docs/compare/teamviewer/`) | the template `layout.ts:204-209` always appends " · section — Roomler Docs" |
| docs pages | **no analytics**, so organic traffic isn't measured | purestat loads only in the SPA (`ui/index.html:42`) |
| `render.ts:194` | card title opens `<h2>` and closes `</h3>` | missed in FR-60's heading pass |
| Search Console | no verified property for roomler.ai on the operator's day-to-day Google account | — |

**What already works, and must keep working:**
- canonical, OG and Twitter tags, and inline JSON-LD on every docs page (the CSP allows data
  blocks);
- `robots.txt` (`Allow: /`, app paths disallowed);
- security headers on HTML identical to `/` (FR-60 AC6);
- the build gates.

## 3. Keyword targets (Google Keyword Planner, US, English, Sep 2025 – Aug 2026)

| Cluster | Monthly searches | Target page |
|---|---|---|
| teamviewer alternative (+ ~20 variants: alternative to teamviewer, teamviewer alternative free, teamviewer competitors, apps/software like teamviewer…) | 1K–10K (variants 100–1K) | the post: the story / "why" angle |
| self hosted / open source teamviewer alternative | 10–100 | the post (title tag, slug, dek) |
| unattended remote access (+ software, teamviewer / anydesk unattended access) | 100–1K; top-of-page bids €8–27 | `/docs/remote-desktop/unattended-access/`, linked from the post |
| remote desktop from browser, web browser remote desktop, teamviewer from browser, rdp browser | 100–1K, **+900% over 3 months** | `/docs/remote-desktop/`, linked from the post |
| teamviewer competitors, anydesk alternative | 100–1K | `/docs/compare/teamviewer/` |
| rustdesk alternative(s) | 100–1K | `/docs/compare/rustdesk/` |
| tailscale alternative, self hosted tailscale, netbird vs tailscale | 100–1K (bids to €46) | `/docs/compare/tailscale/` now; the mesh post later |

The post and each compare page target different angles of the same topic and link to each
other with different anchor text, so they don't compete for one query.

## 4. Design

### 4a. One generator, two collections plus a homepage

`ui/docs/build.ts` stays the only site writer. It gains a **blog collection**
(`ui/blog/posts/*.md`, assets in `ui/blog/assets/`) and a **homepage template**. They share the
`<head>`, sitemaps, robots, search index, link checker and a new asset pipeline. The build
command (`bun run build`, `ui/package.json:9`), the Dockerfile and the CI invocation
(`ci.yml:1538-1568`) stay as they are, and **no dependency is added**. A separate blog builder
was rejected: it would mean two writers of `dist/sitemap.xml` and a link checker that can't see
a `/blog` → `/docs` link.

New modules under `ui/docs/`:

| Module | Purpose |
|---|---|
| `dates.ts` | git → dates manifest |
| `theme/shell.ts` | shared `<head>`, topbar (Docs \| Blog), footer |
| `theme/structured.ts` | JSON-LD builders |
| `theme/xml.ts` | sitemap index, feed, robots |
| `theme/images.ts` | dimensions, hashing, the image rule |
| `theme/links.ts` | the link checker, testable |
| `theme/posts.ts` | post schema, sorting, pagination |
| `theme/blog-layout.ts` | blog page renderers |
| `theme/home-layout.ts` | the homepage renderer |
| `theme/home.js` | the homepage's progressive enhancement |

`ui/src/utils/plans.ts` is a zero-import plan table shared by `LandingView.vue` and the
homepage, following the `enrollCommands.ts` pattern.

### 4b. Dates: a git manifest, a front-matter override, and no date when unknown

- **Manifest generation:** `bun ui/docs/dates.ts --write` runs one `git log` over the content
  and writes a deterministic `ui/docs/content-dates.json`, keyed by path with `created` and
  `modified`. It runs on `hosted-image.yml`, which already checks out full history (`:76`),
  before the Docker build, so `COPY ui/ .` carries the file in. The file is gitignored.
- **Docs resolution order:** front-matter `updated` → live git (only when the clone isn't
  shallow) → manifest → **undefined**.
- **Blog dates** come only from front-matter (`date`, `updated`), because they are editorial.
- **An undefined date is omitted** (no `<lastmod>`, no `dateModified`, no "Last updated"
  line), never replaced by the build date. A build path without the manifest, such as a
  self-host or break-glass build, shows no dates instead of wrong ones.

### 4c. Assets

- Every emitted asset gets a **content-hashed name**, `name.<sha256:10>.ext`, so the one-year
  `immutable` cache is correct. The unhashed theme files are emitted for one more release, so
  that cached HTML can still load them.
- `social-preview.png` keeps its stable name, because `ui/index.html` points at it.
- **Inline markdown images** get a markdown-it rule that emits `width`/`height` read from the
  file itself (PNG, GIF, JPEG, WebP and SVG parsed by hand, no dependency),
  `loading="lazy"` and `decoding="async"`. An image alone in a paragraph becomes a
  `<figure>`/`<figcaption>`.
- **Build errors:** empty alt text, a remote `src`, a raw `<img>`, or a file over 3 MB.
- Heroes get their real dimensions, replacing the hard-coded 960×420 at `layout.ts:236`.

### 4d. `<head>` and structured data

- `og:type` is `website` on listings and `article` on leaves.
- `article:published_time`, `article:modified_time`, `article:author` and `article:tag`.
- `max-image-preview:large`.
- Per-post `og:image`: the hero, at least 1200 px wide at ~1.91:1, with its width, height and
  alt text.
- A `seoTitle` front-matter key, capped at 60 chars by a build gate. The default template drops
  " · section" when the title would exceed 60.
- One Organization entity (`@id https://roomler.ai/#organization`, name "Roomler", legalName
  "G ROX EOOD"), referenced from every page.
- **Types by page:**
  - `BlogPosting` + `BreadcrumbList` on posts;
  - `Blog` on the blog index;
  - `TechArticle` + `BreadcrumbList` on docs pages, now with `datePublished`, `image` and
    `mainEntityOfPage`;
  - `WebSite` + `Organization` + `SoftwareApplication` on the homepage. The sitelinks
    SearchAction is not built: Google retired it in 2024.
- `/sitemap.xml` becomes a **sitemap index** pointing at `sitemap-docs.xml` (which includes
  `/`) and `sitemap-blog.xml`, so Search Console reports each separately. The SPA routes are
  dropped.
- Front-matter becomes **strict**: an unknown key is an error.
- purestat analytics is added to the static pages (the CSP already allows it); setting
  `ANALYTICS = null` turns it off.

### 4e. The blog

- **Posts** live at `/blog/<slug>/`. They require `title`, `description` (≤160), `date`,
  `author` (a key in `AUTHORS`), `tags`, `hero` and `heroAlt`.
- **Optional keys:** `seoTitle`, `subtitle`, `updated`, `related` (link-checked internal URLs),
  `syndication` (the Medium copy) and `canonical` (off-site only).
- **Drafts:** there is no `draft` key, and a future `date` is a build error. A file in
  `ui/blog/posts/` counts as published, and drafts stay in the private promo repo, per
  FR-39's rule on post copy.
- **Listing and tags:** the index is newest first, 10 per page (`/blog/page/N/`). Tag pages
  appear at ≥3 posts and stay out of the sitemap, as in docs.
- **Feed:** an Atom 1.0 feed at `/blog/feed.xml` with full content, advertised by
  `<link rel="alternate">` on every page once one post exists.
- **Post page:**
  - dek (subtitle), author, published and updated `<time>`, reading time;
  - hero with `fetchpriority="high"`;
  - TOC;
  - "Also published on Medium";
  - an author box (with a bio the operator writes);
  - related docs, previous/next, and a self-host call to action.
- **Search and linking:** posts join the site search. A docs page linked from a post shows an
  "On the blog" backlink. The topbar and footer gain Blog and RSS links.
- **Kill switch:** an empty `ui/blog/posts/` produces no `/blog` output, links, feed or sitemap
  entries.

### 4f. nginx (`files/nginx-pod.conf`)

```nginx
absolute_redirect off;                               # Location: /docs/ — never http://
location /docs/ { expires -1; error_page 404 /docs/404.html; }
location /blog/ { expires -1; error_page 404 /docs/404.html; }
location = /blog/feed.xml { types { } default_type application/atom+xml; expires -1; }
map $cookie_access_token $root_doc { "" /home/index.html; default /index.html; }   # http level
location = / { expires -1; try_files $root_doc =404; }
location = /landing { return 301 /; }
location = /pricing { return 301 /#pricing; }
```

- **`expires` only, never `add_header`, in these locations.** `expires` is inherited
  separately from `add_header`, so the server-level CSP, HSTS, XFO, XCTO, Referrer-Policy and
  Permissions-Policy survive. The smoke test proves it with a header diff against `/`.
- **No `try_files` in `/docs/` or `/blog/`.** nginx's static handler then answers a
  slashless directory with a 301 to the slash form, which `absolute_redirect off` keeps
  relative, and a missing file with a real 404. FR-60's `try_files $uri $uri/index.html =404`
  is what made `/docs/start` a 200 duplicate: its `$uri/index.html` test matches the file, so
  nothing ever redirects. (This section's first draft, `try_files $uri $uri/ =404`, does 301,
  measured in `nginx:stable`; it adds nothing, so P1 ships without it.)
- **The session cookie decides the homepage.** The session is the HttpOnly `access_token`
  cookie (`crates/api/src/routes/auth.rs:247`). No cookie (guests and every crawler) gets the
  static homepage; a cookie gets the SPA, whose router behaves as today. A stale or expired
  cookie gets the SPA, which sends the visitor to login on its first 401.

### 4g. The homepage

`home-layout.ts` renders `dist/home/index.html` from `LandingView.vue`'s sections: hero, trust,
the three pillars, "Set up a device in minutes" (with `:::enroll`), pricing from
`ui/src/utils/plans.ts`, and the call to action.

- **Progressive enhancement:** `home.js`, an external script (the CSP forbids inline ones),
  posts the newsletter form to the saas `…/subscribe` route and refreshes pricing from
  `/api/stripe/plans`. Without JavaScript, the page stays complete and the form links to
  sign-up.
- **Router:** a signed-out visitor at the root, and a logout, both navigate fully to `/`.
  `LandingView` keeps working for in-app navigation and takes its fallback plans from the
  shared module, so there is one source for the table.

### 4h. Indexing

**Operator only**, because they involve the Google account or DNS. The agent can drive steps
1–4 in the browser, with the operator's go-ahead, if the Workspace account is signed in.

1. **Search Console:** Add property → **Domain `roomler.ai`** → Verify. A
   `google-site-verification` TXT record already exists on the domain.
2. **Sitemaps:** submit `https://roomler.ai/sitemap.xml` and expect "Success" for both child
   sitemaps.
3. **URL Inspection:** Request indexing for `/`, `/blog/`, the post, `/docs/`, and the
   teamviewer, rustdesk and tailscale compare pages. The quota is about 10 a day.
4. **Medium:** Story settings → Advanced Settings → Customize Canonical Link → the blog URL,
   the same day the post is live.
5. **Bing Webmaster Tools:** Import from Search Console.
6. **Review** at +3, +14 and +28 days: Pages by sitemap, Performance queries, Enhancements
   (Article, Breadcrumbs), Core Web Vitals.

**Agent:**
- **IndexNow:** a key file in `ui/public/`, and a POST of new or changed URLs after the
  health watch in `promote.yml`, gated on `vars.INDEXNOW_ENABLED`. Failure only warns, and no
  server code is involved.
- **Validation:** Rich Results Test and Schema Markup Validator in the browser, the W3C Feed
  Validator, and PageSpeed Insights (mobile and desktop) through its API.
- **What not to expect or use:**
  - FAQ rich results, which have been limited to government and health sites since 2023;
  - a `SoftwareApplication` rich result, which needs ratings, and ratings are never invented;
  - the retired sitemap ping endpoint, and the Indexing API (job and livestream pages only).

## 5. Phases

| P | What | Kill switch |
|---|---|---|
| P0 | claim (issue #1776, spec, ledger row); baselines (§2); **FR-60 close-out**: tick its criteria from the #1165 evidence, run its missing Rich Results check, write `docs/public-site.md` v1 | docs only |
| P1 | nginx (§4f, without the homepage map), plus header-diff and redirect assertions in the `hosted-image.yml` smoke | revert the hunk |
| P2 | dates (§4b), hashed assets and the image pipeline (§4c), the search index URL read from a `data-` attribute, the `</h2>` fix, real hero dimensions | missing manifest ⇒ no dates |
| P3 | `<head>` and structured data, strict front-matter, `seoTitle`, sitemap index, analytics (§4d) | `ANALYTICS = null` |
| P4 | the blog engine (§4e) | empty `ui/blog/posts/` |
| P5 | the post; keyword-tuned `seoTitle`/`description` on the target docs pages (§3); Medium canonical | delete the post |
| P6 | the homepage (§4g) and its nginx branch | remove the `map` and `location = /` |
| P7 | indexing (§4h) | `vars.INDEXNOW_ENABLED` |
| P8 | docs: `docs/public-site.md` with mermaid diagrams (build flow, date resolution, nginx routing incl. the cookie branch, post authoring and the private-draft rule), a `docs/README.md` row, the field log | — |

**Deploy order:**
- P1 goes live before P2, so HTML is no-cache before any asset is hashed. The unhashed copies
  cover the overlap as well.
- P1's real 404s ship before `/blog`.
- P6's `map` ships in the same image as `dist/home/`.

## 6. Acceptance criteria

- [ ] **AC1:** the build emits `dist/blog/{index.html,<slug>/,feed.xml}`, `dist/home/index.html`
  and `sitemap{,-docs,-blog}.xml`, with no change to `package.json`, `bun.lock` or the
  Dockerfile.
- [ ] **AC2:** every new gate is shown failing, then passing: missing alt, unknown key, `draft`,
  future `date`, `updated` < `date`, hero narrower than 1200 px, `seoTitle` over 60, a dead
  blog→docs link.
- [ ] **AC3:** with no posts there is no `/blog` output and no Blog link.
- [ ] **AC4:** production `<lastmod>` equals `git log -1 --format=%cs` for 5 sampled docs files
  (baseline: 70/70 show the build date).
- [ ] **AC5:** the Rich Results Test reports 0 errors on the post (Article + Breadcrumbs), one
  docs page, and `/`.
- [ ] **AC6:** the security headers are identical on `/` (SPA), the static homepage, a docs
  page and a blog page, and HTML is `no-cache`.
- [ ] **AC7:** `/blog/nonexistent/` and `/docs/nonexistent/` return 404; `/docs`, `/blog` and a
  slashless docs path return 301 with a relative Location (baseline: `http://` downgrade and
  soft 404s).
- [ ] **AC8:** `/blog/feed.xml` is served as `application/atom+xml` and the W3C validator
  accepts it.
- [ ] **AC9:** every `/docs/assets` and `/blog/assets` URL in production HTML is content-hashed,
  and every image carries `width`/`height`.
- [ ] **AC10:** Lighthouse SEO scores 100 on `/`, `/blog/`, the post and two docs pages; other
  scores are recorded as measured.
- [ ] **AC11:** `curl /` without a cookie returns the static homepage (its H1 and JSON-LD); with
  `Cookie: access_token=x` it returns the SPA shell. `/landing` 301s to `/`. A signed-in user
  still lands on the dashboard (Playwright e2e green).
- [ ] **AC12:** Search Console shows the sitemap as "Success"; the post reports "URL is on
  Google" within 14 days of the request; Medium's page source carries a canonical link to the
  blog.
- [ ] **AC13:** Bing Webmaster Tools is verified, the first IndexNow POST returns 200/202, and
  purestat records `/blog/` visits.
- [ ] **AC14:** docs updated or created with mermaid diagrams (`docs/public-site.md`) and indexed
  in `docs/README.md`.

## 7. Open decisions

- **Author pages:** deferred until there are ≥3 posts. Until then `author.url` points to
  GitHub.
- **Demo media:** the GIF (2.2 MB, lazy-loaded) ships now; switching to an MP4 `<video>`
  (`media-src 'self'` allows it) is a follow-up.
- **Whether `/privacy`, `/terms` and `/imprint` also become static pages.** They stay SPA
  routes here and simply leave the sitemap.

## 8. Out of scope

- The text of future posts, including the mesh article.
- Translations.
- Comments.
- A public newsletter archive: FR-58's open question, which could later live on this blog.
- AMP.
- Paid search.

## 9. Field-verification log

| Date | Build | What | Result |
|---|---|---|---|
| 2026-09-28 | production (pre-FR) | the §2 baselines | all failing as listed; recorded as the "shown failing" runs |

## 10. Related

- [FR-60](FR-60-public-docs-site.md): the generator this extends. P0 closes it out.
- [FR-58](FR-58-newsletter-sending.md): the newsletter and its hero images, versioned
  `ui/public/newsletter-img/*-vN.png`.
- [FR-39](FR-39-launch-readiness.md): post copy stays private until published; prerendering
  was out of scope there, and P6 does it here.
- [FR-73](FR-73-image-build-on-github.md): the hosted-image and promote pipeline P1, P2 and P7
  change.
