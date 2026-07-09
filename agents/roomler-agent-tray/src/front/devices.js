/*
 * Devices view (unification P2). Polls `cmd_device_view` and paints this node
 * + its overlay peers with their connection type — the Tailscale-style "which
 * of my devices is reachable, and how" list.
 *
 * No bundler (see status.js header); pure ES2020; `window.__TAURI__.core.invoke`
 * via `withGlobalTauri`. Peer-supplied strings (names, IPs from OTHER devices)
 * are written with textContent only — never innerHTML — so a hostile device
 * name can't inject markup.
 */
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {});
  function $(id) { return document.getElementById(id); }
  function show(el) { if (el) el.hidden = false; }
  function hide(el) { if (el) el.hidden = true; }
  function setText(id, t) { const el = $(id); if (el) el.textContent = t; }

  // Wire values are snake_case (ConnectionType serde); map to a label + a CSS
  // badge class. `tunnel` is forward-compat — the agent daemon doesn't emit it
  // until the tunnel-client folds in (P3).
  const CONN = {
    direct: 'Direct',
    relay: 'Relay',
    tunnel: 'Tunnel',
    blocked: 'Blocked',
    offline: 'Offline',
  };

  async function refresh() {
    let view;
    try {
      view = await invoke('cmd_device_view');
    } catch (err) {
      // cmd_device_view is never-fail by contract; a throw here is unexpected.
      console.error('cmd_device_view failed', err);
      return;
    }
    paint(view);
  }

  function paint(view) {
    hide($('devices-loading'));

    // Zero-state 1 — the daemon's LocalAPI wasn't reachable (agent not running).
    if (!view.available) {
      show($('devices-unavailable'));
      hide($('devices-self'));
      return;
    }
    hide($('devices-unavailable'));
    show($('devices-self'));

    const s = view.status || {};
    setText('dv-self-name', s.name || '—');
    setText('dv-self-ip', s.overlay_ip || '(no overlay IP)');
    setText('dv-self-conn', s.connected ? 'connected' : 'disconnected');
    setText('dv-self-version', s.version || '—');

    const body = $('dv-peers-body');
    body.replaceChildren();

    const peers = view.peers || [];
    if (peers.length === 0) {
      // Zero-state 2/3 — connected-but-no-peers (overlay off / alone) vs the
      // WS being down (peers are hidden until re-synced).
      hide($('dv-peers-table'));
      const empty = $('dv-peers-empty');
      empty.textContent = s.connected
        ? 'No peers — the overlay isn’t enabled on this device, or no other devices are online.'
        : 'Disconnected from the server — the peer list is unavailable until reconnected.';
      show(empty);
      return;
    }

    hide($('dv-peers-empty'));
    show($('dv-peers-table'));
    for (const p of peers) {
      body.appendChild(peerRow(p));
    }
  }

  function peerRow(p) {
    const tr = document.createElement('tr');
    tr.appendChild(nameCell(p));
    tr.appendChild(textCell(p.overlay_ip || '—', 'mono'));
    tr.appendChild(badgeCell(p.connection));
    tr.appendChild(textCell(p.rtt_ms != null ? `${p.rtt_ms} ms` : '—'));
    return tr;
  }

  function textCell(text, cls) {
    const td = document.createElement('td');
    td.textContent = text;
    if (cls) td.className = cls;
    return td;
  }

  function nameCell(p) {
    const td = document.createElement('td');
    const dot = document.createElement('span');
    dot.className = 'dot ' + (p.online ? 'dot-on' : 'dot-off');
    td.appendChild(dot);
    td.appendChild(document.createTextNode(' ' + (p.name || '(unnamed)')));
    return td;
  }

  function badgeCell(conn) {
    const key = String(conn || 'offline').toLowerCase();
    const td = document.createElement('td');
    const span = document.createElement('span');
    span.className = 'badge badge-' + key;
    span.textContent = CONN[key] || key;
    td.appendChild(span);
    return td;
  }

  document.addEventListener('DOMContentLoaded', () => {
    void refresh();
    // Peers change as carriers come up / fall back; 2s keeps the view live
    // without hammering the local pipe.
    setInterval(refresh, 2000);
  });
})();
