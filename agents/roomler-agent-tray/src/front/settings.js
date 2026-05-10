/*
 * Settings page. Two flows:
 *   - Rename device (cmd_set_device_name)
 *   - Re-enrol with a new token (cmd_re_enroll)
 */
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {});

  function $(id) { return document.getElementById(id); }

  function showResult(html, isError) {
    const el = $('settings-result');
    el.hidden = false;
    el.className = isError ? 'banner banner-error' : 'banner banner-ok';
    el.innerHTML = html;
  }

  async function loadCurrentName() {
    try {
      const s = await invoke('cmd_status');
      if (s.device_name) $('rename-input').value = s.device_name;
    } catch (e) {
      console.warn('cmd_status failed', e);
    }
  }

  document.addEventListener('DOMContentLoaded', () => {
    void loadCurrentName();

    $('rename-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const name = $('rename-input').value.trim();
      if (!name) return;
      try {
        await invoke('cmd_set_device_name', { name });
        showResult(`Device name updated to <strong>${escape(name)}</strong>.`, false);
      } catch (e) {
        showResult(`Rename failed: ${escape(String(e))}`, true);
      }
    });

    $('re-enroll-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const token = $('re-token').value.trim();
      if (!token) return;
      try {
        await invoke('cmd_re_enroll', { token });
        showResult('Re-enrolment succeeded. Reload the Status page.', false);
        $('re-token').value = '';
      } catch (e) {
        showResult(`Re-enrolment failed: ${escape(String(e))}`, true);
      }
    });
  });

  function escape(s) {
    return s
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }
})();
