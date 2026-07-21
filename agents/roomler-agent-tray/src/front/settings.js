/*
 * Settings view: device rename, re-enrollment, background-service management
 * and file locations. Status data comes from the central store; mutations go
 * through the matching cmd_* commands. Results are rendered with textContent
 * (no innerHTML) into the shared banner slot.
 */
(function () {
  'use strict';
  const { $, invoke, show, hide, setText, on, refreshStatus } = window.Roomler;

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

    $('re-enroll-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const token = $('re-token').value.trim();
      if (!token) return;
      try {
        await invoke('cmd_re_enroll', { token });
        showResult('Re-enrollment succeeded.', false);
        $('re-token').value = '';
        void refreshStatus();
      } catch (e) {
        showResult('Re-enrollment failed: ' + e, true);
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
