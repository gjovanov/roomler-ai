// Roomler Agent Installer — SPA controller.
//
// W8 in the rc.28 plan. Vanilla JS state machine that drives the
// 5-step wizard via Tauri invoke calls into the `wizard_core` lib.
//
// State shape (single source of truth):
//   - `state.step`: which wizard step is currently visible
//   - `state.detection`: result of cmd_detect_install (null until probe)
//   - `state.flavour` + `state.flavourAck`: deployment choice + cross-
//     flavour ack checkbox (gates Continue on Welcome step)
//   - `state.server` + `state.device`: Step 2 form fields
//   - `state.token` + `state.tokenView`: Step 3 form input + cmd_validate_token output
//   - `state.installProgress`: live ProgressEvent stream rendered as a checklist
//   - `state.installDone` / `state.installError`: terminal outcomes for Step 4
//
// Cross-cutting:
//   - Wizard state is persisted to disk via cmd_save_state on every
//     form blur, so a force-killed wizard resumes mid-flow.
//   - cmd_install streams ProgressEvents via tauri::ipc::Channel; the
//     listener is attached BEFORE the invoke so no events are dropped.
//   - "installer-already-running" event from the single-instance plugin
//     (B9) surfaces a snackbar when a second wizard EXE tries to launch
//     during an in-flight install.

const invoke = window.__TAURI__.core.invoke;
const Channel = window.__TAURI__.core.Channel;
const listen = window.__TAURI__.event.listen;

const STEPS = ["welcome", "server", "token", "install", "done"];

const state = {
  step: "welcome",
  detection: null,
  flavour: null,
  flavourAck: false,
  server: "",
  device: "",
  token: "",
  tokenView: null,
  tokenValidating: false,
  installProgress: [],
  installError: null,
  installDone: null,
  installInFlight: false,
};

// ─── Bootstrap ─────────────────────────────────────────────────────────────

async function init() {
  wireGlobalListeners();
  await loadPersistedState();
  await probeAndRender();
}

async function loadPersistedState() {
  try {
    const persisted = await invoke("cmd_load_state");
    state.flavour = persisted.flavour ?? null;
    state.server = persisted.server_url ?? "";
    state.device = persisted.device_name ?? "";
    // Step "install" or "done" on relaunch means previous install was
    // either interrupted or successful; either way, dump the operator
    // back at Welcome so they can re-detect and decide.
    if (persisted.step === "install" || persisted.step === "done") {
      state.step = "welcome";
    } else {
      state.step = persisted.step ?? "welcome";
    }
  } catch (err) {
    console.warn("load wizard state failed:", err);
  }
}

async function persistState() {
  try {
    await invoke("cmd_save_state", {
      state: {
        version: 1,
        step: state.step,
        flavour: state.flavour,
        server_url: state.server,
        device_name: state.device,
      },
    });
  } catch (err) {
    console.warn("save wizard state failed:", err);
  }
}

async function probeAndRender() {
  // Welcome step's pre-flight detection runs once on bootstrap.
  try {
    state.detection = await invoke("cmd_detect_install");
  } catch (err) {
    state.detection = { kind: "clean", peruser_version: null, permachine_version: null, ambiguous: false };
    showSnackbar(`Detection failed: ${err}`);
  }

  // Default server URL + device name come from the lib helpers when
  // the persisted state didn't already supply values.
  if (!state.server) {
    state.server = await invoke("cmd_default_server_url");
  }
  if (!state.device) {
    state.device = await invoke("cmd_default_device_name");
  }

  render();
}

function wireGlobalListeners() {
  // B9: single-instance plugin emits this when a second wizard EXE
  // launch finds an in-flight install in the first process. Pop a
  // snackbar; the new process exits silently on the Rust side.
  listen("installer-already-running", () => {
    showSnackbar("Wizard is already running — focused the existing window.");
  });

  // Recovery panel toggle (cog icon top-right).
  document.getElementById("recovery-toggle").addEventListener("click", () => {
    document.getElementById("recovery-panel").hidden = false;
  });
  document.getElementById("recovery-close").addEventListener("click", () => {
    document.getElementById("recovery-panel").hidden = true;
  });
  document.getElementById("recovery-reset").addEventListener("click", async () => {
    document.getElementById("recovery-panel").hidden = true;
    Object.assign(state, {
      step: "welcome",
      flavour: null,
      flavourAck: false,
      installProgress: [],
      installError: null,
      installDone: null,
    });
    await persistState();
    render();
  });

  // Per-step wiring (idempotent — listeners attached once at startup).
  wireWelcome();
  wireServer();
  wireToken();
  wireInstall();
  wireDone();
}

// ─── Render dispatch ───────────────────────────────────────────────────────

function render() {
  for (const step of STEPS) {
    document.getElementById(`step-${step}`).hidden = state.step !== step;
  }
  renderStepIndicator();
  if (state.step === "welcome") renderWelcome();
  if (state.step === "server") renderServer();
  if (state.step === "token") renderToken();
  if (state.step === "install") renderInstall();
  if (state.step === "done") renderDone();
}

function renderStepIndicator() {
  const items = document.querySelectorAll(".step-indicator li");
  const idx = STEPS.indexOf(state.step);
  items.forEach((li, i) => {
    li.classList.remove("active", "done");
    if (i < idx) li.classList.add("done");
    else if (i === idx) li.classList.add("active");
  });
}

async function gotoStep(name) {
  state.step = name;
  await persistState();
  render();
}

// ─── Step 1: Welcome ────────────────────────────────────────────────────────

function wireWelcome() {
  document.getElementById("welcome-cancel").addEventListener("click", () => {
    window.close();
  });
  document.getElementById("welcome-continue").addEventListener("click", async () => {
    await gotoStep("server");
  });
  document.querySelectorAll("input[name='flavour']").forEach((radio) => {
    radio.addEventListener("change", (e) => {
      state.flavour = e.target.value;
      state.flavourAck = false;
      document.getElementById("cross-flavour-ack").checked = false;
      persistState();
      renderWelcome();
    });
  });
  document.getElementById("cross-flavour-ack").addEventListener("change", (e) => {
    state.flavourAck = e.target.checked;
    renderWelcome();
  });
}

function renderWelcome() {
  // Detection summary.
  const summary = document.getElementById("detection-summary");
  const ambiguous = document.getElementById("detection-ambiguous-warning");
  if (!state.detection) {
    summary.textContent = "Probing…";
    return;
  }
  const d = state.detection;
  if (d.kind === "clean") {
    summary.textContent = "No existing install detected. Fresh setup.";
  } else if (d.kind === "peruser") {
    summary.textContent = `Existing perUser install detected (version ${d.peruser_version ?? "unknown"}).`;
  } else if (d.kind === "permachine") {
    summary.textContent = `Existing perMachine install detected (version ${d.permachine_version ?? "unknown"}).`;
  } else if (d.kind === "ambiguous") {
    summary.textContent = `Both flavours installed (perUser ${d.peruser_version ?? "?"} + perMachine ${d.permachine_version ?? "?"}).`;
  }
  ambiguous.hidden = d.kind !== "ambiguous";

  // Restore radio button selection.
  if (state.flavour) {
    const radio = document.querySelector(`input[name='flavour'][value='${state.flavour}']`);
    if (radio) radio.checked = true;
  }

  // Cross-flavour warning gate.
  const warningEl = document.getElementById("cross-flavour-warning");
  const warningText = document.getElementById("cross-flavour-warning-text");
  const warning = crossFlavourWarning(d, state.flavour);
  if (warning) {
    warningText.textContent = warning;
    warningEl.hidden = false;
  } else {
    warningEl.hidden = true;
  }

  // Continue button gate.
  const canContinue = state.flavour !== null
    && (warning === null || state.flavourAck);
  document.getElementById("welcome-continue").disabled = !canContinue;
}

function crossFlavourWarning(detection, flavour) {
  if (!flavour) return null;
  const wantsPerMachine = flavour.startsWith("permachine");
  const wantsPerUser = flavour === "peruser";
  if (detection.kind === "peruser" && wantsPerMachine) {
    return "Switching from perUser → perMachine. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator.";
  }
  if (detection.kind === "permachine" && wantsPerUser) {
    return "Switching from perMachine → perUser. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator.";
  }
  if (detection.kind === "ambiguous") {
    return "Both perUser and perMachine installs detected. The MSI's cleanup custom action will remove the one not selected.";
  }
  return null;
}

// ─── Step 2: Server URL + device name ──────────────────────────────────────

function wireServer() {
  document.getElementById("server-back").addEventListener("click", async () => {
    await gotoStep("welcome");
  });
  document.getElementById("server-continue").addEventListener("click", async () => {
    state.server = document.getElementById("server-url").value.trim();
    state.device = document.getElementById("device-name").value.trim();
    await persistState();
    await gotoStep("token");
  });
  document.getElementById("server-url").addEventListener("input", () => {
    state.server = document.getElementById("server-url").value.trim();
    renderServer();
  });
  document.getElementById("server-url").addEventListener("blur", persistState);
  document.getElementById("device-name").addEventListener("input", () => {
    state.device = document.getElementById("device-name").value.trim();
    renderServer();
  });
  document.getElementById("device-name").addEventListener("blur", persistState);
}

function renderServer() {
  document.getElementById("server-url").value = state.server;
  document.getElementById("device-name").value = state.device;
  const valid = state.server.match(/^https?:\/\/.+/) && state.device.length > 0;
  document.getElementById("server-continue").disabled = !valid;
}

// ─── Step 3: Enrollment token ──────────────────────────────────────────────

function wireToken() {
  document.getElementById("token-back").addEventListener("click", async () => {
    state.token = "";
    state.tokenView = null;
    document.getElementById("token-input").value = "";
    await gotoStep("server");
  });
  document.getElementById("token-continue").addEventListener("click", async () => {
    await runInstall();
  });
  let debounce;
  document.getElementById("token-input").addEventListener("input", (e) => {
    state.token = e.target.value;
    clearTimeout(debounce);
    debounce = setTimeout(validateToken, 350);
  });
}

async function validateToken() {
  const t = state.token.trim();
  if (!t) {
    state.tokenView = null;
    renderToken();
    return;
  }
  state.tokenValidating = true;
  renderToken();
  try {
    state.tokenView = await invoke("cmd_validate_token", { token: t });
    state.tokenView.error = null;
  } catch (err) {
    state.tokenView = { error: String(err) };
  } finally {
    state.tokenValidating = false;
    renderToken();
  }
}

function renderToken() {
  const validationEl = document.getElementById("token-validation");
  const continueBtn = document.getElementById("token-continue");

  if (state.tokenValidating) {
    validationEl.hidden = false;
    validationEl.className = "token-validation muted";
    validationEl.textContent = "Validating…";
    continueBtn.disabled = true;
    return;
  }
  if (!state.tokenView) {
    validationEl.hidden = true;
    continueBtn.disabled = true;
    return;
  }
  validationEl.hidden = false;
  if (state.tokenView.error) {
    validationEl.className = "token-validation invalid";
    validationEl.textContent = `Token parse failed: ${state.tokenView.error}`;
    continueBtn.disabled = true;
    return;
  }
  if (state.tokenView.appears_expired) {
    validationEl.className = "token-validation invalid";
    validationEl.textContent = "Token has expired. Generate a fresh one in the Roomler admin UI.";
    continueBtn.disabled = true;
    return;
  }
  const issuer = state.tokenView.issuer ?? "(no issuer)";
  const expires = state.tokenView.expires_at_unix
    ? `expires ${formatRelative(state.tokenView.expires_at_unix)}`
    : "(no expiry)";
  validationEl.className = "token-validation valid";
  validationEl.textContent = `Valid token. Issuer: ${issuer}; ${expires}.`;
  continueBtn.disabled = false;
}

function formatRelative(unixSec) {
  const nowSec = Date.now() / 1000;
  const diff = unixSec - nowSec;
  const abs = Math.abs(diff);
  const label = (count, unit) => `${Math.round(count)} ${unit}${count >= 2 ? "s" : ""}`;
  let phrase;
  if (abs < 60) phrase = label(abs, "second");
  else if (abs < 3600) phrase = label(abs / 60, "minute");
  else if (abs < 86400) phrase = label(abs / 3600, "hour");
  else phrase = label(abs / 86400, "day");
  return diff < 0 ? `${phrase} ago` : `in ${phrase}`;
}

// ─── Step 4: Install ───────────────────────────────────────────────────────

const STEP_LABELS = {
  started: "Install started",
  preflight_started: "Pre-flight checks",
  preflight_ok: "Pre-flight: detected install",
  preflight_warning: "Pre-flight warning",
  asset_resolving: "Resolving installer URL",
  asset_resolved: "Installer URL resolved",
  download_started: "Downloading installer",
  download_progress: "Downloading installer", // collapsed into the bar
  download_verified: "Installer verified (SHA256)",
  msi_spawned: "Running MSI installer",
  msi_completed: "MSI installer finished",
  env_var_writing: "Writing service environment",
  env_var_set: "Service environment set",
  service_restarting: "Restarting service",
  service_restarted: "Service restarted",
  enroll_started: "Enrolling agent",
  enroll_ok: "Agent enrolled",
  done: "Done",
  error: "Error",
};

function wireInstall() {
  document.getElementById("install-cancel").addEventListener("click", async () => {
    try {
      await invoke("cmd_cancel_in_progress");
      showSnackbar("Cancellation requested — bailing at the next checkpoint.");
    } catch (err) {
      showSnackbar(`Cancel failed: ${err}`);
    }
  });
  document.getElementById("install-retry").addEventListener("click", async () => {
    state.installError = null;
    state.installProgress = [];
    await runInstall();
  });
  document.getElementById("install-back-to-welcome").addEventListener("click", async () => {
    state.installError = null;
    state.installProgress = [];
    await gotoStep("welcome");
  });
}

async function runInstall() {
  state.installProgress = [];
  state.installError = null;
  state.installInFlight = true;
  await gotoStep("install");

  document.getElementById("install-summary").textContent =
    `Installing ${flavourLabel(state.flavour)} from ${state.server} as “${state.device}”.`;
  renderInstall();

  // Attach the Channel listener BEFORE invoking cmd_install so no
  // ProgressEvent gets dropped.
  const channel = new Channel();
  channel.onmessage = (event) => {
    onProgressEvent(event);
  };

  try {
    const report = await invoke("cmd_install", {
      flavour: state.flavour,
      server: state.server,
      token: state.token,
      deviceName: state.device,
      onEvent: channel,
    });
    state.installDone = report;
    state.installInFlight = false;
    // Token cleared from memory now that enrollment succeeded.
    state.token = "";
    document.getElementById("token-input").value = "";
    await gotoStep("done");
  } catch (err) {
    state.installError = String(err);
    state.installInFlight = false;
    renderInstall();
  }
}

function onProgressEvent(event) {
  // Collapse DownloadProgress into a single progress bar update;
  // every other event becomes a checklist row.
  if (event.kind === "download_progress") {
    const start = state.installProgress.find((e) => e.kind === "download_started");
    if (start) {
      const pct = Math.min(100, (event.received_bytes / start.total_bytes) * 100);
      document.getElementById("progress-fill").style.width = `${pct.toFixed(1)}%`;
      document.getElementById("progress-label").textContent =
        `${(event.received_bytes / 1_000_000).toFixed(1)} MB of ${(start.total_bytes / 1_000_000).toFixed(1)} MB (${pct.toFixed(0)}%)`;
    }
    return;
  }
  state.installProgress.push(event);
  renderInstall();
}

function renderInstall() {
  const list = document.getElementById("install-checklist");
  list.innerHTML = "";
  for (const event of state.installProgress) {
    const li = document.createElement("li");
    li.className = checklistClass(event);
    const icon = document.createElement("span");
    icon.className = "icon";
    li.appendChild(icon);
    const label = document.createElement("span");
    label.textContent = checklistLabel(event);
    li.appendChild(label);
    list.appendChild(li);
  }

  // Progress bar visible only while a download is active.
  const downloadStarted = state.installProgress.some((e) => e.kind === "download_started");
  const downloadVerified = state.installProgress.some((e) => e.kind === "download_verified");
  document.getElementById("install-progress").hidden = !(downloadStarted && !downloadVerified);

  // Error panel.
  const errorEl = document.getElementById("install-error");
  if (state.installError) {
    errorEl.hidden = false;
    document.getElementById("install-error-message").textContent = state.installError;
    document.getElementById("install-cancel").hidden = true;
  } else {
    errorEl.hidden = true;
    document.getElementById("install-cancel").hidden = false;
  }
}

function checklistClass(event) {
  if (event.kind === "error") return "error";
  if (event.kind === "preflight_warning") return "warning";
  if (event.kind === "done" || event.kind === "enroll_ok" || event.kind === "msi_completed"
      || event.kind === "service_restarted" || event.kind === "env_var_set"
      || event.kind === "download_verified" || event.kind === "preflight_ok"
      || event.kind === "asset_resolved") {
    return "ok";
  }
  return "running";
}

function checklistLabel(event) {
  const base = STEP_LABELS[event.kind] ?? event.kind;
  if (event.kind === "preflight_ok") return `Pre-flight: detected ${event.existing}`;
  if (event.kind === "preflight_warning") return `${base}: ${event.message}`;
  if (event.kind === "asset_resolved") return `Resolved tag ${event.tag} (${(event.size_bytes / 1_000_000).toFixed(1)} MB)`;
  if (event.kind === "msi_spawned") return `Running MSI installer (PID ${event.pid})`;
  if (event.kind === "msi_completed") return `MSI installer finished (exit ${event.code}, ${event.decoded})`;
  if (event.kind === "enroll_ok") return `Agent enrolled (agent_id ${event.agent_id})`;
  if (event.kind === "error") return `Error at ${event.step}: ${event.message}`;
  return base;
}

function flavourLabel(flavour) {
  if (flavour === "peruser") return "perUser";
  if (flavour === "permachine") return "perMachine";
  if (flavour === "permachine-system-context") return "perMachine + SystemContext";
  return flavour;
}

// ─── Step 5: Done ──────────────────────────────────────────────────────────

function wireDone() {
  document.getElementById("done-finish").addEventListener("click", () => {
    window.close();
  });
}

function renderDone() {
  if (!state.installDone) return;
  document.getElementById("done-agent-id").textContent = state.installDone.agent_id;
  document.getElementById("done-tenant-id").textContent = state.installDone.tenant_id;
  document.getElementById("done-flavour").textContent = flavourLabel(state.installDone.flavour);
  document.getElementById("done-tag").textContent = state.installDone.tag;
  // SystemContext note: shown only when the operator picked that flavour.
  // (v1: cmd_install returns plain perMachine; the operator chose SC.)
  document.getElementById("done-systemcontext-note").hidden =
    state.flavour !== "permachine-system-context";
}

// ─── Snackbar ──────────────────────────────────────────────────────────────

let snackbarTimer;
function showSnackbar(message, durationMs = 4000) {
  const el = document.getElementById("snackbar");
  el.textContent = message;
  el.hidden = false;
  clearTimeout(snackbarTimer);
  snackbarTimer = setTimeout(() => {
    el.hidden = true;
  }, durationMs);
}

// ─── Go ────────────────────────────────────────────────────────────────────

init().catch((err) => {
  console.error("init failed:", err);
  showSnackbar(`Init failed: ${err}`);
});
