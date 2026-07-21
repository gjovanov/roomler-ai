/*
 * Overview view: this device's identity + health at a glance, the exit-node
 * card (P5), and the update check/apply flow.
 *
 * Renders from the central store (app.js polls `cmd_status` + `cmd_device_view`);
 * no polling of its own. All dynamic strings land via textContent.
 */
(function () {
  'use strict';
  const { $, invoke, show, hide, setText, on, get, navigate } = window.Roomler;

  let routesActive = null; // painted by tunnels.js via the shared event below

  function paintStatus(s) {
    // Banners.
    if (s.enrolled) hide($('ov-not-enrolled'));
    else show($('ov-not-enrolled'));
    if (s.attention) {
      show($('ov-attention'));
      setText('ov-attention-path', s.attention);
    } else {
      hide($('ov-attention'));
    }

    const chip = $('ov-enrolled-chip');
    if (chip) {
      chip.textContent = s.enrolled ? 'Enrolled' : 'Not enrolled';
      chip.className = 'chip ' + (s.enrolled ? 'chip-ok' : 'chip-warn');
    }

    setText('ov-server', s.server_url || '—');
    setText('ov-app-version', s.agent_version);
    setText(
      'ov-service',
      s.service_kind === 'scmService'
        ? 'System service' + (s.service_running ? ' · running' : ' · stopped')
        : s.service_kind === 'scheduledTask'
          ? 'Per-user auto-start' + (s.service_running ? ' · running' : '')
          : 'not installed',
    );
    // Config-file identity is a fallback for the fields the live daemon
    // fills in below when it's reachable.
    if (!get('deviceView') || !get('deviceView').available) {
      setText('ov-name', s.device_name || '—');
    }
  }

  function paintDeviceView(dv) {
    if (!dv.available) {
      setText('ov-link', 'service offline');
      setText('ov-daemon-version', '—');
      setText('ov-ip', '—');
      paintCounts(dv);
      return;
    }
    const st = dv.status || {};
    setText('ov-name', st.name || '—');
    setText('ov-daemon-version', st.version || '—');
    setText('ov-link', st.connected ? 'connected' : 'disconnected');
    setText(
      'ov-ip',
      st.overlay_ip
        ? st.overlay_ip6
          ? st.overlay_ip + ' · ' + st.overlay_ip6
          : st.overlay_ip
        : '(no overlay IP)',
    );
    paintExit(st.exit_node);
    paintCounts(dv);
  }

  function paintCounts(dv) {
    const peers = (dv && dv.peers) || [];
    const online = peers.filter((p) => p.online).length;
    let text = online + ' of ' + peers.length + ' devices online';
    if (routesActive !== null) text += ' · ' + routesActive + ' routes active';
    setText('ov-counts', peers.length ? text : '—');
  }

  // P5 exit-node status: only rendered when this node is configured as an
  // exit-node client; the withheld_reason / v6 / DNS caveats surface the
  // fail-closed states so a "why is my traffic not moving" never needs logs.
  function paintExit(exit) {
    const card = $('ov-exit-card');
    if (!exit) { hide(card); return; }
    show(card);
    setText('ov-exit-selector', exit.selector || '—');
    const chip = $('ov-exit-chip');
    if (chip) {
      chip.textContent = exit.active ? 'Active' : 'Withheld';
      chip.className = 'chip ' + (exit.active ? 'chip-ok' : 'chip-warn');
    }
    let detail;
    if (exit.active) {
      const caveats = [];
      if (!exit.v6_active) caveats.push('IPv6 blackholed (exit is v4-only)');
      if (!exit.dns_steered) caveats.push('DNS not steered — queries may leak locally');
      detail = caveats.length ? 'routing active · ' + caveats.join(' · ') : 'all traffic routes via the exit node';
    } else {
      detail = exit.withheld_reason || 'not active';
    }
    setText('ov-exit-status', detail);
  }

  function pushOutput(text) {
    const el = $('action-output');
    if (!el) return;
    el.textContent = text;
    show(el);
  }

  document.addEventListener('DOMContentLoaded', () => {
    on('status', paintStatus);
    on('deviceView', paintDeviceView);

    $('ov-btn-onboard').addEventListener('click', () => navigate('onboarding'));

    $('btn-check-update').addEventListener('click', async () => {
      pushOutput('Checking for updates…');
      try {
        const out = await invoke('cmd_check_update');
        pushOutput(out);
        if (out.toLowerCase().includes('update available')) {
          show($('btn-apply-update'));
        }
      } catch (e) {
        pushOutput('Error: ' + e);
      }
    });

    $('btn-apply-update').addEventListener('click', async () => {
      pushOutput('Spawning installer. The service will restart briefly while the update applies.');
      try {
        await invoke('cmd_apply_update');
      } catch (e) {
        pushOutput('Error: ' + e);
      }
    });

    // Tray's "Check for Updates" menu item evals a global + dispatches this
    // event (tray.rs) — surface the result where it's visible.
    window.addEventListener('roomler-update-check', () => {
      const r = window.__roomler_check_update_result;
      if (r && r.check) {
        navigate('overview');
        pushOutput(r.check);
        if (r.check.toLowerCase().includes('update available')) {
          show($('btn-apply-update'));
        }
      }
    });

    // tunnels.js broadcasts its route list; the overview only needs the count.
    document.addEventListener('roomler:routes', (ev) => {
      const routes = ev.detail || [];
      routesActive = routes.filter(
        (r) => r.state && r.state.state === 'active',
      ).length;
      paintCounts(get('deviceView'));
    });
  });
})();
