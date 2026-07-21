/*
 * Devices view: the network's peers with their live connection type — the
 * Tailscale-style "which of my devices is reachable, and how" table.
 *
 * Renders from the central store (`cmd_device_view`, polled by app.js).
 * Peer-supplied strings (names, IPs from OTHER devices) are written with
 * textContent only — never innerHTML — so a hostile device name can't
 * inject markup.
 */
(function () {
  'use strict';
  const { $, invoke, show, hide, fmtRelative, on } = window.Roomler;

  // Wire values are snake_case (ConnectionType serde); map to a label + a CSS
  // badge class.
  const CONN = {
    direct: 'Direct',
    relay: 'Relay',
    tunnel: 'Tunnel',
    blocked: 'Blocked',
    offline: 'Offline',
  };

  // Last ping outcome per target (overlay IP / name). The peers table repaints
  // every 2 s (replaceChildren), so results are kept here and re-rendered
  // rather than living only in the transient DOM.
  const pingResults = new Map();

  function paint(view) {
    // Zero-state 1 — the daemon's LocalAPI wasn't reachable (service down).
    if (!view.available) {
      show($('devices-unavailable'));
      hide($('devices-content'));
      return;
    }
    hide($('devices-unavailable'));
    show($('devices-content'));

    const s = view.status || {};
    const body = $('dv-peers-body');
    const peers = view.peers || [];

    if (peers.length === 0) {
      // Zero-state 2/3 — connected-but-no-peers vs the WS being down.
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
    body.replaceChildren(...peers.map(peerRow));
  }

  function peerRow(p) {
    const tr = document.createElement('tr');
    tr.appendChild(nameCell(p));
    tr.appendChild(ipCell(p));
    tr.appendChild(badgeCell(p));
    tr.appendChild(pingCell(p));
    return tr;
  }

  // v4 with the derived v6 as a second line — one column instead of two, so
  // the table fits the window's minimum width without truncation.
  function ipCell(p) {
    const td = document.createElement('td');
    td.className = 'mono';
    td.appendChild(document.createTextNode(p.overlay_ip || '—'));
    if (p.overlay_ip6) {
      const sub = document.createElement('div');
      sub.className = 'muted small mono';
      sub.textContent = p.overlay_ip6;
      td.appendChild(sub);
    }
    return td;
  }

  // A live ICMP ping button per peer — resolves the peer by overlay IP (or
  // name) and pings it over the userspace netstack via `cmd_ping`. The result
  // (RTT, or a failure with the daemon's message as a tooltip) persists across
  // repaints via `pingResults`. Disabled for a peer with no address to target.
  function pingCell(p) {
    const td = document.createElement('td');
    td.className = 'ping-cell';
    const target = p.overlay_ip || p.name;
    const btn = document.createElement('button');
    btn.className = 'sm';
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
      const text = Number(r.rtt_ms).toFixed(1) + ' ms';
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

  // Name + online dot, with data-freshness as a second line ("active 5m
  // ago") — last_seen_ms is only published for peers with a live carrier,
  // so it reads as "how fresh is this row", not an offline timestamp.
  function nameCell(p) {
    const td = document.createElement('td');
    const dot = document.createElement('span');
    dot.className = 'dot ' + (p.online ? 'dot-on' : 'dot-off');
    td.appendChild(dot);
    td.appendChild(document.createTextNode(' ' + (p.name || '(unnamed)')));
    if (p.last_seen_ms != null) {
      const sub = document.createElement('div');
      sub.className = 'muted small';
      sub.textContent = 'active ' + fmtRelative(p.last_seen_ms);
      td.appendChild(sub);
    }
    return td;
  }

  // Connection badge with the live RTT beside it ("Direct · 4 ms").
  function badgeCell(p) {
    const key = String(p.connection || 'offline').toLowerCase();
    const td = document.createElement('td');
    td.className = 'ping-cell';
    const span = document.createElement('span');
    span.className = 'badge badge-' + key;
    span.textContent = CONN[key] || key;
    // For a relay peer the daemon reports both coturn-relayed endpoints
    // (rc.187) — surface them as a tooltip for same-vs-cross-worker triage.
    if (p.relay_local || p.relay_dst) {
      span.title = 'relay ' + (p.relay_local || '?') + ' → ' + (p.relay_dst || '?');
    }
    td.appendChild(span);
    if (p.rtt_ms != null) {
      const rtt = document.createElement('span');
      rtt.className = 'muted small';
      rtt.textContent = ' · ' + p.rtt_ms + ' ms';
      td.appendChild(rtt);
    }
    return td;
  }

  document.addEventListener('DOMContentLoaded', () => {
    on('deviceView', paint);
  });
})();
