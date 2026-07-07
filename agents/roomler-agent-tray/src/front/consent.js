// Phase 3 — remote-control consent popup.
//
// Polls the tray backend for `.pending` markers the agent drops when a remote
// session is awaiting a decision, renders an Approve/Deny modal, and writes the
// operator's choice through the existing cmd_consent_approve / cmd_consent_deny
// commands. Self-contained: it creates its own modal DOM so it can be dropped
// onto any page (status / onboarding / settings) via a single <script> tag.
//
// The Rust-side watcher brings the window forward when a new marker appears;
// this loop does the rendering and the decision.
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {})

  let activeSession = null // session id currently shown in the modal
  let busy = false // an approve/deny call is in flight

  function ensureModal() {
    let el = document.getElementById('consent-modal')
    if (el) return el
    el = document.createElement('div')
    el.id = 'consent-modal'
    el.hidden = true
    el.innerHTML = [
      '<div class="consent-backdrop"></div>',
      '<div class="consent-card" role="dialog" aria-modal="true" aria-labelledby="consent-title">',
      '  <h2 id="consent-title">Remote control request</h2>',
      '  <p class="consent-lead"><strong id="consent-who"></strong> is requesting to control this device.</p>',
      '  <p class="consent-perms">Permissions: <span id="consent-perms" class="mono"></span></p>',
      '  <div class="consent-actions">',
      '    <button type="button" id="consent-deny" class="consent-btn consent-deny">Deny</button>',
      '    <button type="button" id="consent-approve" class="consent-btn consent-approve">Approve</button>',
      '  </div>',
      '  <p class="consent-hint muted small" id="consent-hint"></p>',
      '</div>',
    ].join('\n')
    document.body.appendChild(el)
    el.querySelector('#consent-approve').addEventListener('click', () => decide(true))
    el.querySelector('#consent-deny').addEventListener('click', () => decide(false))
    return el
  }

  async function decide(approve) {
    if (!activeSession || busy) return
    busy = true
    const hint = document.getElementById('consent-hint')
    if (hint) hint.textContent = approve ? 'Approving…' : 'Denying…'
    try {
      await invoke(approve ? 'cmd_consent_approve' : 'cmd_consent_deny', { session: activeSession })
      hide()
    } catch (e) {
      if (hint) hint.textContent = 'Failed: ' + e
    } finally {
      busy = false
    }
  }

  function show(pc) {
    const el = ensureModal()
    activeSession = pc.session_id
    document.getElementById('consent-who').textContent = pc.controller_name || 'A remote operator'
    document.getElementById('consent-perms').textContent = pc.permissions || '—'
    document.getElementById('consent-hint').textContent = ''
    el.hidden = false
  }

  function hide() {
    const el = document.getElementById('consent-modal')
    if (el) el.hidden = true
    activeSession = null
  }

  async function poll() {
    let pending
    try {
      pending = await invoke('cmd_get_pending_consents')
    } catch (_) {
      return // backend not ready / dir absent — stay quiet
    }
    if (Array.isArray(pending) && pending.length > 0) {
      // Keep showing the current one if it's still pending; otherwise show the
      // first outstanding request (an operator may have resolved one elsewhere).
      if (!activeSession || !pending.some((p) => p.session_id === activeSession)) {
        show(pending[0])
      }
    } else if (activeSession && !busy) {
      hide() // resolved (e.g. via CLI, or timed out) — dismiss the modal
    }
  }

  setInterval(poll, 1500)
  poll()
})()
