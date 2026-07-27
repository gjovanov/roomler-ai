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

  /* ── S2: device configuration editor ──────────────────────────────
   * Rows come from cmd_config_entries (the daemon's editable surface);
   * each row's editor is keyed on the entry's `kind` contract:
   *   bool / tribool / enum:<a|b|c> → a <select>, applied on change;
   *   string / list / json          → text input or textarea + Save.
   * Everything is built with createElement + textContent (no innerHTML).
   */
  let cfgLoaded = false;

  function cfgRowStatus(row) {
    return row.querySelector('.cfg-row-status');
  }

  async function applyCfg(key, raw, row) {
    // Empty text/selection = clear the key back to its default. The
    // bool/enum selects never produce '' — only tribool/text kinds do.
    const value = raw === '' ? null : raw;
    const status = cfgRowStatus(row);
    try {
      const entry = await invoke('cmd_config_set', { key, value });
      status.textContent = 'Saved — takes effect after the service restarts.';
      status.classList.remove('error');
      return entry;
    } catch (e) {
      status.textContent = String(e);
      status.classList.add('error');
      return null;
    }
  }

  function makeSelect(options, current) {
    const sel = document.createElement('select');
    for (const opt of options) {
      const o = document.createElement('option');
      o.value = opt.value;
      o.textContent = opt.label;
      sel.appendChild(o);
    }
    sel.value = current;
    return sel;
  }

  function cfgRow(entry) {
    const row = document.createElement('div');
    row.className = 'cfg-row';
    row.style.margin = '10px 0 0';

    const head = document.createElement('div');
    const key = document.createElement('span');
    key.className = 'mono small';
    key.textContent = entry.key;
    head.appendChild(key);
    row.appendChild(head);

    const desc = document.createElement('p');
    desc.className = 'muted small';
    desc.style.margin = '2px 0 4px';
    desc.textContent = entry.description;
    row.appendChild(desc);

    const controls = document.createElement('div');
    controls.className = 'actions';
    const val = entry.value == null ? '' : entry.value;

    if (entry.kind === 'bool') {
      const sel = makeSelect(
        [
          { value: 'true', label: 'on' },
          { value: 'false', label: 'off' },
        ],
        val || 'false',
      );
      sel.addEventListener('change', () => void applyCfg(entry.key, sel.value, row));
      controls.appendChild(sel);
    } else if (entry.kind === 'tribool') {
      const sel = makeSelect(
        [
          { value: '', label: 'default' },
          { value: 'true', label: 'on' },
          { value: 'false', label: 'off' },
        ],
        val,
      );
      sel.addEventListener('change', () => void applyCfg(entry.key, sel.value, row));
      controls.appendChild(sel);
    } else if (entry.kind.startsWith('enum:')) {
      const opts = entry.kind
        .slice('enum:'.length)
        .split('|')
        .map((v) => ({ value: v, label: v }));
      const sel = makeSelect(opts, val || opts[0].value);
      sel.addEventListener('change', () => void applyCfg(entry.key, sel.value, row));
      controls.appendChild(sel);
    } else if (entry.kind === 'json') {
      const ta = document.createElement('textarea');
      ta.rows = 4;
      ta.spellcheck = false;
      ta.className = 'mono small';
      ta.style.width = '100%';
      try {
        ta.value = val ? JSON.stringify(JSON.parse(val), null, 2) : '';
      } catch {
        ta.value = val;
      }
      const save = document.createElement('button');
      save.type = 'button';
      save.textContent = 'Save';
      save.addEventListener('click', async () => {
        save.disabled = true;
        const entryBack = await applyCfg(entry.key, ta.value.trim(), row);
        if (entryBack && entryBack.value) {
          try { ta.value = JSON.stringify(JSON.parse(entryBack.value), null, 2); } catch {}
        }
        save.disabled = false;
      });
      const wrap = document.createElement('div');
      wrap.style.width = '100%';
      wrap.appendChild(ta);
      controls.appendChild(wrap);
      controls.appendChild(save);
    } else {
      // string / list — one-line text input + Save.
      const input = document.createElement('input');
      input.type = 'text';
      input.spellcheck = false;
      input.className = 'mono small';
      input.value = val;
      input.style.flex = '1';
      if (entry.kind === 'list') input.placeholder = 'e.g. 192.168.1.0/24, 10.0.0.0/8';
      const save = document.createElement('button');
      save.type = 'button';
      save.textContent = 'Save';
      const submit = async () => {
        save.disabled = true;
        const entryBack = await applyCfg(entry.key, input.value.trim(), row);
        if (entryBack) input.value = entryBack.value == null ? '' : entryBack.value;
        save.disabled = false;
      };
      save.addEventListener('click', submit);
      input.addEventListener('keydown', (ev) => {
        if (ev.key === 'Enter') { ev.preventDefault(); void submit(); }
      });
      controls.appendChild(input);
      controls.appendChild(save);
    }
    row.appendChild(controls);

    const status = document.createElement('p');
    status.className = 'muted small cfg-row-status';
    status.style.margin = '2px 0 0';
    row.appendChild(status);
    return row;
  }

  async function loadCfg(force) {
    if (cfgLoaded && !force) return;
    const container = $('cfg-rows');
    if (!container) return;
    try {
      const entries = await invoke('cmd_config_entries');
      container.replaceChildren();
      for (const entry of entries) container.appendChild(cfgRow(entry));
      cfgLoaded = true;
    } catch (e) {
      container.replaceChildren();
      const p = document.createElement('p');
      p.className = 'muted small';
      p.textContent = 'Could not load the configuration: ' + e;
      const retry = document.createElement('button');
      retry.type = 'button';
      retry.textContent = 'Retry';
      retry.addEventListener('click', () => void loadCfg(true));
      container.appendChild(p);
      container.appendChild(retry);
    }
  }

  /* ── S2: log viewer — bounded TailLog polls, client-side filter ──── */
  let logTimer = null;

  async function refreshLog() {
    const sourceEl = $('log-source');
    const view = $('log-view');
    if (!sourceEl || !view) return;
    try {
      const r = await invoke('cmd_tail_log', { source: sourceEl.value, maxBytes: 32768 });
      setText('log-path', r.path + ' · ' + r.size + ' bytes');
      const filter = ($('log-filter').value || '').trim().toLowerCase();
      const lines = r.content.split('\n');
      const shown = filter
        ? lines.filter((l) => l.toLowerCase().includes(filter))
        : lines;
      // Keep the view pinned to the tail unless the operator scrolled up.
      const atBottom = view.scrollHeight - view.scrollTop - view.clientHeight < 32;
      view.textContent = shown.join('\n').trim() || '(no matching lines)';
      if (atBottom) view.scrollTop = view.scrollHeight;
    } catch (e) {
      setText('log-path', '—');
      view.textContent = 'Could not read the log: ' + e;
    }
  }

  function setLogFollow(onOff) {
    if (logTimer) { clearInterval(logTimer); logTimer = null; }
    if (onOff) logTimer = setInterval(() => void refreshLog(), 3000);
  }

  document.addEventListener('roomler:view', (ev) => {
    if (ev.detail === 'settings') {
      void loadCfg(false);
    } else {
      // Leaving Settings pauses a running follow (checkbox state stays).
      setLogFollow(false);
      const follow = $('log-follow');
      if (follow) follow.checked = false;
    }
  });

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

    // S2 — log viewer wiring.
    $('btn-log-refresh').addEventListener('click', () => void refreshLog());
    $('log-source').addEventListener('change', () => void refreshLog());
    $('log-filter').addEventListener('change', () => void refreshLog());
    $('log-follow').addEventListener('change', (ev) => {
      setLogFollow(ev.target.checked);
      if (ev.target.checked) void refreshLog();
    });
  });
})();
