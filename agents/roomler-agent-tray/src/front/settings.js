/*
 * Settings view: device rename, background-service management, split-config
 * cleanup and file locations (re-enrollment moved to Onboarding in S1b).
 * Status data comes from the central store; mutations go through the
 * matching cmd_* commands. Results are rendered with textContent (no
 * innerHTML) into the shared banner slot.
 */
(function () {
  'use strict';
  const { $, invoke, show, hide, setText, on, navigate, refreshStatus } = window.Roomler;

  function showResult(text, isError) {
    const el = $('settings-result');
    el.hidden = false;
    el.textContent = text;
    el.className = 'banner ' + (isError ? 'banner-error' : 'banner-ok');
  }

  function paintStatus(s) {
    const rename = $('rename-input');
    if (rename && !rename.matches(':focus') && s.device_name && !rename.value) {
      rename.value = s.device_name;
    }
    setText(
      'st-service',
      s.service_kind === 'scmService'
        ? 'System service' + (s.service_running ? ' · running' : ' · stopped')
        : s.service_kind === 'scheduledTask'
          ? 'Per-user auto-start' + (s.service_running ? ' · running' : '')
          : 'not installed',
    );
    setText('st-log-dir', s.log_dir);
    setText('st-config-dir', s.config_dir);
    if (s.config_split) show($('st-split-banner'));
    else hide($('st-split-banner'));
    // S1b: when the daemon is reachable it reports the config file it
    // actually LOADED — authoritative over the flavour-guess above.
    const dv = window.Roomler.get && window.Roomler.get('deviceView');
    if (dv && dv.available && dv.status && dv.status.config_path) {
      setText('st-config-dir', dv.status.config_path);
    }
    // Rename/re-enroll write the machine-wide config under an SCM install,
    // which an unelevated desktop app can't do — say so up front instead of
    // only failing on submit.
    const isScm = s.service_kind === 'scmService';
    document.querySelectorAll('.scm-hint').forEach((el) => { el.hidden = !isScm; });
  }

  document.addEventListener('DOMContentLoaded', () => {
    on('status', paintStatus);

    $('rename-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const name = $('rename-input').value.trim();
      if (!name) return;
      try {
        await invoke('cmd_set_device_name', { name });
        showResult('Device name updated to “' + name + '”.', false);
        void refreshStatus();
      } catch (e) {
        showResult('Rename failed: ' + e, true);
      }
    });

    // S1b: enrollment consolidated into Onboarding — this card just links.
    $('btn-goto-onboarding').addEventListener('click', () => navigate('onboarding'));

    // S1b: the split-config banner's cleanup action — the DAEMON archives
    // the stale copy (identity-guarded, never deletes).
    $('btn-config-cleanup').addEventListener('click', async () => {
      const btn = $('btn-config-cleanup');
      btn.disabled = true;
      try {
        const detail = await invoke('cmd_config_cleanup');
        showResult('Cleaned up: ' + detail, false);
        void refreshStatus();
      } catch (e) {
        showResult('Cleanup not performed: ' + e, true);
      } finally {
        btn.disabled = false;
      }
    });

    $('btn-service-install').addEventListener('click', async () => {
      // false = per-user auto-start (Scheduled Task on Windows). Machine-wide
      // SCM installs are the Roomler Setup installer's job, not the desktop's.
      try {
        await invoke('cmd_service_install', { asService: false });
        showResult('Auto-start installed.', false);
        void refreshStatus();
      } catch (e) {
        showResult('Install failed: ' + e, true);
      }
    });

    $('btn-service-uninstall').addEventListener('click', async () => {
      try {
        await invoke('cmd_service_uninstall', { asService: false });
        showResult('Auto-start removed.', false);
        void refreshStatus();
      } catch (e) {
        showResult('Removal failed: ' + e, true);
      }
    });

    $('btn-open-logs').addEventListener('click', async () => {
      try {
        await invoke('cmd_open_log_dir');
      } catch (e) {
        showResult('Could not open the logs folder: ' + e, true);
      }
    });

    $('btn-open-config').addEventListener('click', async () => {
      try {
        await invoke('cmd_open_config_dir');
      } catch (e) {
        showResult('Could not open the config folder: ' + e, true);
      }
    });
  });
})();
