/*
 * Tunnels view: declared, daemon-supervised routes (P6) + the live-forwards
 * table (`cmd_flows` — per-forward transport, connection count and byte
 * counters). Routes and flows poll ONLY while this view is visible; the
 * route list is also broadcast (`roomler:routes`) so the Overview can show
 * an active-count without polling again.
 *
 * The add-route form offers a picker of known agent peers (from the central
 * device view — PeerInfo.agent_id is the join key) with a free-text escape
 * hatch for an id that isn't in the mesh list.
 *
 * Conventions match devices.js: every daemon-supplied string (ids, error
 * reasons, names) is written with textContent only — never innerHTML.
 */
(function () {
  'use strict';
  const { $, invoke, show, hide, fmtBytes, on, get, currentView } = window.Roomler;

  let busy = false; // one mutation in flight at a time
  let lastRoutes = [];
  let lastFlows = [];

  const CUSTOM = '__custom__';

  function visible() { return currentView() === 'tunnels'; }

  async function refresh() {
    if (!visible()) return;
    try {
      [lastRoutes, lastFlows] = await Promise.all([
        invoke('cmd_route_list'),
        invoke('cmd_flows'),
      ]);
    } catch (err) {
      // Both commands are never-fail by contract.
      console.error('tunnels refresh failed', err);
      return;
    }
    paintRoutes(lastRoutes);
    paintFlows(lastFlows);
    document.dispatchEvent(new CustomEvent('roomler:routes', { detail: lastRoutes }));
  }

  /* ── peer lookups ───────────────────────────────────────────────── */

  function agentPeers() {
    const dv = get('deviceView');
    return ((dv && dv.peers) || []).filter((p) => p.agent_id);
  }

  // agent-id (24-hex) → display name, for the routes/flows Device columns.
  function peerName(agentId) {
    if (!agentId) return null;
    const hit = agentPeers().find((p) => p.agent_id === agentId);
    return hit ? hit.name || null : null;
  }

  function deviceLabel(agentId) {
    const name = peerName(agentId);
    if (name) return name;
    if (!agentId) return '—';
    return agentId.length > 10 ? agentId.slice(0, 8) + '…' : agentId;
  }

  /* ── declared routes ────────────────────────────────────────────── */

  // Compact human word for a RouteState (adjacently tagged on `state`).
  function stateLabel(s) {
    switch (s.state) {
      case 'disabled': return { text: 'disabled', cls: 'muted' };
      case 'pending': return { text: 'pending', cls: 'muted' };
      case 'active': return { text: 'active', cls: 'ok' };
      case 'backoff':
        return { text: 'retrying in ' + s.next_retry_secs + 's: ' + s.last_error, cls: 'warn' };
      case 'failed':
        return { text: 'FAILED: ' + s.reason, cls: 'err' };
      default: return { text: s.state || '—', cls: 'muted' };
    }
  }

  function td(text, cls) {
    const el = document.createElement('td');
    el.textContent = text;
    if (cls) el.className = cls;
    return el;
  }

  function actionBtn(label, danger, onClick) {
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.className = danger ? 'sm danger' : 'sm';
    b.style.marginRight = '6px';
    b.addEventListener('click', async () => {
      if (busy) return;
      busy = true;
      b.disabled = true;
      try {
        await onClick();
      } catch (err) {
        // Mutation errors are actionable — surface them on the form slot.
        const slot = $('tn-form-error');
        if (slot) { slot.textContent = String(err); show(slot); }
      } finally {
        busy = false;
        await refresh();
      }
    });
    return b;
  }

  // The flow backing an active route, for its live Traffic column.
  function flowForRoute(state) {
    if (!state || state.state !== 'active' || !state.flow_id) return null;
    return lastFlows.find((f) => f.id === state.flow_id) || null;
  }

  function paintRoutes(routes) {
    const empty = $('tn-empty');
    const table = $('tn-table');
    const body = $('tn-body');
    if (!body) return;

    if (!routes.length) {
      show(empty); hide(table);
      return;
    }
    hide(empty); show(table);

    const rows = routes.map((r) => {
      const d = r.route;
      const tr = document.createElement('tr');
      tr.appendChild(td(d.id, 'mono'));
      tr.appendChild(td(d.kind));
      tr.appendChild(td('127.0.0.1:' + d.local, 'mono'));
      tr.appendChild(td(d.remote || '—', 'mono'));
      tr.appendChild(td(deviceLabel(d.node)));
      const st = stateLabel(r.state);
      const stateTd = td(st.text, st.cls);
      // Live traffic rides in the state cell ("active · ↓ 2 MiB ↑ 1 MiB") —
      // one column fewer keeps the table inside a narrow window.
      const flow = flowForRoute(r.state);
      if (flow) {
        const traffic = document.createElement('span');
        traffic.className = 'muted small';
        traffic.textContent =
          ' · ↓ ' + fmtBytes(flow.bytes_in) + ' ↑ ' + fmtBytes(flow.bytes_out);
        stateTd.appendChild(traffic);
      }
      tr.appendChild(stateTd);

      const actions = document.createElement('td');
      if (d.enabled) {
        actions.appendChild(actionBtn('Disable', false, () =>
          invoke('cmd_route_set_enabled', { id: d.id, enabled: false })));
      } else {
        actions.appendChild(actionBtn('Enable', false, () =>
          invoke('cmd_route_set_enabled', { id: d.id, enabled: true })));
      }
      actions.appendChild(actionBtn('Remove', true, () =>
        invoke('cmd_route_remove', { id: d.id })));
      tr.appendChild(actions);
      return tr;
    });
    body.replaceChildren(...rows);
  }

  /* ── live flows ─────────────────────────────────────────────────── */

  function paintFlows(flows) {
    const card = $('tn-flows-card');
    const body = $('tn-flows-body');
    if (!body) return;
    if (!flows.length) {
      hide(card);
      return;
    }
    show(card);
    body.replaceChildren(...flows.map((f) => {
      const tr = document.createElement('tr');
      tr.appendChild(td(f.kind));
      tr.appendChild(td(f.local_addr, 'mono'));
      tr.appendChild(td(f.target || '—', 'mono'));
      tr.appendChild(td(deviceLabel(f.node)));
      tr.appendChild(td(f.transport));
      tr.appendChild(td(String(f.active_flows)));
      tr.appendChild(td('↓ ' + fmtBytes(f.bytes_in) + ' ↑ ' + fmtBytes(f.bytes_out), 'small'));
      return tr;
    }));
  }

  /* ── add-route form ─────────────────────────────────────────────── */

  function paintNodeOptions() {
    const sel = $('tn-node');
    if (!sel) return;
    // Don't yank a dropdown the operator has open — the 2 s repaint would
    // close it mid-pick.
    if (document.activeElement === sel) return;
    const prev = sel.value;
    const opts = [];
    for (const p of agentPeers()) {
      const o = document.createElement('option');
      o.value = p.agent_id;
      o.textContent = (p.name || p.agent_id.slice(0, 8) + '…') + (p.online ? '' : ' (offline)');
      opts.push(o);
    }
    const custom = document.createElement('option');
    custom.value = CUSTOM;
    custom.textContent = 'Other device (enter agent id)…';
    opts.push(custom);
    sel.replaceChildren(...opts);
    // Keep the operator's selection stable across the 2 s repaint.
    if (prev && [...sel.options].some((o) => o.value === prev)) sel.value = prev;
    $('tn-custom-node').hidden = sel.value !== CUSTOM;
  }

  function selectedNode() {
    const sel = $('tn-node');
    if (!sel || !sel.value) return '';
    if (sel.value === CUSTOM) return $('tn-custom-node').value.trim();
    return sel.value;
  }

  function wireForm() {
    const form = $('tn-form');
    if (!form) return;

    $('tn-node').addEventListener('change', () => {
      $('tn-custom-node').hidden = $('tn-node').value !== CUSTOM;
    });

    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (busy) return;
      const errSlot = $('tn-form-error');
      hide(errSlot);

      const node = selectedNode();
      if (!node) {
        errSlot.textContent = 'Pick a target device (or enter its agent id).';
        show(errSlot);
        return;
      }
      const local = parseInt($('tn-local').value, 10);
      const remoteRaw = $('tn-remote').value.trim();
      const transport = $('tn-transport').value;
      const id = $('tn-id').value.trim();

      const route = {
        id: id,
        kind: remoteRaw ? 'forward' : 'socks5',
        node: node,
        local: local,
        transport: transport,
        enabled: true,
      };
      if (remoteRaw) route.remote = remoteRaw;

      busy = true;
      try {
        await invoke('cmd_route_add', { route });
        form.reset();
        $('tn-custom-node').hidden = true;
      } catch (err) {
        errSlot.textContent = String(err);
        show(errSlot);
      } finally {
        busy = false;
        await refresh();
      }
    });
  }

  document.addEventListener('DOMContentLoaded', () => {
    wireForm();
    on('deviceView', paintNodeOptions);
    // Refresh immediately when the view is entered; poll only while visible.
    document.addEventListener('roomler:view', (ev) => {
      if (ev.detail === 'tunnels') void refresh();
    });
    void refresh();
    setInterval(refresh, 2000);
  });
})();
