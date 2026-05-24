// Roomler Tunnel Installer SPA driver.
//
// Single-page state machine over 5 steps:
// welcome → server → token → install → done.
// Each step shows/hides via the `hidden` attribute; the indicator
// at the top updates by adding `.active` / `.done` classes.
//
// All backend calls go through Tauri 2's `window.__TAURI__.core.invoke`
// — same shape the agent installer's app.js uses.

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = ['welcome', 'server', 'token', 'install', 'done'];
let currentStep = 'welcome';
let formState = {
  schemaVersion: 1,
  step: 'welcome',
  serverUrl: '',
  deviceName: '',
};
let token = '';            // pasted JWT — kept in JS memory ONLY,
                           // never sent through cmd_save_state.
let installInProgress = false;

document.addEventListener('DOMContentLoaded', async () => {
  await boot();
});

// ─── Boot ─────────────────────────────────────────────────────────

async function boot() {
  // Load any persisted state from a prior crash/kill.
  try {
    formState = await invoke('cmd_load_state');
  } catch (e) {
    console.warn('cmd_load_state failed', e);
  }

  // Resume on the saved step EXCEPT when it's install (we never
  // persist mid-install; token isn't in state, so we'd be stuck).
  // Saved step "install" or "done" → restart at welcome.
  if (formState.step === 'install' || formState.step === 'done') {
    formState.step = 'welcome';
  }
  currentStep = formState.step || 'welcome';

  // Pre-fill defaults BEFORE rendering so the input values are right.
  if (!formState.serverUrl) {
    try { formState.serverUrl = await invoke('cmd_default_server_url'); } catch {}
  }
  if (!formState.deviceName) {
    try { formState.deviceName = await invoke('cmd_default_device_name'); } catch {}
  }

  document.getElementById('server-url').value = formState.serverUrl;
  document.getElementById('device-name').value = formState.deviceName;

  wireEvents();
  await runDetection();
  showStep(currentStep);

  // Snackbar relay for single-instance "wizard already running" event.
  await listen('installer-already-running', () => {
    showSnackbar('Wizard is already running — finish the current install first.');
  });
}

// ─── Detection (Welcome step) ─────────────────────────────────────

async function runDetection() {
  const summary = document.getElementById('detection-summary');
  const reinstall = document.getElementById('detection-reinstall-warning');
  try {
    const detect = await invoke('cmd_detect_install');
    if (detect.kind === 'installed') {
      const machine = detect.machineName ? ` (machine: ${detect.machineName})` : '';
      summary.textContent = `Existing tunnel client found${machine} at ${detect.configPath}`;
      reinstall.hidden = false;
    } else {
      summary.textContent = 'No existing tunnel client found on this machine.';
      reinstall.hidden = true;
    }
  } catch (e) {
    summary.textContent = `Detection failed: ${e}`;
  }
}

// ─── Wiring ───────────────────────────────────────────────────────

function wireEvents() {
  // Welcome
  document.getElementById('welcome-cancel').addEventListener('click', exitWizard);
  document.getElementById('welcome-continue').addEventListener('click', () => goTo('server'));

  // Server
  document.getElementById('server-back').addEventListener('click', () => goTo('welcome'));
  document.getElementById('server-continue').addEventListener('click', () => {
    formState.serverUrl = document.getElementById('server-url').value.trim();
    formState.deviceName = document.getElementById('device-name').value.trim();
    persistState();
    goTo('token');
  });
  document.getElementById('server-url').addEventListener('input', refreshServerStep);
  document.getElementById('device-name').addEventListener('input', refreshServerStep);
  refreshServerStep();

  // Token
  document.getElementById('token-back').addEventListener('click', () => goTo('server'));
  document.getElementById('token-input').addEventListener('input', refreshTokenStep);
  document.getElementById('token-continue').addEventListener('click', () => {
    token = document.getElementById('token-input').value.trim();
    if (!token) return;
    goTo('install');
    startInstall();
  });

  // Install
  document.getElementById('install-cancel').addEventListener('click', cancelInstall);
  document.getElementById('install-retry').addEventListener('click', () => {
    document.getElementById('install-error').hidden = true;
    startInstall();
  });

  // Done
  document.getElementById('done-finish').addEventListener('click', exitWizard);
}

function refreshServerStep() {
  const url = document.getElementById('server-url').value.trim();
  const name = document.getElementById('device-name').value.trim();
  // Light validation — full URL parsing happens server-side. Block
  // continue only when fields are visibly empty / malformed.
  const looksLikeUrl = /^https?:\/\/[^\s]+$/i.test(url);
  document.getElementById('server-continue').disabled = !(looksLikeUrl && name.length > 0);
}

async function refreshTokenStep() {
  const value = document.getElementById('token-input').value.trim();
  const info = document.getElementById('token-info');
  const warnAudience = document.getElementById('token-warn-audience');
  const warnExpired = document.getElementById('token-warn-expired');
  const warnParse = document.getElementById('token-warn-parse');
  const cont = document.getElementById('token-continue');

  // Reset.
  info.hidden = true;
  warnAudience.hidden = true;
  warnExpired.hidden = true;
  warnParse.hidden = true;
  cont.disabled = true;
  if (!value) return;

  try {
    const view = await invoke('cmd_validate_token', { token: value });
    info.hidden = false;
    document.getElementById('token-issuer').textContent = view.issuer || '(none)';
    document.getElementById('token-audience').textContent = view.audience || '(none)';
    document.getElementById('token-expiry').textContent =
      view.expiresAtUnix ? formatExpiry(view.expiresAtUnix) : '(no exp)';

    if (!view.audienceMatches) {
      warnAudience.hidden = false;
      document.getElementById('token-warn-aud-name').textContent =
        view.audience || '(none)';
      return;
    }
    if (view.appearsExpired) {
      warnExpired.hidden = false;
      return;
    }
    cont.disabled = false;
  } catch (e) {
    warnParse.hidden = false;
    console.warn('token validation failed', e);
  }
}

function formatExpiry(unix) {
  const date = new Date(unix * 1000);
  const now = new Date();
  const minutes = Math.round((date - now) / 60000);
  const stamp = date.toLocaleString();
  if (minutes < 0) return `${stamp} (${-minutes} min ago)`;
  if (minutes < 60) return `${stamp} (in ${minutes} min)`;
  return stamp;
}

// ─── Install ──────────────────────────────────────────────────────

async function startInstall() {
  installInProgress = true;
  document.getElementById('install-cancel').disabled = false;

  const log = document.getElementById('install-log');
  log.innerHTML = '';
  setProgress(0);
  setCurrent('Starting…');

  const channel = new Channel();
  channel.onmessage = handleProgress;

  try {
    const done = await invoke('cmd_install', {
      server: formState.serverUrl,
      token,
      deviceName: formState.deviceName,
      onEvent: channel,
    });
    installInProgress = false;
    populateDone(done);
    goTo('done');
  } catch (e) {
    installInProgress = false;
    document.getElementById('install-error').hidden = false;
    document.getElementById('install-error-message').textContent = String(e);
  }
}

async function cancelInstall() {
  if (!installInProgress) return;
  try {
    await invoke('cmd_cancel_in_progress');
    appendLog('Cancel requested — finishing current step…', 'err');
  } catch (e) {
    appendLog(`Cancel failed: ${e}`, 'err');
  }
}

function handleProgress(event) {
  // ProgressEvent = {type: "...", data: {...}}
  const { type, data } = event;
  switch (type) {
    case 'Started':
      appendLog('Pipeline started');
      break;
    case 'PreflightStarted':
      setCurrent('Detecting existing install…');
      break;
    case 'PreflightOk':
      appendLog(`Preflight: ${data.existing}`, 'ok');
      break;
    case 'PreflightWarning':
      appendLog(`Warning: ${data.message}`);
      break;
    case 'AssetResolving':
      setCurrent(`Resolving installer for ${data.platform}…`);
      break;
    case 'AssetResolved':
      appendLog(`Resolved ${data.tag} (${formatBytes(data.sizeBytes)})`, 'ok');
      break;
    case 'DownloadStarted':
      setCurrent('Downloading CLI archive…');
      break;
    case 'DownloadProgress':
      setProgress((data.receivedBytes / lastTotalBytes) * 100);
      break;
    case 'DownloadVerified':
      appendLog(data.sha256Match ? 'SHA256 verified' : 'SHA256 not verified', data.sha256Match ? 'ok' : 'err');
      setProgress(100);
      break;
    case 'ExtractStarted':
      setCurrent('Extracting archive…');
      break;
    case 'ExtractDone':
      appendLog(`Extracted. Binary at ${data.tunnelBinary}`, 'ok');
      break;
    case 'IntegrationStarted':
      setCurrent('Wiring up PATH and shortcuts…');
      break;
    case 'IntegrationDone':
      appendLog(
        `Integration: PATH ${data.pathUpdated ? 'updated' : 'unchanged'}; shortcut ${data.shortcutCreated ? 'created' : 'skipped'}`,
        'ok'
      );
      break;
    case 'EnrollStarted':
      setCurrent('Enrolling tunnel client…');
      break;
    case 'EnrollOk':
      appendLog(`Enrolled (client ${data.tunnelClientId})`, 'ok');
      break;
    case 'Done':
      appendLog('Done', 'ok');
      break;
    case 'Error':
      appendLog(`Error in ${data.step}: ${data.message}`, 'err');
      break;
    default:
      appendLog(`(unknown event: ${type})`);
  }
  if (type === 'AssetResolved' || type === 'DownloadStarted') {
    lastTotalBytes = data.sizeBytes ?? data.totalBytes ?? lastTotalBytes;
  }
}

let lastTotalBytes = 1; // avoids div-by-zero before AssetResolved lands

function setProgress(pct) {
  const clamped = Math.max(0, Math.min(100, pct));
  document.getElementById('progress-bar').style.width = `${clamped}%`;
  document.getElementById('progress-summary').textContent = `${Math.round(clamped)}%`;
}

function setCurrent(s) {
  document.getElementById('install-current').textContent = s;
}

function appendLog(message, cls) {
  const log = document.getElementById('install-log');
  const li = document.createElement('li');
  li.textContent = message;
  if (cls) li.className = cls;
  log.appendChild(li);
  log.scrollTop = log.scrollHeight;
}

function formatBytes(bytes) {
  if (!bytes) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let i = 0;
  while (bytes >= 1024 && i < units.length - 1) {
    bytes /= 1024;
    i++;
  }
  return `${bytes.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

// ─── Done ─────────────────────────────────────────────────────────

function populateDone(done) {
  document.getElementById('done-client-id').textContent = done.tunnelClientId;
  document.getElementById('done-tenant-id').textContent = done.tenantId;
  document.getElementById('done-binary-path').textContent = done.binaryPath;
  document.getElementById('done-config-path').textContent = done.configPath;
  document.getElementById('done-tag').textContent = done.tag;
  if (done.pathUpdated) {
    document.getElementById('done-path-note').hidden = false;
  }
}

async function exitWizard() {
  try {
    await invoke('cmd_exit_wizard');
  } catch (e) {
    console.warn('exit wizard failed', e);
  }
}

// ─── Step navigation ──────────────────────────────────────────────

function goTo(step) {
  currentStep = step;
  formState.step = step;
  persistState();
  showStep(step);
}

function showStep(step) {
  STEPS.forEach(s => {
    document.getElementById(`step-${s}`).hidden = (s !== step);
  });
  document.querySelectorAll('.step-indicator li').forEach(li => {
    li.classList.remove('active', 'done');
  });
  const idx = STEPS.indexOf(step);
  STEPS.forEach((s, i) => {
    const li = document.querySelector(`.step-indicator li[data-step="${s}"]`);
    if (i < idx) li.classList.add('done');
    if (i === idx) li.classList.add('active');
  });
}

async function persistState() {
  try {
    await invoke('cmd_save_state', { state: formState });
  } catch (e) {
    console.warn('cmd_save_state failed', e);
  }
}

// ─── Snackbar ─────────────────────────────────────────────────────

function showSnackbar(text) {
  const el = document.getElementById('snackbar');
  el.textContent = text;
  el.hidden = false;
  setTimeout(() => { el.hidden = true; }, 4000);
}
