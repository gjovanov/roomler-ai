/*
 * Status page. Reads `cmd_status` from the Tauri backend, paints the
 * KV grid + banners, wires the action buttons.
 *
 * No bundler — Tauri 2 serves these files directly (frontendDist
 * points at this folder). Pure ES2020. `window.__TAURI__.core.invoke`
 * is exposed because tauri.conf.json has `withGlobalTauri: true`.
 */
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {});

  function $(id) { return document.getElementById(id); }
  function show(el) { el.hidden = false; }
  function hide(el) { el.hidden = true; }

  function setText(id, text) {
    const el = $(id);
    if (el) el.textContent = text;
  }

  async function refresh() {
    try {
      const s = await invoke('cmd_status');
      paint(s);
    } catch (err) {
      console.error('cmd_status failed', err);
    }
  }

  function paint(s) {
    hide($('status-loading'));
    show($('status-content'));

    if (s.enrolled) {
      show($('enrolled-banner'));
      hide($('not-enrolled-banner'));
      setText('banner-device-name', s.device_name || '');
    } else {
      hide($('enrolled-banner'));
      show($('not-enrolled-banner'));
    }

    if (s.attention) {
      show($('attention-banner'));
      setText('attention-path', s.attention);
    } else {
      hide($('attention-banner'));
    }

    setText('kv-version', s.agent_version);
    setText(
      'kv-service',
      `${s.service_kind === 'scmService' ? 'SCM Service' : s.service_kind === 'scheduledTask' ? 'Scheduled Task' : 'not installed'}${s.service_running ? ' (running)' : ''}`,
    );
    setText('kv-server', s.server_url || '(not enrolled)');
    setText('kv-agent-id', s.agent_id || '—');
    setText('kv-tenant-id', s.tenant_id || '—');
    setText('kv-schema', s.config_schema_version || 'legacy (pre-rc.18)');
    setText('kv-log-dir', s.log_dir);
  }

  function pushOutput(text) {
    const el = $('action-output');
    if (!el) return;
    el.textContent = text;
    show(el);
  }

  document.addEventListener('DOMContentLoaded', () => {
    void refresh();

    $('btn-check-update').addEventListener('click', async () => {
      pushOutput('Checking for updates…');
      try {
        const out = await invoke('cmd_check_update');
        pushOutput(out);
        if (out.toLowerCase().includes('update available')) {
          show($('btn-apply-update'));
        }
      } catch (e) {
        pushOutput(`Error: ${e}`);
      }
    });

    $('btn-apply-update').addEventListener('click', async () => {
      pushOutput('Spawning installer. The agent will exit briefly while the update applies.');
      try {
        await invoke('cmd_apply_update');
      } catch (e) {
        pushOutput(`Error: ${e}`);
      }
    });

    $('btn-open-logs').addEventListener('click', async () => {
      try {
        await invoke('cmd_open_log_dir');
      } catch (e) {
        pushOutput(`Could not open log dir: ${e}`);
      }
    });

    $('btn-open-config').addEventListener('click', async () => {
      try {
        await invoke('cmd_open_config_dir');
      } catch (e) {
        pushOutput(`Could not open config dir: ${e}`);
      }
    });

    $('btn-service-install').addEventListener('click', async () => {
      // false = Scheduled Task path (default). PerMachine fleet
      // installs (--as-service) should be triggered from the CLI
      // by IT; the tray's "install service" is the per-user path.
      try {
        await invoke('cmd_service_install', { asService: false });
        pushOutput('Service installed (Scheduled Task). Reloading…');
        setTimeout(refresh, 500);
      } catch (e) {
        pushOutput(`service install failed: ${e}`);
      }
    });

    // Tray's "Check for Updates" menu item dispatches this event.
    window.addEventListener('roomler-update-check', () => {
      const r = window.__roomler_check_update_result;
      if (r && r.check) {
        pushOutput(r.check);
        if (r.check.toLowerCase().includes('update available')) {
          show($('btn-apply-update'));
        }
      }
    });

    // Light periodic refresh so the service-running state stays
    // accurate when the operator triggers install/uninstall from a
    // shell while the tray is open.
    setInterval(refresh, 10000);
  });
})();
