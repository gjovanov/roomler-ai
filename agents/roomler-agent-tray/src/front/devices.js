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

  // Last ping outcome per target (overlay IP / name). The peers table repaints
  // every 2s (replaceChildren), so results are kept here and re-rendered rather
  // than living only in the transient DOM.
  const pingResults = new Map();

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
    // Both families when the daemon publishes the derived v6.
    setText(
      'dv-self-ip',
      s.overlay_ip
        ? s.overlay_ip6
          ? `${s.overlay_ip} · ${s.overlay_ip6}`
          : s.overlay_ip
        : '(no overlay IP)',
    );
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
    tr.appendChild(textCell(p.overlay_ip6 || '—', 'mono'));
    tr.appendChild(badgeCell(p.connection));
    tr.appendChild(textCell(p.rtt_ms != null ? `${p.rtt_ms} ms` : '—'));
    tr.appendChild(pingCell(p));
    return tr;
  }

  // A live ICMP ping button per peer — resolves the peer by overlay IP (or name)
  // and pings it over the userspace netstack via `cmd_ping`. The result (RTT or a
  // failure with the daemon's message as a tooltip) persists across repaints via
  // `pingResults`. Disabled for a peer with no address to target.
  function pingCell(p) {
    const td = document.createElement('td');
    td.className = 'ping-cell';
    const target = p.overlay_ip || p.name;
    const btn = document.createElement('button');
    btn.className = 'ping-btn';
    btn.textContent = 'Ping';
    const out = document.createElement('span');
    out.className = 'ping-result';

    const prior = target ? pingResults.get(target) : null;
    if (prior) {
      out.classList.add(prior.ok ? 'ok' : 'err');
      out.textContent = prior.text;
      if (prior.title) out.title = prior.title;
    }
    if (!target) {
      btn.disabled = true;
      btn.title = 'No overlay address to ping';
    } else {
      btn.addEventListener('click', () => void runPing(target, btn, out));
    }
    td.appendChild(btn);
    td.appendChild(document.createTextNode(' '));
    td.appendChild(out);
    return td;
  }

  async function runPing(target, btn, out) {
    btn.disabled = true;
    const label = btn.textContent;
    btn.textContent = '…';
    out.className = 'ping-result';
    out.textContent = '';
    out.removeAttribute('title');
    try {
      const r = await invoke('cmd_ping', { target });
      const text = `${Number(r.rtt_ms).toFixed(1)} ms`;
      pingResults.set(target, { ok: true, text });
      out.classList.add('ok');
      out.textContent = text;
    } catch (err) {
      const msg = String(err);
      pingResults.set(target, { ok: false, text: 'failed', title: msg });
      out.classList.add('err');
      out.textContent = 'failed';
      out.title = msg;
    } finally {
      btn.textContent = label;
      btn.disabled = false;
    }
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
