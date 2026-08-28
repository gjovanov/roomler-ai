/*
 * FR-27 — the consent panel's logic.
 *
 * Polls the daemon (through the tray backend) for pending prompts and renders
 * the first one. Self-contained: no app shell, no router, no shared store —
 * this window has to work when the main window has never been opened.
 *
 * The three KINDS are not the same question. "Approve" over a screen share and
 * "Approve" over a command running as SYSTEM/root deserve different words, and
 * before FR-27 exec and SSH prompts reached no UI at all, so there was nothing
 * to get wrong. An ABSENT kind means a pre-FR-27 daemon, which only ever wrote
 * remote-control prompts — hence `rc` as the fallback, not "unknown".
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
  }

  const $ = (id) => document.getElementById(id)
  let active = null // session id currently rendered
  let expiresAt = 0 // unix ms, 0 = unknown
  let busy = false

  function render(pc) {
    active = pc.session_id
    const kind = KINDS[pc.kind] || KINDS.rc
    $('p-title').textContent = kind.title
    $('p-verb').textContent = kind.verb
    $('p-who').textContent = pc.controller_name || 'A remote operator'

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
    paintCountdown()
  }

  function paintCountdown() {
    const el = $('p-countdown')
    if (!expiresAt) {
      el.textContent = ''
      return
    }
    const left = Math.max(0, Math.round((expiresAt - Date.now()) / 1000))
    el.textContent = left > 0 ? `Expires in ${left}s` : 'Expired'
  }

  async function decide(approve) {
    if (!active || busy) return
    busy = true
    $('p-countdown').textContent = approve ? 'Approving…' : 'Denying…'
    try {
      await invoke(approve ? 'cmd_consent_approve' : 'cmd_consent_deny', { session: active })
      active = null
    } catch (e) {
      $('p-countdown').textContent = 'Failed: ' + e
    } finally {
      busy = false
    }
  }

  async function poll() {
    let pending = []
    try {
      pending = await invoke('cmd_get_pending_consents')
    } catch (_) {
      return // backend not ready — stay quiet, the window is about to hide
    }
    if (!Array.isArray(pending) || pending.length === 0) {
      active = null
      expiresAt = 0
      return
    }
    // Keep showing the current one while it is still pending; otherwise take
    // the first outstanding request (it may have been resolved elsewhere).
    if (!active || !pending.some((p) => p.session_id === active)) render(pending[0])
  }

  $('p-approve').addEventListener('click', () => decide(true))
  $('p-deny').addEventListener('click', () => decide(false))
  // Escape denies. A prompt you cannot dismiss with the obvious key is a
  // prompt people learn to click through.
  window.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') decide(false)
  })

  setInterval(poll, 750)
  setInterval(paintCountdown, 1000)
  poll()
})()
