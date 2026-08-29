/*
 * FR-27 — the session banner's logic.
 *
 * Reads live remote-control sessions from the daemon and offers a Disconnect
 * that takes the SAME teardown path as the native Windows overlay's button
 * (LocalAPI RcDisconnect -> the signalling loop's kill channel), rather than a
 * second, subtly different way to end a session.
 *
 * Every daemon-supplied string goes in with textContent: controller names come
 * from another machine.
 */
;(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {})
  const $ = (id) => document.getElementById(id)

  let sessions = []
  let busy = false

  function ago(startedAtMs) {
    if (!startedAtMs) return ''
    const secs = Math.max(0, Math.round((Date.now() - startedAtMs) / 1000))
    if (secs < 60) return `${secs}s`
    const mins = Math.round(secs / 60)
    return mins < 60 ? `${mins} min` : `${Math.round(mins / 60)} h`
  }

  function paint() {
    if (sessions.length === 0) {
      $('v-who').textContent = 'Being viewed'
      $('v-sub').textContent = ''
      return
    }
    const first = sessions[0]
    const extra = sessions.length - 1
    $('v-who').textContent =
      `Being viewed by ${first.controller_name || 'a remote operator'}` +
      (extra > 0 ? ` +${extra}` : '')

    // The GRANT matters as much as the name: "watching" and "typing on this
    // machine" are different things to be told about.
    const perms = (first.permissions || '').toUpperCase()
    const typing = perms.includes('INPUT')
    const bits = [typing ? 'keyboard + mouse' : 'view only']
    if (first.org) bits.push(first.org)
    const age = ago(first.started_at_ms)
    if (age) bits.push(age)
    $('v-sub').textContent = bits.join(' · ')
  }

  async function poll() {
    try {
      const next = await invoke('cmd_rc_sessions')
      sessions = Array.isArray(next) ? next : []
    } catch (_) {
      // Daemon down: keep the last known text rather than blanking the strip.
      // The window is hidden by the watcher moments later anyway, and an
      // indicator that flickers to empty is worse than one that is briefly
      // stale.
      return
    }
    paint()
  }

  $('v-stop').addEventListener('click', async () => {
    if (busy || sessions.length === 0) return
    busy = true
    const btn = $('v-stop')
    btn.disabled = true
    btn.textContent = 'Stopping…'
    try {
      // Disconnect EVERY live session, not just the first. The button says
      // "Disconnect" next to "being viewed by X +2"; ending one of three and
      // leaving the strip up would read as a failure.
      for (const s of sessions) await invoke('cmd_rc_disconnect', { session: s.session_id })
    } catch (e) {
      $('v-sub').textContent = 'Failed: ' + e
    } finally {
      busy = false
      btn.disabled = false
      btn.textContent = 'Disconnect'
    }
  })

  setInterval(poll, 1000)
  poll()
})()
