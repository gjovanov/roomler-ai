/* SPDX-License-Identifier: AGPL-3.0-only
 * Copyright (C) 2026 G ROX EOOD
 *
 * FR-58 (#1165) — client-side documentation search.
 *
 * A prebuilt section-level index plus prefix scoring. Deliberately NOT
 * Pagefind: Pagefind is the better ranker at scale, but it is WASM, and
 * the pod CSP is `script-src 'self'` with no `wasm-unsafe-eval`
 * (files/nginx-pod.conf). Adopting it would mean widening a security
 * header to ship a docs feature. This runs under the CSP exactly as it
 * stands today.
 *
 * The index is fetched on FIRST OPEN, not on page load, so a reader who
 * never searches never pays for it.
 *
 * Index shape (keys are short because every byte ships):
 *   { p: [{ u, t, s }],                        pages: url, title, section
 *     r: [{ p, h, a, x, g }] }                 records: pageIdx, heading,
 *                                              anchor, excerpt, tags
 */
;(function () {
  'use strict'

  var INDEX_URL = '/docs/assets/search-index.json'
  var MAX_RESULTS = 24
  var MAX_PER_PAGE = 3

  var dialog = document.querySelector('[data-search-dialog]')
  var input = document.querySelector('[data-search-input]')
  var resultsEl = document.querySelector('[data-search-results]')
  if (!dialog || !input || !resultsEl) return

  var index = null
  var loading = null
  var hits = []
  var active = -1

  // ── loading ───────────────────────────────────────────────────────────
  function load() {
    if (index) return Promise.resolve(index)
    if (loading) return loading
    loading = fetch(INDEX_URL, { credentials: 'omit' })
      .then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status)
        return r.json()
      })
      .then(function (data) {
        index = data
        // Precompute a lowercased haystack once, rather than per keystroke.
        for (var i = 0; i < index.r.length; i++) {
          var rec = index.r[i]
          var page = index.p[rec.p]
          rec._t = (page.t || '').toLowerCase()
          rec._h = (rec.h || '').toLowerCase()
          rec._x = (rec.x || '').toLowerCase()
          rec._g = (rec.g || []).join(' ').toLowerCase()
        }
        return index
      })
      .catch(function (err) {
        loading = null
        // An index that failed to load must SAY so. Rendering "No results"
        // for a network error is a lie the reader would act on by
        // concluding the docs do not cover their topic.
        resultsEl.innerHTML =
          '<p class="search-empty">Search index could not be loaded (' +
          esc(String(err.message || err)) +
          '). Try reloading the page.</p>'
        throw err
      })
    return loading
  }

  // ── scoring ───────────────────────────────────────────────────────────
  function esc(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
  }

  function parseQuery(raw) {
    var tags = []
    var terms = []
    var parts = raw.toLowerCase().split(/\s+/)
    for (var i = 0; i < parts.length; i++) {
      var p = parts[i]
      if (!p) continue
      if (p.indexOf('tag:') === 0) {
        if (p.length > 4) tags.push(p.slice(4))
      } else {
        terms.push(p)
      }
    }
    return { terms: terms, tags: tags }
  }

  /** Field weights. A heading match beats a body match beats a tag match,
   *  because a reader searching "exit node" wants the page ABOUT exit
   *  nodes, not every page that mentions one in passing. */
  function scoreRecord(rec, q) {
    for (var t = 0; t < q.tags.length; t++) {
      if (rec._g.indexOf(q.tags[t]) === -1) return 0
    }
    if (q.terms.length === 0) return q.tags.length ? 1 : 0

    var total = 0
    for (var i = 0; i < q.terms.length; i++) {
      var term = q.terms[i]
      var s = 0
      if (rec._t.indexOf(term) !== -1) s += rec._t.indexOf(term) === 0 ? 60 : 40
      if (rec._h.indexOf(term) !== -1) s += rec._h.indexOf(term) === 0 ? 50 : 34
      if (rec._g.indexOf(term) !== -1) s += 22
      if (rec._x.indexOf(term) !== -1) s += 10
      // Every term must appear somewhere: an AND search, so "exit node
      // windows" does not rank a page that only mentions Windows.
      if (s === 0) return 0
      total += s
    }
    return total
  }

  function search(raw) {
    var q = parseQuery(raw)
    if (q.terms.length === 0 && q.tags.length === 0) return []

    var scored = []
    for (var i = 0; i < index.r.length; i++) {
      var s = scoreRecord(index.r[i], q)
      if (s > 0) scored.push({ rec: index.r[i], score: s })
    }
    scored.sort(function (a, b) {
      return b.score - a.score
    })

    // Cap per page so one long page cannot fill the whole result list.
    var perPage = {}
    var out = []
    for (var j = 0; j < scored.length && out.length < MAX_RESULTS; j++) {
      var pi = scored[j].rec.p
      perPage[pi] = (perPage[pi] || 0) + 1
      if (perPage[pi] > MAX_PER_PAGE) continue
      out.push(scored[j])
    }
    return out
  }

  function highlight(text, terms) {
    var safe = esc(text)
    for (var i = 0; i < terms.length; i++) {
      var term = terms[i]
      if (term.length < 2) continue
      var re = new RegExp('(' + term.replace(/[.*+?^${}()|[\]\\]/g, '\\$&') + ')', 'gi')
      safe = safe.replace(re, '<mark>$1</mark>')
    }
    return safe
  }

  function excerptAround(text, terms) {
    var lower = text.toLowerCase()
    var at = -1
    for (var i = 0; i < terms.length && at === -1; i++) at = lower.indexOf(terms[i])
    if (at < 0) return text.slice(0, 150)
    var start = Math.max(0, at - 55)
    return (start > 0 ? '…' : '') + text.slice(start, start + 155)
  }

  // ── rendering ─────────────────────────────────────────────────────────
  function render(raw) {
    var q = parseQuery(raw)
    var found = search(raw)
    hits = []
    active = -1

    if (!raw.trim()) {
      resultsEl.innerHTML =
        '<p class="search-empty">Search by topic, command or tag — try <strong>exit node</strong>, ' +
        '<strong>install</strong> or <strong>tag:windows</strong>.</p>'
      return
    }
    if (!found.length) {
      resultsEl.innerHTML = '<p class="search-empty">No matches for “' + esc(raw) + '”.</p>'
      return
    }

    var html = ''
    var lastSection = null
    for (var i = 0; i < found.length; i++) {
      var rec = found[i].rec
      var page = index.p[rec.p]
      if (page.s !== lastSection) {
        html += '<p class="search-group__head">' + esc(page.s || 'Docs') + '</p>'
        lastSection = page.s
      }
      var url = page.u + (rec.a ? '#' + rec.a : '')
      hits.push(url)
      html +=
        '<a class="search-hit" href="' +
        esc(url) +
        '" data-hit="' +
        i +
        '">' +
        '<span class="search-hit__title">' +
        highlight(rec.h || page.t, q.terms) +
        '</span>' +
        (rec.h ? '<span class="search-hit__crumb">' + esc(page.t) + '</span>' : '') +
        '<span class="search-hit__excerpt">' +
        highlight(excerptAround(rec.x || '', q.terms), q.terms) +
        '</span>' +
        '</a>'
    }
    resultsEl.innerHTML = html
  }

  function setActive(next) {
    var nodes = resultsEl.querySelectorAll('.search-hit')
    if (!nodes.length) return
    if (active >= 0 && nodes[active]) nodes[active].classList.remove('is-active')
    active = ((next % nodes.length) + nodes.length) % nodes.length
    nodes[active].classList.add('is-active')
    nodes[active].scrollIntoView({ block: 'nearest' })
  }

  // ── open / close ──────────────────────────────────────────────────────
  function open() {
    if (dialog.open) return
    // showModal gives focus trapping and Esc-to-close for free; `.show()`
    // is the fallback where the dialog polyfill surface is missing.
    if (typeof dialog.showModal === 'function') dialog.showModal()
    else dialog.setAttribute('open', '')
    input.focus()
    input.select()
    load().then(function () {
      render(input.value)
    })
  }

  function close() {
    if (typeof dialog.close === 'function') dialog.close()
    else dialog.removeAttribute('open')
  }

  var openers = document.querySelectorAll('[data-search-open]')
  for (var i = 0; i < openers.length; i++) openers[i].addEventListener('click', open)

  var closer = document.querySelector('[data-search-close]')
  if (closer) closer.addEventListener('click', close)

  // Click on the backdrop (the dialog element itself, outside its children).
  dialog.addEventListener('click', function (ev) {
    if (ev.target === dialog) close()
  })

  input.addEventListener('input', function () {
    if (!index) {
      load().then(function () {
        render(input.value)
      })
      return
    }
    render(input.value)
  })

  input.addEventListener('keydown', function (ev) {
    if (ev.key === 'ArrowDown') {
      ev.preventDefault()
      setActive(active + 1)
    } else if (ev.key === 'ArrowUp') {
      ev.preventDefault()
      setActive(active - 1)
    } else if (ev.key === 'Enter') {
      ev.preventDefault()
      if (active >= 0 && hits[active]) window.location.href = hits[active]
      else if (hits.length) window.location.href = hits[0]
    }
  })

  document.addEventListener('keydown', function (ev) {
    if (ev.key !== '/' || ev.ctrlKey || ev.metaKey || ev.altKey) return
    var el = document.activeElement
    var tag = el && el.tagName
    // Never steal "/" from a field the reader is typing into.
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (el && el.isContentEditable)) return
    ev.preventDefault()
    open()
  })
})()
