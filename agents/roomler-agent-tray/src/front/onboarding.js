/*
 * Onboarding view: enroll this device into a tenant.
 *   1. Operator pastes an enrollment token (from the Roomler admin UI).
 *   2. Operator confirms the device name (defaulted to the hostname).
 *   3. Enroll → `cmd_enroll` → on success offer per-user auto-start and a
 *      jump back to the Overview.
 *
 * Result content is built with createElement/textContent — the ids echoed
 * back come from the server and must not be innerHTML'd.
 */
(function () {
  'use strict';
  const { $, invoke, show, on, navigate, refreshStatus } = window.Roomler;

  async function prefillDeviceName() {
    try {
      const name = await invoke('cmd_default_device_name');
      const el = $('device-name');
      if (el && !el.value) el.value = name;
    } catch (e) {
      console.warn('default device name lookup failed', e);
    }
  }

  function resultSlot() {
    const el = $('enroll-result');
    el.hidden = false;
    el.replaceChildren();
    return el;
  }

  function showError(text) {
    const el = resultSlot();
    el.className = 'banner banner-error';
    el.textContent = text;
  }

  function showEnrolled(status) {
    const el = resultSlot();
    el.className = 'banner banner-ok';

    const box = document.createElement('div');
    const head = document.createElement('strong');
    head.textContent = 'Enrolled.';
    box.appendChild(head);

    const detail = document.createElement('p');
    detail.className = 'muted small';
    detail.style.margin = '4px 0 8px';
    detail.textContent =
      'device ' + (status.agent_id || '?') + ' · tenant ' + (status.tenant_id || '?');
    box.appendChild(detail);

    const actions = document.createElement('div');
    actions.className = 'actions';
    actions.style.marginTop = '0';

    const install = document.createElement('button');
    install.type = 'button';
    install.className = 'primary';
    install.textContent = 'Install auto-start';
    install.addEventListener('click', async () => {
      install.disabled = true;
      install.textContent = 'Installing…';
      try {
        await invoke('cmd_service_install', { asService: false });
        install.textContent = 'Auto-start installed';
      } catch (e) {
        install.textContent = 'Install failed: ' + e;
        install.disabled = false;
      }
    });
    actions.appendChild(install);

    const done = document.createElement('button');
    done.type = 'button';
    done.textContent = 'Go to Overview';
    done.addEventListener('click', () => navigate('overview'));
    actions.appendChild(done);

    box.appendChild(actions);
    el.appendChild(box);
  }

  document.addEventListener('DOMContentLoaded', () => {
    void prefillDeviceName();

    // An already-enrolled device rarely wants this view — hint via the label.
    on('status', (s) => {
      const btn = $('btn-enroll');
      if (btn) btn.textContent = s.enrolled ? 'Re-enroll this device' : 'Enroll this device';
    });

    $('enroll-form').addEventListener('submit', async (ev) => {
      ev.preventDefault();
      const server = $('server').value.trim();
      const deviceName = $('device-name').value.trim();
      const token = $('token').value.trim();
      if (!server || !deviceName || !token) {
        showError('Fill all three fields.');
        return;
      }
      const btn = $('btn-enroll');
      btn.disabled = true;
      const label = btn.textContent;
      btn.textContent = 'Enrolling…';
      try {
        const s = await invoke('cmd_enroll', { server, token, deviceName });
        $('token').value = '';
        showEnrolled(s);
        void refreshStatus();
      } catch (e) {
        showError('Enrollment failed: ' + e);
      } finally {
        btn.disabled = false;
        btn.textContent = label;
      }
    });
  });
})();
