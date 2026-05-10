/*
 * Onboarding page. Three-step flow:
 *   1. Operator pastes enrollment token (from the Roomler admin UI).
 *   2. Operator confirms device name (defaulted to hostname).
 *   3. Click Enrol → calls `cmd_enroll` → on success offers
 *      to also install the service (Scheduled Task).
 */
(function () {
  const invoke = (name, payload) => window.__TAURI__.core.invoke(name, payload || {});

  function $(id) { return document.getElementById(id); }

  async function prefillDeviceName() {
    try {
      const name = await invoke('cmd_default_device_name');
      const el = $('device-name');
      if (el && !el.value) el.value = name;
    } catch (e) {
      console.warn('default device name lookup failed', e);
    }
  }

  function showResult(html, isError) {
    const el = $('enroll-result');
    el.hidden = false;
    el.innerHTML = html;
    el.className = isError ? 'banner banner-error' : 'banner banner-ok';
  }

  document.addEventListener('DOMContentLoaded', () => {
    void prefillDeviceName();

    $('enroll-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const server = $('server').value.trim();
      const deviceName = $('device-name').value.trim();
      const token = $('token').value.trim();
      if (!server || !deviceName || !token) {
        showResult('Fill all three fields.', true);
        return;
      }
      const btn = $('btn-enroll');
      btn.disabled = true;
      btn.textContent = 'Enrolling…';
      try {
        const s = await invoke('cmd_enroll', { server, token, deviceName });
        showResult(
          `<strong>Enrolled.</strong> agent_id: <code>${s.agent_id}</code> · ` +
            `tenant_id: <code>${s.tenant_id}</code>. ` +
            `<button type="button" id="btn-install-service" class="primary" style="margin-left:8px">Install Scheduled Task (auto-start)</button>`,
          false,
        );
        const installBtn = $('btn-install-service');
        if (installBtn) {
          installBtn.addEventListener('click', async () => {
            installBtn.disabled = true;
            installBtn.textContent = 'Installing…';
            try {
              await invoke('cmd_service_install', { asService: false });
              installBtn.textContent = 'Installed — close this window and the agent will run on next login.';
            } catch (e) {
              installBtn.textContent = `Install failed: ${e}`;
              installBtn.disabled = false;
            }
          });
        }
      } catch (e) {
        showResult(`Enrollment failed: ${e}`, true);
      } finally {
        btn.disabled = false;
        btn.textContent = 'Enrol this device';
      }
    });
  });
})();
