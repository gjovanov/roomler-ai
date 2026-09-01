/* SPDX-License-Identifier: AGPL-3.0-only
 * Copyright (C) 2026 G ROX EOOD
 *
 * FR-58 (#1165) — page behaviour: OS-tab persistence, copy buttons, the
 * mobile nav, and TOC scroll-spy.
 *
 * Every one of these is a PROGRESSIVE ENHANCEMENT. With JavaScript off:
 * the OS tabs still switch (CSS `:checked`), code is still selectable, the
 * sidebar is still reachable (it renders in the flow below 900px once the
 * burger is gone), and the TOC is still a list of working anchors. Nothing
 * here is load-bearing for reading the documentation.
 */
;(function () {
  'use strict'

  var OS_KEY = 'roomler-docs-os'
  var VALID_OS = { windows: 1, macos: 1, linux: 1 }

  function readOs() {
    try {
      var v = localStorage.getItem(OS_KEY)
      return VALID_OS[v] ? v : null
    } catch (e) {
      return null
    }
  }

  function writeOs(os) {
    try {
      localStorage.setItem(OS_KEY, os)
    } catch (e) {
      /* private window / site data blocked — the choice just does not persist */
    }
  }

  // ── OS tabs ───────────────────────────────────────────────────────────
  function syncOs(os, groups) {
    for (var i = 0; i < groups.length; i++) {
      var radio = groups[i].querySelector('.os-radio--' + os)
      // A group that does not offer this OS keeps whatever it had — a
      // Linux-only block must not blank out because the reader is on macOS.
      if (radio && !radio.checked) radio.checked = true
    }
  }

  function initOsTabs() {
    var groups = document.querySelectorAll('[data-os-tabs]')
    if (!groups.length) {
      document.documentElement.removeAttribute('data-os')
      return
    }

    var stored = readOs()
    if (stored) syncOs(stored, groups)
    // Hand control back to the CSS-only mechanism now that the real radios
    // hold the state. Leaving the attribute would pin every group to one OS
    // and defeat the per-group fallback above.
    document.documentElement.removeAttribute('data-os')

    document.addEventListener('change', function (ev) {
      var t = ev.target
      if (!t || !t.classList || !t.classList.contains('os-radio')) return
      var os = null
      for (var key in VALID_OS) {
        if (t.classList.contains('os-radio--' + key)) os = key
      }
      if (!os) return
      writeOs(os)
      syncOs(os, groups)
    })
  }

  // ── copy buttons ──────────────────────────────────────────────────────
  function initCopy() {
    document.addEventListener('click', function (ev) {
      var btn = ev.target && ev.target.closest ? ev.target.closest('.code-copy') : null
      if (!btn) return
      var block = btn.closest('[data-code]')
      var code = block && block.querySelector('code')
      if (!code) return

      var done = function () {
        btn.classList.add('is-copied')
        btn.setAttribute('aria-label', 'Copied')
        setTimeout(function () {
          btn.classList.remove('is-copied')
          btn.setAttribute('aria-label', 'Copy code to clipboard')
        }, 1400)
      }

      // navigator.clipboard is undefined outside a secure context, and the
      // docs are readable over plain http on a self-hosted LAN instance —
      // so the legacy path is a real fallback here, not belt-and-braces.
      if (navigator.clipboard && window.isSecureContext) {
        navigator.clipboard.writeText(code.textContent).then(done, function () {})
        return
      }
      var ta = document.createElement('textarea')
      ta.value = code.textContent
      ta.setAttribute('readonly', '')
      ta.style.position = 'fixed'
      ta.style.opacity = '0'
      document.body.appendChild(ta)
      ta.select()
      try {
        document.execCommand('copy')
        done()
      } catch (e) {
        /* nothing sensible to do; the text is still selectable by hand */
      }
      document.body.removeChild(ta)
    })
  }

  // ── mobile nav ────────────────────────────────────────────────────────
  function initNav() {
    var toggle = document.querySelector('[data-nav-toggle]')
    var nav = document.querySelector('[data-nav]')
    if (!toggle || !nav) return
    toggle.addEventListener('click', function () {
      var open = nav.classList.toggle('is-open')
      toggle.setAttribute('aria-expanded', open ? 'true' : 'false')
    })
  }

  // ── TOC scroll-spy ────────────────────────────────────────────────────
  function initScrollSpy() {
    var links = document.querySelectorAll('.toc__item a')
    if (!links.length || !('IntersectionObserver' in window)) return

    var byId = {}
    var targets = []
    for (var i = 0; i < links.length; i++) {
      var id = decodeURIComponent(links[i].getAttribute('href').slice(1))
      var el = document.getElementById(id)
      if (!el) continue
      byId[id] = links[i]
      targets.push(el)
    }
    if (!targets.length) return

    var visible = {}
    var obs = new IntersectionObserver(
      function (entries) {
        for (var i = 0; i < entries.length; i++) {
          visible[entries[i].target.id] = entries[i].isIntersecting
        }
        // The first heading currently in the band wins, so the highlight
        // reads top-down like the page does rather than jumping to
        // whichever entry the observer happened to report last.
        var currentId = null
        for (var j = 0; j < targets.length; j++) {
          if (visible[targets[j].id]) {
            currentId = targets[j].id
            break
          }
        }
        for (var id in byId) byId[id].classList.remove('is-current')
        if (currentId && byId[currentId]) byId[currentId].classList.add('is-current')
      },
      { rootMargin: '-76px 0px -68% 0px', threshold: 0 },
    )
    for (var k = 0; k < targets.length; k++) obs.observe(targets[k])
  }

  function boot() {
    initOsTabs()
    initCopy()
    initNav()
    initScrollSpy()
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot)
  } else {
    boot()
  }
})()
