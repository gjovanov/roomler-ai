// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
// Roomler Setup — unified SPA controller.
//
// Vanilla JS state machine driving the 5-step wizard (Welcome →
// Server → Token → Install → Done) via Tauri invoke calls into the
// `wizard_app` lib. Merges the two legacy SPA drivers:
//
//   - role cards + cross-flavour ack gate + force-kill flow from the
//     legacy agent wizard's SPA
//   - the {type, data} adjacently-tagged ProgressEvent handling from
//     the legacy tunnel wizard's SPA
//   (both legacy wizard crates retired in P4c-2)
//
// plus two NET-NEW patterns:
//   - `cmd_install_progress_replay` fast-forward on the Install step
//     (the legacy SPAs shipped the command but never called it), and
//   - an in-HTML force-kill confirmation section instead of relying
//     on a native dialog.
//
// Wire contracts:
//   - ProgressEvent: {type: "PascalCase", data: {camelCase fields}}
//     (wizard_shared::progress).
//   - WizardState: {schemaVersion, step, role, serverUrl, deviceName}
//     — camelCase, token NEVER included (H5).
//   - Command payloads (DetectResult / TokenValidation / DoneReport):
//     camelCase.

const { invoke, Channel } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = ["welcome", "server", "token", "install", "done"];
const ROLES = ["daemon-system", "daemon-machine", "daemon-user", "tunnel-client"];
// The roles whose MSI is perMachine (Role::msi_flavour → "permachine*"): the
// daemon runs as a service that can create the overlay's TUN adapter, so the
// private network is on by default for them (FR-84 S2).
const PER_MACHINE_ROLES = ["daemon-system", "daemon-machine"];

const state = {
  step: "welcome",
  role: null,          // one of ROLES, or null until picked
  roleAck: false,      // cross-flavour acknowledgement checkbox
  detection: null,     // DetectResult {agent: {...}, tunnel: {...}}
  server: "",
  device: "",
  token: "",           // pasted JWT — kept in JS memory ONLY, never
                       // sent through cmd_save_state.
  tokenView: null,     // TokenValidation (or {error}) from cmd_validate_token
  tokenValidating: false,
  installError: null,
  installErrorStep: null, // step scope from the last Error event
  installDone: null,   // DoneReport
  installInFlight: false,
  msiSpawned: false,   // relabels Cancel → Force-kill installer
  totalBytes: 0,       // download denominator
  overlayChoice: null, // FR-84 S2: the overlay box once the operator touched
                       // it (true/false); null = follow the role. Memory
                       // only — deliberately NOT in the persisted
                       // WizardState, so a relaunch re-derives it from the
                       // role instead of replaying a stale click.
  anchorSkipReason: null, // FR-104: why the anchor step did not anchor
  diagOptIn: false,    // FR-102 P3b: the Welcome page's diagnostics box.
                       // Memory only — deliberately NOT in the persisted
                       // WizardState and never defaulted from an earlier
                       // run: unticked is the default, every launch.
  pageEnteredAt: 0,    // performance.now() when the current page was shown;
                       // a page boundary's elapsed_ms is measured from it.
  installMode: "pasted", // FR-129: which token cmd_install enrolls with —
                       // "pasted" (state.token) or "paired" (the one Rust
                       // holds from the pairing; the SPA never sees it).
  pair: {              // FR-129 A3: the pairing as this page sees it. The
                       // device code and the delivered token never come
                       // here — see src/pairing.rs.
    phase: "idle",     // idle | starting | waiting | approved | ended
    unavailableFor: null, // the server whose start answered 404: paste only
    userCode: null,    // "BCDF-GHJK"
    uri: null,         // the page to open; carries no code
    expiresAt: 0,      // Date.now() ms when the code runs out
    intervalMs: 5000,  // the server's poll interval; grows on slow_down
    nextPollAt: 0,     // Date.now() ms of the next poll
    ticker: null,      // the ONE setInterval driving countdown + polls
    gen: 0,            // bumped by every start/stop: a stale async result
                       // (an old loop's poll, a superseded start) is dropped
    inFlight: false,   // a poll is awaiting its answer (never two at once)
    opened: false,     // the page was opened at least once (button label)
    orgName: null,     // from the approval, shown before the install: the
                       // org's display name (not unique) …
    orgSlug: null,     // … and its slug (unique); null from an older server
    approverName: null,     // the approver's display name (free text) …
    approverUsername: null, // … and their username (unique); null if older
    error: null,       // a start that failed (network, refusal)
    hint: null,        // a poll that failed, shown under the countdown
                       // until the next poll answers
    endedText: null,   // why the pairing ended (denied / expired)
  },
};

// Checklist rows keyed by event type — replay + live events render
// idempotently (a repeated type overwrites its row instead of
// appending a duplicate). JS Map preserves insertion order.
let installRows = new Map();

// ─── Bootstrap ─────────────────────────────────────────────────────────────

async function init() {
  wireGlobalListeners();
  await loadPersistedState();
  await probeAndRender();
}

async function loadPersistedState() {
  try {
    const persisted = await invoke("cmd_load_state");
    // Unknown / stale role strings re-prompt the picker on resume.
    state.role = ROLES.includes(persisted.role) ? persisted.role : null;
    state.server = persisted.serverUrl ?? "";
    state.device = persisted.deviceName ?? "";
    // Step "install" or "done" on relaunch means the previous install
    // was either interrupted or successful; either way, dump the
    // operator back at Welcome so they can re-detect and decide.
    if (persisted.step === "install" || persisted.step === "done") {
      state.step = "welcome";
    } else {
      state.step = STEPS.includes(persisted.step) ? persisted.step : "welcome";
    }
  } catch (err) {
    console.warn("load wizard state failed:", err);
  }
}

async function persistState() {
  try {
    await invoke("cmd_save_state", {
      state: {
        schemaVersion: 1,
        step: state.step,
        role: state.role,
        serverUrl: state.server,
        deviceName: state.device,
        // Token is deliberately NEVER part of this payload (H5).
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
    state.detection = {
      agent: { supported: false, kind: "clean", peruserVersion: null, permachineVersion: null, ambiguous: false },
      tunnel: { installed: false, machineName: null, configPath: null },
    };
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

  state.pageEnteredAt = performance.now();
  render();
}

function wireGlobalListeners() {
  // Wire the step handlers FIRST — the Next/Back buttons are critical,
  // and NOTHING below may prevent them from being bound. (Regression
  // 2026-07-17: the optional single-instance listener was wired first;
  // when the `event:listen` capability was missing it threw the whole
  // function aborted before these ran, so every Next button was dead.)
  wireWelcome();
  wireServer();
  wireToken();
  wireInstall();
  wireDone();

  // B9: single-instance plugin emits this when a second wizard EXE
  // launch finds an in-flight install in the first process. Pop a
  // snackbar; the new process exits silently on the Rust side. Fully
  // OPTIONAL + best-effort — guarded against both a synchronous throw
  // and a rejected promise so a missing/denied event capability can
  // never break wizard navigation.
  try {
    Promise.resolve(
      listen("installer-already-running", () => {
        showSnackbar("Wizard is already running — finish the current install first.");
      }),
    ).catch((e) => console.warn("single-instance listener unavailable:", e));
  } catch (e) {
    console.warn("single-instance listener unavailable:", e);
  }
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
  // FR-129: no poll loop outlives the Join step. The session itself is the
  // caller's decision — Back cancels it, a paired Install keeps it for
  // cmd_install — but the timer stops on every step change, whoever made it.
  if (name !== "token" && state.pair.ticker) stopPairingLoop();
  state.step = name;
  state.pageEnteredAt = performance.now();
  await persistState();
  render();
}

// ─── FR-102 P3b: opt-in setup diagnostics ──────────────────────────────────

// A page boundary: `welcome` / `server` / `token` with `ok` (Continue) or
// `cancelled` (Cancel), and the time spent on the page. The Rust recorder is
// the gate — it drops everything while the box is unticked and buffers until
// cmd_install names the server — so this is only ever a report, never a
// send, and it can never fail the wizard (the invoke's error is swallowed).
async function pageBoundary(step, outcome) {
  if (!state.diagOptIn) return;
  const elapsedMs = Math.max(0, Math.round(performance.now() - state.pageEnteredAt));
  try {
    await invoke("cmd_diagnostics_page", { step, outcome, elapsedMs });
  } catch (err) {
    console.warn("[diagnostics] page boundary not recorded:", err);
  }
}

async function setDiagOptIn(enabled) {
  state.diagOptIn = enabled;
  try {
    await invoke("cmd_diagnostics_opt_in", { enabled });
  } catch (err) {
    console.warn("[diagnostics] opt-in not recorded:", err);
    // The recorder never heard the tick, so nothing is sent — the honest
    // state to show is unticked.
    state.diagOptIn = false;
    document.getElementById("diag-opt-in").checked = false;
  }
}

// ─── Role helpers ──────────────────────────────────────────────────────────

function roleFlavour(role) {
  // Mirror of Role::msi_flavour on the Rust side.
  if (role === "daemon-system") return "permachine-system-context";
  if (role === "daemon-machine") return "permachine";
  if (role === "daemon-user") return "peruser";
  return null; // tunnel-client
}

function roleLabel(role) {
  if (role === "daemon-system") return "Background service (pre-logon)";
  if (role === "daemon-machine") return "Attended service";
  if (role === "daemon-user") return "This user only";
  if (role === "tunnel-client") return "Tunnel client";
  return role ?? "(none)";
}

function flavourLabel(flavour) {
  if (flavour === "peruser") return "perUser";
  if (flavour === "permachine") return "perMachine";
  if (flavour === "permachine-system-context") return "perMachine + SystemContext";
  return flavour;
}

function isPerMachine(role) {
  return PER_MACHINE_ROLES.includes(role);
}

// FR-84 S2 — what the overlay box shows: the operator's explicit choice if
// they made one, else the role's default. Mirrors
// `AdvancedOptions::with_flavour_defaults` on the Rust side, which applies
// the same rule when the option never reaches it.
function overlayChecked(role, choice) {
  return choice ?? isPerMachine(role);
}

// ─── Step 1: Welcome ────────────────────────────────────────────────────────

function wireWelcome() {
  document.getElementById("welcome-cancel").addEventListener("click", async () => {
    // FR-102 P3b: a cancel before any server is known is buffered in the
    // recorder and dies with the process — nothing is sent anywhere.
    await pageBoundary("welcome", "cancelled");
    await exitWizard();
  });
  document.getElementById("welcome-continue").addEventListener("click", async () => {
    await pageBoundary("welcome", "ok");
    await gotoStep("server");
  });
  // FR-102 P3b: the diagnostics box — memory only, reported to the recorder.
  document.getElementById("diag-opt-in").addEventListener("change", async (e) => {
    await setDiagOptIn(e.target.checked);
  });
  document.querySelectorAll("input[name='role']").forEach((radio) => {
    radio.addEventListener("change", (e) => {
      state.role = e.target.value;
      state.roleAck = false;
      document.getElementById("cross-flavour-ack").checked = false;
      // Role change invalidates any previous token introspection (the
      // audience gate depends on the role) — and any pairing, whose token
      // kind follows the role it was started for (FR-129).
      state.tokenView = null;
      if (state.pair.phase !== "idle") cancelPairing();
      persistState();
      renderWelcome();
    });
  });
  document.getElementById("cross-flavour-ack").addEventListener("change", (e) => {
    state.roleAck = e.target.checked;
    renderWelcome();
  });
}

function renderWelcome() {
  const agentSummary = document.getElementById("detect-agent-summary");
  const tunnelSummary = document.getElementById("detect-tunnel-summary");
  const ambiguous = document.getElementById("detect-ambiguous-warning");
  if (!state.detection) {
    agentSummary.textContent = "Probing…";
    return;
  }
  const a = state.detection.agent;
  const t = state.detection.tunnel;

  // Daemon detection summary.
  if (!a.supported) {
    agentSummary.textContent = "Daemon: not available on this platform (Windows-only).";
  } else if (a.kind === "clean") {
    agentSummary.textContent = "Daemon: no existing install detected.";
  } else if (a.kind === "peruser") {
    agentSummary.textContent = `Daemon: existing perUser install (version ${a.peruserVersion ?? "unknown"}).`;
  } else if (a.kind === "permachine") {
    agentSummary.textContent = `Daemon: existing perMachine install (version ${a.permachineVersion ?? "unknown"}).`;
  } else if (a.kind === "ambiguous") {
    agentSummary.textContent = `Daemon: both flavours installed (perUser ${a.peruserVersion ?? "?"} + perMachine ${a.permachineVersion ?? "?"}).`;
  }
  ambiguous.hidden = a.kind !== "ambiguous";

  // Tunnel detection summary.
  if (t.installed) {
    const machine = t.machineName ? ` (machine: ${t.machineName})` : "";
    tunnelSummary.textContent = `Tunnel client: already configured${machine} at ${t.configPath}. Continuing will overwrite the enrollment.`;
  } else {
    tunnelSummary.textContent = "Tunnel client: not configured on this machine.";
  }

  // Non-Windows: only the tunnel-client card makes sense.
  document.getElementById("platform-note").hidden = a.supported;
  document.querySelectorAll("label.role-daemon").forEach((card) => {
    card.hidden = !a.supported;
  });
  if (!a.supported && state.role && state.role !== "tunnel-client") {
    state.role = null;
  }

  // Restore radio selection.
  if (state.role) {
    const radio = document.querySelector(`input[name='role'][value='${state.role}']`);
    if (radio) radio.checked = true;
  }

  // Cross-flavour warning gate (daemon roles only) — BLOCKER-7 of
  // the rc.28 plan critique, relocated from the agent wizard.
  const warningEl = document.getElementById("cross-flavour-warning");
  const warningText = document.getElementById("cross-flavour-warning-text");
  const warning = crossFlavourWarning(a, roleFlavour(state.role));
  if (warning) {
    warningText.textContent = warning;
    warningEl.hidden = false;
  } else {
    warningEl.hidden = true;
  }

  // Continue button gate.
  const canContinue = state.role !== null && (warning === null || state.roleAck);
  document.getElementById("welcome-continue").disabled = !canContinue;

  // FR-102 P3b: the box shows the in-memory state (unticked on every
  // launch) and names the server the diagnostics would go to — the one the
  // enrollment goes to, as the Server page currently has it.
  document.getElementById("diag-opt-in").checked = state.diagOptIn;
  document.getElementById("diag-server").textContent = state.server || "https://roomler.ai";
}

function crossFlavourWarning(agentDetect, flavour) {
  if (!flavour || !agentDetect || !agentDetect.supported) return null;
  const wantsPerMachine = flavour.startsWith("permachine");
  const wantsPerUser = flavour === "peruser";
  if (agentDetect.kind === "peruser" && wantsPerMachine) {
    return "Switching from perUser → perMachine. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator.";
  }
  if (agentDetect.kind === "permachine" && wantsPerUser) {
    return "Switching from perMachine → perUser. Your existing enrollment will be lost; you'll need a fresh enrollment token from your administrator.";
  }
  if (agentDetect.kind === "ambiguous") {
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
    await pageBoundary("server", "ok");
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
  // FR-84 S2: a click on the overlay box is an explicit choice — it sticks
  // across Back → another role → Continue, where an untouched box would
  // follow the new role instead.
  document.getElementById("adv-overlay").addEventListener("change", (e) => {
    state.overlayChoice = e.target.checked;
    renderServer();
  });
}

function renderServer() {
  document.getElementById("server-url").value = state.server;
  document.getElementById("device-name").value = state.device;
  const valid = /^https?:\/\/[^\s]+$/i.test(state.server) && state.device.length > 0;
  document.getElementById("server-continue").disabled = !valid;
  // S2: the advanced section holds daemon config keys — meaningless for
  // the tunnel-client role.
  document.getElementById("advanced-wrap").hidden = state.role === "tunnel-client";
  // FR-84 S2: the overlay box follows the role until touched — on for the
  // perMachine roles (the service can create the TUN adapter), off for a
  // per-user install (its worker runs with a filtered token and cannot;
  // the hint says so). The box stays enabled: the choice is the
  // operator's, the default is only the likely one.
  document.getElementById("adv-overlay").checked = overlayChecked(state.role, state.overlayChoice);
  document.getElementById("adv-overlay-hint").hidden = state.role !== "daemon-user";
}

// S2 — gather the advanced daemon options for cmd_install. `null` for
// the tunnel-client role (the section is hidden there; the Rust side
// ignores it anyway). The overlay box is sent as the operator SAW it
// (renderServer already resolved the role default into it), so what lands
// in config.toml is what the screen showed.
function gatherAdvanced() {
  if (state.role === "tunnel-client") return null;
  return {
    overlayEnabled: document.getElementById("adv-overlay").checked,
    advertiseLocalSubnets: document.getElementById("adv-local-subnets").checked,
    autoGrantSession: document.getElementById("adv-auto-grant").checked,
    overlayAdvertisedRoutes: document.getElementById("adv-overlay-routes").value.trim(),
    advertiseRoutes: document.getElementById("adv-tunnel-routes").value.trim(),
  };
}

// ─── Step 3: Enrollment token ──────────────────────────────────────────────

function wireToken() {
  document.getElementById("token-back").addEventListener("click", async () => {
    state.token = "";
    state.tokenView = null;
    document.getElementById("token-input").value = "";
    // FR-129: Back forgets the pairing — its code and any delivered token —
    // so a changed server or role never meets a token minted for another.
    await cancelPairing();
    await gotoStep("server");
  });
  document.getElementById("token-continue").addEventListener("click", async () => {
    // FR-102 P3b: the token page is passed once, here; a Retry from the
    // Install page re-runs the pipeline, not this boundary.
    await pageBoundary("token", "ok");
    // FR-129: the pasted token wins; a pairing left running or approved
    // beside it is dropped, so the two paths never cross.
    await cancelPairing();
    state.installMode = "pasted";
    await runInstall();
  });
  let debounce;
  document.getElementById("token-input").addEventListener("input", (e) => {
    state.token = e.target.value;
    clearTimeout(debounce);
    debounce = setTimeout(validateToken, 350);
  });
  wirePairing();
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
    state.tokenView = await invoke("cmd_validate_token", { token: t, role: state.role });
    state.tokenView.error = null;
  } catch (err) {
    state.tokenView = { error: String(err) };
  } finally {
    state.tokenValidating = false;
    renderToken();
  }
}

function renderToken() {
  renderPairing();

  // The paste path's role-specific hint (the whole page's lead before
  // FR-129; now the line under "I already have a token").
  document.getElementById("paste-lead").textContent = state.role === "tunnel-client"
    ? "Paste a tunnel-enrollment token from Admin → Tunnels."
    : "Paste a fresh enrollment token from Admin → Agents.";

  const info = document.getElementById("token-info");
  const warnAudience = document.getElementById("token-warn-audience");
  const warnExpired = document.getElementById("token-warn-expired");
  const warnParse = document.getElementById("token-warn-parse");
  const continueBtn = document.getElementById("token-continue");

  // Reset.
  info.hidden = true;
  warnAudience.hidden = true;
  warnExpired.hidden = true;
  warnParse.hidden = true;
  continueBtn.disabled = true;

  if (state.tokenValidating || !state.tokenView) return;

  if (state.tokenView.error) {
    warnParse.hidden = false;
    return;
  }

  const view = state.tokenView;
  info.hidden = false;
  document.getElementById("token-issuer").textContent = view.issuer ?? "(none)";
  document.getElementById("token-type").textContent =
    view.tokenType ?? view.audience ?? "(none)";
  document.getElementById("token-expiry").textContent =
    view.expiresAtUnix ? formatExpiry(view.expiresAtUnix) : "(no exp)";
  // FR-104 — say up front that a second administrator prompt is coming.
  document.getElementById("token-anchor-yes").hidden = view.willAnchor !== true;
  document.getElementById("token-anchor-no").hidden = view.willAnchor !== false;

  // Tunnel role gates Continue on the token-type match; daemon roles
  // are not gated client-side (audienceMatches is null for them).
  if (view.audienceMatches === false) {
    warnAudience.hidden = false;
    document.getElementById("token-warn-aud-name").textContent =
      view.tokenType ?? view.audience ?? "(none)";
    return;
  }
  if (view.appearsExpired) {
    warnExpired.hidden = false;
    return;
  }
  continueBtn.disabled = false;
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

// ─── Step 3, FR-129 A3: pairing from the browser ───────────────────────────
//
// The flow: "Set up in the browser" → cmd_pair_start (Rust keeps the device
// code; this page gets the user code, the page and the clock) → the person
// types the code at /pair and approves → the poll loop sees `approved` →
// "Approved by X to join Y. Install into this org?" → Install runs the
// ordinary install with the token Rust holds (cmd_install, usePaired).
//
// The poll loop is ONE setInterval tick per second that updates the
// countdown and fires a poll when its interval is due. It stops on
// approval, denial, expiry, Cancel, Back, a new start and any step change
// (gotoStep); `gen` is bumped by every stop, and an async answer whose
// generation is stale is dropped, so two loops never run and no late
// answer lands on a later pairing. A failed poll (the network blinked) is
// retried at the interval until the clock, with one grace interval past
// expiry for the server's own answer, ends it.

function wirePairing() {
  document.getElementById("pair-start").addEventListener("click", startPairing);
  document.getElementById("pair-restart").addEventListener("click", startPairing);
  document.getElementById("pair-copy").addEventListener("click", copyPairCode);
  document.getElementById("pair-open").addEventListener("click", openPairPage);
  document.getElementById("pair-cancel").addEventListener("click", async () => {
    await cancelPairing();
    renderToken();
  });
  document.getElementById("pair-approved-cancel").addEventListener("click", async () => {
    // AC5: Cancel leaves the device unenrolled; the delivered token is
    // dropped here and expires unused on its own clock.
    await cancelPairing();
    renderToken();
  });
  document.getElementById("pair-approved-install").addEventListener("click", async () => {
    if (state.pair.phase !== "approved") return;
    await pageBoundary("token", "ok");
    state.installMode = "paired";
    await runInstall();
  });
}

async function startPairing() {
  stopPairingLoop();
  const p = state.pair;
  p.phase = "starting";
  p.error = null;
  p.hint = null;
  p.endedText = null;
  p.orgName = null;
  p.orgSlug = null;
  p.approverName = null;
  p.approverUsername = null;
  p.opened = false;
  renderToken();

  const gen = p.gen;
  let view;
  try {
    view = await invoke("cmd_pair_start", {
      server: state.server,
      role: state.role,
      deviceName: state.device,
    });
  } catch (err) {
    if (gen !== p.gen) return;
    console.warn("[pairing] start failed:", err);
    p.phase = "idle";
    p.error = `Could not start pairing: ${err}`;
    renderToken();
    return;
  }
  if (gen !== p.gen) {
    // Back or Cancel raced this start, and Rust now holds a session nobody
    // wants; drop it — unless a newer start is under way, which replaced
    // it already.
    if (p.phase === "idle") invoke("cmd_pair_cancel").catch(() => {});
    return;
  }
  if (view.kind === "unavailable") {
    // The server has no pairing (switched off, or older): paste only,
    // silently — the card hides and the paste box opens.
    p.unavailableFor = state.server;
    p.phase = "idle";
    document.getElementById("paste-wrap").open = true;
    renderToken();
    return;
  }
  p.phase = "waiting";
  p.userCode = view.userCode;
  p.uri = view.verificationUri;
  p.expiresAt = Date.now() + Math.max(0, view.expiresIn) * 1000;
  p.intervalMs = Math.max(1, view.interval ?? 5) * 1000;
  p.nextPollAt = Date.now() + p.intervalMs;
  p.ticker = setInterval(pairTick, 1000);
  renderToken();
}

// Stop the loop (timer + generation); the session in Rust is untouched.
function stopPairingLoop() {
  const p = state.pair;
  if (p.ticker) {
    clearInterval(p.ticker);
    p.ticker = null;
  }
  p.gen += 1;
  p.inFlight = false;
}

// Forget the pairing on both sides: the loop here, the device code and
// any delivered token in Rust.
async function cancelPairing() {
  stopPairingLoop();
  const p = state.pair;
  p.phase = "idle";
  p.userCode = null;
  p.uri = null;
  p.orgName = null;
  p.orgSlug = null;
  p.approverName = null;
  p.approverUsername = null;
  p.endedText = null;
  p.error = null;
  p.hint = null;
  p.opened = false;
  try {
    await invoke("cmd_pair_cancel");
  } catch (err) {
    console.warn("[pairing] cancel failed:", err);
  }
}

async function pairTick() {
  const p = state.pair;
  if (p.phase !== "waiting") return;
  renderPairStatus();
  const now = Date.now();
  if (p.inFlight) return;
  if (now < p.nextPollAt) {
    // Past the clock with one grace interval given to the server's own
    // answer: end it here.
    if (now >= p.expiresAt + p.intervalMs) endPairing("The code expired.");
    return;
  }

  p.inFlight = true;
  const gen = p.gen;
  let view;
  try {
    // Rust computes the next interval from this one on slow_down / 429.
    view = await invoke("cmd_pair_poll", { intervalSecs: Math.round(p.intervalMs / 1000) });
  } catch (err) {
    if (gen !== p.gen) return;
    p.inFlight = false;
    console.warn("[pairing] poll failed:", err);
    p.nextPollAt = Date.now() + p.intervalMs;
    if (Date.now() >= p.expiresAt + p.intervalMs) {
      endPairing("The code expired.");
    } else {
      p.hint = `Could not reach the server (${err}) — trying again.`;
      renderPairStatus();
    }
    return;
  }
  if (gen !== p.gen) return;
  p.inFlight = false;
  p.hint = null;
  p.nextPollAt = Date.now() + p.intervalMs;

  switch (view.status) {
    case "authorization_pending":
      break;
    case "slow_down":
      // RFC 8628 §3.5: the next interval as Rust computed it from ours —
      // the server's, never shorter than what we polled with.
      p.intervalMs = Math.max(p.intervalMs, Math.max(1, view.interval ?? 5) * 1000);
      p.nextPollAt = Date.now() + p.intervalMs;
      break;
    case "rate_limited":
      // HTTP 429 from the per-IP governor (several installers behind one
      // NAT): backed off like a slow_down — ours + 5 s, or Retry-After when
      // longer, as Rust computed it — and said so, since nothing is wrong
      // with the pairing itself.
      p.intervalMs = Math.max(p.intervalMs, Math.max(1, view.interval ?? 5) * 1000);
      p.nextPollAt = Date.now() + p.intervalMs;
      p.hint = "The server asked for a slower pace — trying again.";
      renderPairStatus();
      break;
    case "approved":
      stopPairingLoop();
      p.phase = "approved";
      p.orgName = view.orgName ?? null;
      p.orgSlug = view.orgSlug ?? null;
      p.approverName = view.approverName ?? null;
      p.approverUsername = view.approverUsername ?? null;
      renderToken();
      break;
    case "access_denied":
      endPairing("This was denied in the browser.");
      break;
    case "expired_token":
      endPairing("The code expired.");
      break;
    default:
      // A status this wizard does not know (a newer server): keep polling;
      // the clock still ends the loop.
      console.warn("[pairing] unknown poll status:", view.status, view.detail ?? "");
      break;
  }
}

function endPairing(text) {
  stopPairingLoop();
  const p = state.pair;
  p.phase = "ended";
  p.endedText = text;
  // Rust already dropped the session on the server's own denial/expiry;
  // on the clock's, drop it here — nothing is left to deliver.
  invoke("cmd_pair_cancel").catch(() => {});
  renderToken();
}

async function copyPairCode() {
  const code = state.pair.userCode;
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    showSnackbar("Code copied.");
  } catch (err) {
    // A webview may refuse the async clipboard; the selection path works on
    // a click.
    const ok = copyBySelection(code);
    showSnackbar(ok ? "Code copied." : "Could not copy — type the code shown.");
  }
}

function copyBySelection(text) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  document.body.removeChild(ta);
  return ok;
}

async function openPairPage() {
  // Rust opens the page it holds for this pairing; nothing is passed from
  // here, and the URL carries no code (operator decision 1).
  try {
    await invoke("cmd_pair_open");
    state.pair.opened = true;
    renderPairing();
  } catch (err) {
    showSnackbar(`Could not open the browser: ${err}`);
  }
}

function pairDisplayUri(uri) {
  return (uri ?? "").replace(/^https?:\/\//i, "").replace(/\/+$/, "");
}

// «Goran Jovanov» (@gjovanov) — or «Goran Jovanov» from an older server.
function pairApproverLabel(p) {
  const name = `«${p.approverName || "an administrator"}»`;
  return p.approverUsername ? `${name} (@${p.approverUsername})` : name;
}

// «Roomler» (roomler) — or «Roomler» from an older server.
function pairOrgLabel(p) {
  const name = `«${p.orgName || "an organization"}»`;
  return p.orgSlug ? `${name} (${p.orgSlug})` : name;
}

function renderPairing() {
  const p = state.pair;
  const available = p.unavailableFor !== state.server;
  document.getElementById("pair-card").hidden = !available;

  document.getElementById("token-lead").textContent = available
    ? "Approve this device from your browser — nothing to copy. Or paste a token you already have."
    : "This server does not offer pairing from the browser. Paste a fresh enrollment token.";
  const paste = document.getElementById("paste-wrap");
  document.getElementById("paste-summary").textContent = available
    ? "I already have a token"
    : "Enrollment token";
  if (!available) paste.open = true;

  const idle = p.phase === "idle" || p.phase === "starting";
  document.getElementById("pair-idle").hidden = !idle;
  const startBtn = document.getElementById("pair-start");
  startBtn.disabled = p.phase === "starting";
  startBtn.textContent = p.phase === "starting" ? "Starting…" : "Set up in the browser";
  const errEl = document.getElementById("pair-error");
  errEl.hidden = !p.error;
  errEl.textContent = p.error ?? "";

  document.getElementById("pair-active").hidden = p.phase !== "waiting";
  if (p.phase === "waiting") {
    document.getElementById("pair-code").textContent = p.userCode ?? "";
    const shown = pairDisplayUri(p.uri);
    document.getElementById("pair-uri").textContent = shown;
    document.getElementById("pair-open").textContent = p.opened
      ? "Open the page again"
      : `Open ${shown}`;
    renderPairStatus();
  }

  document.getElementById("pair-approved").hidden = p.phase !== "approved";
  if (p.phase === "approved") {
    // The hijack guard reads: «display name» (unique identifier). A display
    // name is free text anyone can copy; the username and the slug are
    // unique, so the person can tell their admin and their org from ones
    // merely named like them. An older server sends no identifiers, and the
    // wording then falls back to the names alone.
    document.getElementById("pair-approved-text").textContent =
      `Approved by ${pairApproverLabel(p)} to join ${pairOrgLabel(p)}.`;
  }

  document.getElementById("pair-ended").hidden = p.phase !== "ended";
  document.getElementById("pair-ended-text").textContent = p.endedText ?? "";
}

function renderPairStatus() {
  const p = state.pair;
  const left = Math.max(0, Math.ceil((p.expiresAt - Date.now()) / 1000));
  const mm = String(Math.floor(left / 60)).padStart(2, "0");
  const ss = String(left % 60).padStart(2, "0");
  const base = left > 0
    ? `Waiting for approval in the browser… the code expires in ${mm}:${ss}.`
    : "The code has run out — checking once more.";
  document.getElementById("pair-status").textContent = p.hint ? `${base} ${p.hint}` : base;
}

// ─── Step 4: Install ───────────────────────────────────────────────────────

const STEP_LABELS = {
  Started: "Install started",
  PreflightStarted: "Pre-flight checks",
  PreflightOk: "Pre-flight: detected install",
  PreflightWarning: "Pre-flight warning",
  AssetResolving: "Resolving download URL",
  AssetResolved: "Download URL resolved",
  DownloadStarted: "Downloading",
  DownloadVerified: "Download verified (SHA256)",
  MsiSpawned: "Running MSI installer",
  MsiCompleted: "MSI installer finished",
  ExtractStarted: "Extracting archive",
  ExtractDone: "Archive extracted",
  IntegrationStarted: "Wiring up PATH",
  IntegrationDone: "PATH integration finished",
  EnrollStarted: "Enrolling with Roomler",
  EnrollOk: "Enrolled",
  AnchorStarted: "Anchoring for SYSTEM exec (administrator prompt)",
  AnchorOk: "Anchored for SYSTEM exec",
  AnchorSkipped: "Not anchored",
  Done: "Done",
  Error: "Error",
  SystemContextError: "SystemContext step failed",
};

function wireInstall() {
  document.getElementById("install-cancel").addEventListener("click", async () => {
    if (state.msiSpawned) {
      // Post-spawn: the polite cancel flag can't stop msiexec —
      // surface the force-kill confirmation instead (H4).
      document.getElementById("force-kill-confirm").hidden = false;
      return;
    }
    try {
      await invoke("cmd_cancel_in_progress");
      showSnackbar("Cancellation requested — bailing at the next checkpoint.");
    } catch (err) {
      showSnackbar(`Cancel failed: ${err}`);
    }
  });
  document.getElementById("force-kill-back").addEventListener("click", () => {
    document.getElementById("force-kill-confirm").hidden = true;
  });
  document.getElementById("force-kill-confirm-btn").addEventListener("click", async () => {
    document.getElementById("force-kill-confirm").hidden = true;
    try {
      await invoke("cmd_force_kill_msi");
      showSnackbar("msiexec terminated. The install may be partial.");
    } catch (err) {
      showSnackbar(`Force-kill failed: ${err}`);
    }
  });
  document.getElementById("install-retry").addEventListener("click", async () => {
    await runInstall();
  });
  document.getElementById("install-back-to-welcome").addEventListener("click", async () => {
    state.installError = null;
    state.installErrorStep = null;
    installRows = new Map();
    // FR-129: a failed paired install's token is dropped with the pairing —
    // the next pass through the Join step starts clean.
    if (state.installMode === "paired") await cancelPairing();
    state.installMode = "pasted";
    await gotoStep("welcome");
  });
}

async function runInstall() {
  installRows = new Map();
  state.installError = null;
  state.installErrorStep = null;
  state.installDone = null;
  state.installInFlight = true;
  state.msiSpawned = false;
  state.totalBytes = 0;
  document.getElementById("install-systemcontext-error").hidden = true;
  document.getElementById("force-kill-confirm").hidden = true;
  await gotoStep("install");

  const what = state.role === "tunnel-client"
    ? "tunnel client"
    : `daemon (${flavourLabel(roleFlavour(state.role))})`;
  // FR-129: a paired install names the org it was approved into.
  const paired = state.installMode === "paired";
  const into = paired && state.pair.orgName ? ` into ${pairOrgLabel(state.pair)}` : "";
  document.getElementById("install-summary").textContent =
    `Installing the ${what} from ${state.server} as “${state.device}”${into}.`;
  renderInstall();

  // 1. Attach the Channel listener FIRST so no live event is dropped.
  const channel = new Channel();
  channel.onmessage = (event) => handleProgress(event, true);

  // 2. Fast-forward through the replay log (H1). The legacy SPAs
  //    shipped the command but never called it; here any events a
  //    previous attach raced past get re-rendered through the same
  //    handler. Rendering is idempotent (rows keyed by event type;
  //    a live `Started` clears the board), so overlap is harmless.
  try {
    const replayed = await invoke("cmd_install_progress_replay");
    for (const event of replayed) handleProgress(event, false);
  } catch (err) {
    console.warn("[runInstall] progress replay failed:", err);
  }

  // 3. Kick the install. A paired install sends no token: Rust enrolls
  //    with the one the pairing delivered (usePaired) and forgets it on
  //    success; a failure keeps it for Retry.
  try {
    const report = await invoke("cmd_install", {
      role: state.role,
      server: state.server,
      token: paired ? "" : state.token,
      deviceName: state.device,
      advanced: gatherAdvanced(),
      usePaired: paired,
      onEvent: channel,
    });
    console.log("[runInstall] cmd_install returned:", report);
    state.installDone = report;
    state.installInFlight = false;
    // Token cleared from memory now that enrollment succeeded.
    state.token = "";
    document.getElementById("token-input").value = "";
    if (paired) {
      // The session in Rust is already gone (cmd_install's success path);
      // this page's view of it goes too, keeping only the org and the
      // approver, which the Done page's lead shows (renderDone).
      const { orgName, orgSlug, approverName, approverUsername } = state.pair;
      stopPairingLoop();
      state.pair.phase = "idle";
      state.pair.userCode = null;
      state.pair.uri = null;
      Object.assign(state.pair, { orgName, orgSlug, approverName, approverUsername });
    }
    // Two-step transition: paint Done step FIRST (so the operator
    // sees the Finish button immediately), THEN persist state in the
    // background. If persistState ever hangs or rejects, the user is
    // already on the Done page and unblocked (rc.28 field lesson).
    state.step = "done";
    render();
    persistState().catch((err) =>
      console.warn("[runInstall] persistState failed:", err),
    );
  } catch (err) {
    console.error("[runInstall] cmd_install threw:", err);
    state.installError = String(err);
    state.installInFlight = false;
    renderInstall();
  }
}

// Throttle for the download progress bar (~10 Hz — DownloadProgress
// fires per 64 KiB chunk, hundreds of times per MB).
let lastProgressPaint = 0;

function handleProgress(event, live) {
  const type = event.type;
  const data = event.data ?? {};

  if (type === "DownloadProgress") {
    const now = performance.now();
    if (now - lastProgressPaint < 100) return;
    lastProgressPaint = now;
    if (state.totalBytes > 0) {
      const pct = Math.min(100, (data.receivedBytes / state.totalBytes) * 100);
      document.getElementById("progress-fill").style.width = `${pct.toFixed(1)}%`;
      document.getElementById("progress-label").textContent =
        `${(data.receivedBytes / 1_000_000).toFixed(1)} MB of ${(state.totalBytes / 1_000_000).toFixed(1)} MB (${pct.toFixed(0)}%)`;
    }
    return;
  }

  // A (live) Started marks the beginning of a fresh pipeline run —
  // clear any rows a stale replay painted.
  if (type === "Started" && live) {
    installRows = new Map();
  }

  if (type === "AssetResolved" || type === "DownloadStarted") {
    state.totalBytes = data.sizeBytes ?? data.totalBytes ?? state.totalBytes;
  }

  if (type === "MsiSpawned") {
    state.msiSpawned = true;
  }

  // FR-104 — kept for the Done page's note.
  if (type === "AnchorSkipped") {
    state.anchorSkipReason = data.reason ?? null;
  }

  if (type === "SystemContextError") {
    const stageLabel = {
      env_var_write: "Writing the service env-var failed",
      service_restart: "Restarting the service failed",
      unknown: "SystemContext step failed",
    }[data.stage] ?? `SystemContext (${data.stage}) failed`;
    document.getElementById("systemcontext-error-stage").textContent = stageLabel;
    document.getElementById("systemcontext-error-message").textContent = data.message ?? "";
    document.getElementById("systemcontext-error-hint").textContent = data.hint ?? "";
    document.getElementById("install-systemcontext-error").hidden = false;
  }

  if (type === "Error") {
    state.installErrorStep = data.step ?? null;
  }

  // Idempotent checklist row keyed by event type.
  installRows.set(type, { type, data });
  renderInstall();
}

function renderInstall() {
  const list = document.getElementById("install-checklist");
  list.innerHTML = "";
  for (const event of installRows.values()) {
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
  const downloadStarted = installRows.has("DownloadStarted");
  const downloadVerified = installRows.has("DownloadVerified");
  document.getElementById("install-progress").hidden = !(downloadStarted && !downloadVerified);

  // Cancel button: relabel once msiexec is running (the polite flag
  // can't interrupt it any more — only force-kill can).
  const cancelBtn = document.getElementById("install-cancel");
  cancelBtn.textContent = state.msiSpawned ? "Force-kill installer" : "Cancel";

  // Error recovery panel, scoped to the failing step.
  const errorEl = document.getElementById("install-error");
  if (state.installError) {
    errorEl.hidden = false;
    document.getElementById("install-error-title").textContent =
      state.installErrorStep ? `Install failed at: ${state.installErrorStep}` : "Install failed";
    document.getElementById("install-error-message").textContent = state.installError;
    cancelBtn.hidden = true;
  } else {
    errorEl.hidden = true;
    cancelBtn.hidden = false;
  }
}

function checklistClass(event) {
  const t = event.type;
  if (t === "Error" || t === "SystemContextError") return "error";
  if (t === "PreflightWarning" || t === "AnchorSkipped") return "warning";
  if (
    t === "Done" || t === "EnrollOk" || t === "AnchorOk" || t === "MsiCompleted" ||
    t === "ExtractDone" || t === "IntegrationDone" ||
    t === "DownloadVerified" || t === "PreflightOk" || t === "AssetResolved"
  ) {
    return "ok";
  }
  return "running";
}

function checklistLabel(event) {
  const t = event.type;
  const d = event.data ?? {};
  const base = STEP_LABELS[t] ?? t;
  if (t === "PreflightOk") return `Pre-flight: detected ${d.existing}`;
  if (t === "PreflightWarning") return `${base}: ${d.message}`;
  if (t === "AssetResolving") return `Resolving download URL (${d.artifact})`;
  if (t === "AssetResolved") return `Resolved tag ${d.tag} (${(d.sizeBytes / 1_000_000).toFixed(1)} MB)`;
  if (t === "MsiSpawned") return `Running MSI installer (PID ${d.pid})`;
  if (t === "MsiCompleted") return `MSI installer finished (exit ${d.code}, ${d.decoded})`;
  if (t === "ExtractDone") return `Extracted. Binary at ${d.tunnelBinary}`;
  if (t === "IntegrationDone")
    return `Integration: PATH ${d.pathUpdated ? "updated" : "unchanged"}; shortcut ${d.shortcutCreated ? "created" : "skipped"}`;
  if (t === "EnrollOk") return `Enrolled (${d.principalKind} ${d.principalId})`;
  if (t === "AnchorSkipped") return `Not anchored: ${d.reason}`;
  if (t === "Error") return `Error at ${d.step}: ${d.message}`;
  if (t === "SystemContextError") {
    const stageLabel = {
      env_var_write: "Writing service env-var",
      service_restart: "Restarting service",
      unknown: "SystemContext step",
    }[d.stage] ?? `SystemContext (${d.stage})`;
    return `${stageLabel} failed: ${d.message}${d.hint ? " — " + d.hint : ""}`;
  }
  return base;
}

// ─── Step 5: Done ──────────────────────────────────────────────────────────

function wireDone() {
  document.getElementById("done-finish").addEventListener("click", exitWizard);
}

function renderDone() {
  // Defensive: NEVER short-circuit when installDone is missing — the
  // operator must always see the Finish button + a value (placeholder
  // is fine) so the Done page can't render blank (rc.28 field lesson).
  const done = state.installDone ?? {};
  const isTunnel = (done.principalKind ?? "") === "tunnel_client"
    || (!done.principalKind && state.role === "tunnel-client");

  // P4b: daemon MSIs carry the roomler CLI (role→action composition —
  // a daemon install subsumes the tunnel client). cliIncluded is the
  // orchestrator's post-install existence check; false/absent means an
  // old pre-P4b MSI was served, so we don't promise a CLI we didn't
  // deliver.
  const cliIncluded = !isTunnel && done.cliIncluded === true;
  // GAP-A/P6: daemon roles also place the roomler-desktop GUI beside
  // the daemon (best-effort; a stale server / download failure leaves
  // it false, so we don't claim a desktop we didn't install).
  const desktopInstalled = !isTunnel && done.desktopInstalled === true;
  let lead;
  if (isTunnel) {
    lead = "The tunnel client is installed, enrolled, and on PATH.";
  } else {
    lead = "The Roomler daemon is installed and enrolled.";
    if (cliIncluded) lead += " The roomler CLI came with it (managed by the daemon's updater).";
    if (desktopInstalled) lead += " The roomler-desktop app is installed alongside it.";
    if (done.anchored === true) lead += " It is anchored: commands can run as SYSTEM here.";
  }
  // FR-129: a paired install names the org it joined and who approved it,
  // each with its unique identifier (runInstall keeps them after the
  // pairing's own view is cleared).
  if (state.pair.orgName) {
    lead += ` It joined ${pairOrgLabel(state.pair)}, approved by ${pairApproverLabel(state.pair)}.`;
  }
  document.getElementById("done-lead").textContent = lead;

  // FR-104 — the anchor step ran and did not anchor: the manual command,
  // with a placeholder where a fresh token goes (the one pasted here is
  // spent, and is never shown again).
  const anchorFailed = !isTunnel && done.anchored === false;
  document.getElementById("done-anchor-note").hidden = !anchorFailed;
  if (anchorFailed) {
    document.getElementById("done-anchor-reason").textContent = state.anchorSkipReason ?? "";
    const server = (state.server || "https://roomler.ai").replace(/\/+$/, "");
    document.getElementById("done-anchor-command").textContent =
      `roomlerd anchor create --server ${server} --token <fresh token> ` +
      `--device ${done.principalId ?? "<device id>"}`;
  }

  document.getElementById("done-principal-label").textContent =
    isTunnel ? "Tunnel client ID" : "Agent ID";
  document.getElementById("done-principal-id").textContent =
    done.principalId ?? "(unknown — install report missing)";
  document.getElementById("done-tenant-id").textContent = done.tenantId ?? "(unknown)";
  document.getElementById("done-role").textContent = roleLabel(done.role ?? state.role);
  document.getElementById("done-tag").textContent = done.tag ?? "(unknown)";

  // Daemon extras: MSI flavour.
  const flavour = done.flavour ?? (isTunnel ? null : roleFlavour(state.role));
  document.getElementById("done-flavour-label").hidden = !flavour;
  document.getElementById("done-flavour").hidden = !flavour;
  if (flavour) {
    document.getElementById("done-flavour").textContent = flavourLabel(flavour);
  }

  // Tunnel extras: binary path + PATH note.
  document.getElementById("done-binary-label").hidden = !done.binaryPath;
  document.getElementById("done-binary-path").hidden = !done.binaryPath;
  if (done.binaryPath) {
    document.getElementById("done-binary-path").textContent = done.binaryPath;
  }
  // P4b: the PATH note applies to BOTH principals now (daemon MSIs
  // append the install dir; the tunnel pipeline integrates its own
  // bin dir) — gate on pathUpdated alone.
  document.getElementById("done-path-note").hidden = !done.pathUpdated;

  // Config path (both pipelines report it).
  document.getElementById("done-config-label").hidden = !done.configPath;
  document.getElementById("done-config-path").hidden = !done.configPath;
  if (done.configPath) {
    document.getElementById("done-config-path").textContent = done.configPath;
  }

  document.getElementById("done-systemcontext-note").hidden =
    flavour !== "permachine-system-context";

  // Surface the underlying state for support — operator can copy
  // from DevTools or paste a screenshot when something looks off.
  console.log("[renderDone] state.installDone:", state.installDone);
  console.log("[renderDone] state.role:", state.role);
}

async function exitWizard() {
  // Tauri 2's JS `window.close()` from a webview can blank the
  // webview WITHOUT actually exiting the process — operator gets a
  // white/gray dead window with no controls (rc.28 field repro).
  // `cmd_exit_wizard` calls `AppHandle::exit(0)` on the Rust side.
  // Defensive fallback to `window.close()` if the invoke rejects.
  try {
    await invoke("cmd_exit_wizard");
  } catch (err) {
    console.warn("[exitWizard] cmd_exit_wizard failed, falling back:", err);
    window.close();
  }
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

// ─── Global error surfacing ────────────────────────────────────────────────

// Surface any unhandled JS exception or promise rejection in a sticky
// snackbar so the operator can take a screenshot for support. Without
// these handlers, a thrown exception mid-render leaves the wizard
// frozen on the previous step with no visible error.
window.addEventListener("error", (event) => {
  console.error("[window.error]", event.message, event.error);
  showSnackbar(`Wizard error: ${event.message}`, 30_000);
});
window.addEventListener("unhandledrejection", (event) => {
  console.error("[unhandledrejection]", event.reason);
  showSnackbar(`Wizard error: ${event.reason}`, 30_000);
});

// ─── Go ────────────────────────────────────────────────────────────────────

init().catch((err) => {
  console.error("init failed:", err);
  showSnackbar(`Init failed: ${err}`, 30_000);
});
