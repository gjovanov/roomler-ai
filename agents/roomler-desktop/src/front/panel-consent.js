// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
/*
 * FR-27 — the consent panel's logic.
 *
 * Polls the daemon (through the tray backend) for pending prompts and renders
 * the first one. Self-contained: no app shell, no router, no shared store —
 * this window has to work when the main window has never been opened.
 *
 * The KINDS are not the same question. "Approve" over a screen share and
 * "Approve" over a command running as SYSTEM/root deserve different words, and
 * before FR-27 exec and SSH prompts reached no UI at all, so there was nothing
 * to get wrong. An ABSENT kind means a pre-FR-27 daemon, which only ever wrote
 * remote-control prompts — hence `rc` as the fallback, not "unknown".
 *
 * #3873 — a click is acted on, or the person reads why not, and the app's log
 * has it either way. Every Approve and Deny goes to the app, which logs it
 * when it arrives and again with its outcome: nothing returns before the
 * send. The buttons are off only while no question is shown, an answer is on
 * its way, or this window answered the question, and the status line says
 * which. It is an element of its own: the countdown repaints every second.
 *
 * #3987 — each click also says how long its question has been shown. The app
 * holds back a click within a second of a new question coming up.
 */
;(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {})

  const KINDS = {
    rc: {
      title: 'Remote control request',
      verb: 'is requesting to control this device',
    },
    exec: {
      title: 'Command execution request',
      verb: 'wants to run a command on this device',
    },
    ssh: {
      title: 'SSH session request',
      verb: 'wants to open an SSH session on this device',
    },
    // FR-85 P3b — a controller already in a session asks to keep a COPY of the
    // screen. Its own question: seeing and recording are different answers.
    record: {
      title: 'Screen recording request',
      verb: 'wants to record this screen',
    },
  }

  // #3873 — how long the page waits for the app's reply to a click. Above the
  // app's own 10 s on the daemon's answer (`CONSENT_ANSWER_WITHIN` in
  // commands.rs), so it fires only when the app itself never answers; the
  // buttons then come back instead of staying off.
  const PAGE_LIMIT_MS = 15000

  const $ = (id) => document.getElementById(id)
  let active = null // session id of the question shown, null = none
  let expiresAt = 0 // unix ms, 0 = unknown
  let sending = false // an answer is on its way
  let answered = null // the question this window answered
  let shownAt = 0 // #3987: performance.now() when the question shown came up

  function setStatus(text) {
    const el = $('p-status')
    el.textContent = text
    el.hidden = text === ''
  }

  // The buttons work while a question is shown that this window has not
  // answered and no answer is on its way. Off, the status line says why.
  function paintButtons() {
    const off = !active || sending || answered === active
    $('p-approve').disabled = off
    $('p-deny').disabled = off
  }

  function render(pc) {
    active = pc.session_id
    answered = null
    shownAt = performance.now()
    const kind = KINDS[pc.kind] || KINDS.rc
    $('p-title').textContent = kind.title
    $('p-verb').textContent = kind.verb
    $('p-who').textContent = pc.controller_name || 'A remote operator'
    $('p-lead').hidden = false

    const org = (pc.org || '').trim()
    $('p-org').textContent = org
    $('p-org-row').hidden = org === ''

    const detail = (pc.detail || '').trim()
    $('p-detail').textContent = detail
    $('p-detail-row').hidden = detail === ''

    // Only remote control carries a permission set. "Permissions: —" above an
    // exec prompt reads as "nothing needed", which is the opposite of true for
    // a command running as the daemon identity.
    $('p-perms').textContent = pc.permissions || ''
    $('p-perms-row').hidden = !pc.permissions

    // A deadline from the daemon beats a timer started here: re-reading the
    // list must not restart the countdown, and this window can be created
    // partway through a prompt's life.
    expiresAt = typeof pc.expires_at_ms === 'number' && pc.expires_at_ms > 0 ? pc.expires_at_ms : 0
    setStatus('')
    paintCountdown()
    paintButtons()
  }

  // #3873 — nothing is waiting: the question goes, so nothing on screen looks
  // as if it could still be answered.
  function clear() {
    active = null
    answered = null
    expiresAt = 0
    $('p-lead').hidden = true
    $('p-org-row').hidden = true
    $('p-detail-row').hidden = true
    $('p-perms-row').hidden = true
    setStatus('No question is waiting.')
    paintCountdown()
    paintButtons()
  }

  function paintCountdown() {
    const el = $('p-countdown')
    if (!expiresAt) {
      el.textContent = ''
      return
    }
    const left = Math.max(0, Math.round((expiresAt - Date.now()) / 1000))
    // m:ss above a minute — the plain-prompt window is 5 min (FR-34), and
    // "Expires in 287s" reads worse than "Expires in 4:47".
    const pretty = left >= 60 ? `${Math.floor(left / 60)}:${String(left % 60).padStart(2, '0')}` : `${left}s`
    el.textContent = left > 0 ? `Expires in ${pretty}` : 'Expired'
  }

  async function decide(approve) {
    // #3873 — the click is sent before anything else can stop it: one with no
    // question shown goes too, and the app logs it. Before #3873 this left
    // early, without a word, while an earlier answer was "on its way", and an
    // answer that never came back kept it so for good: on a Linux system
    // install the second of two clicks on a question this window showed left
    // no line anywhere.
    const session = active
    sending = true
    paintButtons()
    setStatus(approve ? 'Approving…' : 'Denying…')
    let timer = null
    const limit = new Promise((_, reject) => {
      timer = setTimeout(
        () => reject(new Error(`No answer from the app within ${PAGE_LIMIT_MS / 1000} s. Try again.`)),
        PAGE_LIMIT_MS
      )
    })
    try {
      await Promise.race([
        invoke(approve ? 'cmd_consent_approve' : 'cmd_consent_deny', { session, shown: Math.round(performance.now() - shownAt) }),
        limit,
      ])
      answered = session
      setStatus(approve ? 'Approved.' : 'Denied.')
    } catch (e) {
      // The app's words: the daemon's own refusal, or why it was not asked.
      setStatus(String(e && e.message ? e.message : e))
    } finally {
      clearTimeout(timer)
      sending = false
      paintButtons()
    }
  }

  async function poll() {
    // An answer on its way decides what shows next.
    if (sending) return
    let pending
    try {
      pending = await invoke('cmd_get_pending_consents')
    } catch (_) {
      // #3873 — a read that failed keeps the question shown: the daemon may
      // still be asking it, and a click on it reaches the app, which logs
      // what becomes of it. The app logs the failed read too.
      return
    }
    if (sending || !Array.isArray(pending)) return
    // FR-27 — skip anything the DAEMON is already showing natively. The marker
    // is written either way (it is also what `roomler consent --list` reads),
    // so without this filter a Windows/X11/macOS host would get two Approve
    // buttons for one decision — which is how someone approves the wrong
    // thing. Absent `surface` = a pre-FR-27 daemon, where nothing was native.
    pending = pending.filter((p) => p.surface !== 'native')
    if (pending.length === 0) {
      clear()
      return
    }
    // Keep showing the current one while it is still pending; otherwise take
    // the first outstanding request (it may have been resolved elsewhere).
    if (!active || !pending.some((p) => p.session_id === active)) render(pending[0])
  }

  $('p-approve').addEventListener('click', () => decide(true))
  $('p-deny').addEventListener('click', () => decide(false))
  // Escape denies. A prompt you cannot dismiss with the obvious key is a
  // prompt people learn to click through. It is the Deny button's key, so it
  // acts exactly while that button does (#3873).
  window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && !$('p-deny').disabled) decide(false)
  })

  setInterval(poll, 750)
  setInterval(paintCountdown, 1000)
  paintButtons()
  poll()
})()
