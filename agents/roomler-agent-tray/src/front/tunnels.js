/*
 * Tunnels view (P6). Polls `cmd_route_list` and paints the declared,
 * daemon-supervised routes with their live state; the Add-route form +
 * per-row Enable/Disable/Remove buttons call the matching cmd_route_*
 * passthroughs.
 *
 * Conventions match devices.js: no bundler, pure ES2020,
 * `window.__TAURI__.core.invoke` via `withGlobalTauri`, and every
 * daemon-supplied string (ids, error reasons) is written with
 * textContent only — never innerHTML.
 */
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {});
  function $(id) { return document.getElementById(id); }
  function show(el) { if (el) el.hidden = false; }
  function hide(el) { if (el) el.hidden = true; }

  let busy = false; // one mutation in flight at a time

  async function refresh() {
    let routes;
    try {
      routes = await invoke('cmd_route_list');
    } catch (err) {
      // cmd_route_list is never-fail by contract.
      console.error('cmd_route_list failed', err);
      return;
    }
    paint(routes);
  }

  // Compact human word for a RouteState (adjacently tagged on `state`).
  function stateLabel(s) {
    switch (s.state) {
      case 'disabled': return { text: 'disabled', cls: 'muted' };
      case 'pending': return { text: 'pending', cls: 'muted' };
      case 'active': return { text: 'active (' + s.flow_id + ')', cls: 'ok' };
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

  function actionBtn(label, onClick) {
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    b.className = 'small';
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

  function paint(routes) {
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
      tr.appendChild(td(d.node.slice(0, 8) + '…', 'mono small'));
      const st = stateLabel(r.state);
      tr.appendChild(td(st.text, st.cls));

      const actions = document.createElement('td');
      if (d.enabled) {
        actions.appendChild(actionBtn('Disable', () =>
          invoke('cmd_route_set_enabled', { id: d.id, enabled: false })));
      } else {
        actions.appendChild(actionBtn('Enable', () =>
          invoke('cmd_route_set_enabled', { id: d.id, enabled: true })));
      }
      actions.appendChild(actionBtn('Remove', () =>
        invoke('cmd_route_remove', { id: d.id })));
      tr.appendChild(actions);
      return tr;
    });
    body.replaceChildren(...rows);
  }

  function wireForm() {
    const form = $('tn-form');
    if (!form) return;
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (busy) return;
      const errSlot = $('tn-form-error');
      hide(errSlot);

      const agent = $('tn-agent').value.trim();
      const local = parseInt($('tn-local').value, 10);
      const remoteRaw = $('tn-remote').value.trim();
      const transport = $('tn-transport').value;
      const id = $('tn-id').value.trim();

      const route = {
        id: id,
        kind: remoteRaw ? 'forward' : 'socks5',
        node: agent,
        local: local,
        transport: transport,
        enabled: true,
      };
      if (remoteRaw) route.remote = remoteRaw;

      busy = true;
      try {
        await invoke('cmd_route_add', { route });
        form.reset();
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
    refresh();
    // Same cadence as the devices view — the state column tracks the
    // reconciler's backoff/active transitions closely enough at 2 s.
    setInterval(refresh, 2000);
  });
})();
