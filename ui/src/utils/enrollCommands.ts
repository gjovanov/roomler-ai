// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * Enrollment command templates — ONE source for every "how do I enroll
 * this machine" string the UI renders (unified EnrollmentDialog, S4).
 *
 * Derives everything from the CURRENT origin (never a hardcoded server
 * URL) and the freshly-issued single-use token. The exact flag
 * vocabulary mirrors the shipped installers:
 *   - scripts/install.sh   — roles `daemon` | `tunnel`; `--system`
 *                            (Linux-only root systemd SYSTEM unit)
 *   - scripts/install.ps1  — roles `daemon-user` | `daemon-machine` |
 *                            `daemon-system` | `tunnel-client`
 *   - roomlerd / roomler   — `enroll --server … --token … --name …`
 *     (+ `--machine-global` for the Windows SystemContext flavour ONLY —
 *     S1b: a plain-SCM daemon-machine service reads the per-user config)
 *   - /api/setup/{platform} — wizard EXE download proxy
 *     (windows | linux | macos per setup_release.rs::normalise_platform)
 *
 * The agent `scope` maps onto that vocabulary per OS:
 *   scope     Windows (-Role)   Linux                    macOS
 *   system    daemon-system     --role daemon --system   per-user (no --system)
 *   machine   daemon-machine    --role daemon --system   per-user (no --system)
 *   user      daemon-user       --role daemon            --role daemon
 * (Linux has no SystemContext/attended split — any machine-wide scope is
 * its `--system` unit; macOS daemons are per-user LaunchAgents only.)
 *
 * Locked by `ui/src/__tests__/utils/enrollCommands.spec.ts` so a binary
 * or flag rename fails unit tests instead of shipping stale copy.
 */

export type EnrollKind = 'agent' | 'tunnel'
export type EnrollOs = 'windows' | 'linux' | 'macos'
/** Agent install scope. Tunnel clients are a per-user CLI — no scope. */
export type DaemonScope = 'system' | 'machine' | 'user'

export interface CommandBlock {
  /** Stable id for tests + copy-button tracking. */
  id: string
  /** Short human label ("Recommended — one-line install"). */
  label: string
  /** The command itself (or URL for download links). */
  command: string
  /** True when `command` is a URL to download, not a shell command. */
  isDownload?: boolean
}

export interface OsCommands {
  os: EnrollOs
  title: string
  blocks: CommandBlock[]
  /** Scope caveat for this OS (elevation, macOS per-user fallback). */
  note?: string
}

const TOKEN_PLACEHOLDER = '<token>'

/** install.ps1 role per scope (same vocabulary as the setup wizard). */
const PS_ROLE: Record<DaemonScope, string> = {
  system: 'daemon-system',
  machine: 'daemon-machine',
  user: 'daemon-user',
}

/** Where the anchor command wants the device's id, which exists only once
 *  the device has enrolled. Not a valid PowerShell word on purpose: pasted
 *  unchanged, the command fails instead of anchoring something else. */
export const DEVICE_ID_PLACEHOLDER = '<device-id>'

/**
 * FR-129 B1-s — what the server advertises on `GET /api/setup/features`
 * (`stores/setupFeatures.ts`), as far as the commands care: one switch per
 * OS (B1-s2), so Linux can go live without macOS.
 */
export interface EnrollOptions {
  /**
   * The released Roomler Setup installs the full daemon on Linux (FR-129
   * B1), so the Linux wizard block is offered for an agent too: the store's
   * `wizardLinuxDaemon`, which reads `wizard_linux_daemon` or an older
   * server's `wizard_unix_daemon`. `false` or absent — the server has not
   * answered, failed to, or said no — keeps the FR-97 D5 rule: for an agent,
   * no wizard on Linux. The output is then exactly what it was before the
   * option existed.
   */
  wizardLinuxDaemon?: boolean
  /** The same for macOS: the store's `wizardMacosDaemon`. */
  wizardMacosDaemon?: boolean
}

/**
 * Build the per-OS command matrix for one enrollment kind. `token` may
 * be null while the token is still being issued — the placeholder keeps
 * the layout stable and nothing secret renders. `scope` only applies to
 * the agent kind (the tunnel CLI is always per-user).
 *
 * `anchor` (FR-104): the token may also anchor the Windows device it
 * enrolls. A machine-wide Windows install then gets a fourth block, the
 * elevated `roomlerd anchor create` with the same token, for the script
 * and manual paths; Roomler Setup runs it itself.
 *
 * FR-97 D5: Roomler Setup installed the agent on Windows only — on Linux and
 * macOS it installed the tunnel client (`agents/roomler-setup`), so an agent
 * enrollment there offered the script and the manual path, never the wizard.
 * FR-129 B1 teaches it the daemon there, and the SERVER says, per OS, when
 * the released wizard is that one: `options.wizardLinuxDaemon` and
 * `options.wizardMacosDaemon`, from `GET /api/setup/features` (B1-s2), off by
 * default and read as false whenever the server did not send a JSON `true`.
 */
export function enrollCommands(
  kind: EnrollKind,
  origin: string,
  token: string | null,
  scope: DaemonScope = 'system',
  anchor = false,
  options: EnrollOptions = {},
): OsCommands[] {
  const base = origin.replace(/\/+$/, '')
  const tok = token || TOKEN_PLACEHOLDER
  const scoped: DaemonScope = kind === 'agent' ? scope : 'user'
  const machineWide = scoped !== 'user'
  const shRole = kind === 'agent' ? 'daemon' : 'tunnel'
  const psRole = kind === 'agent' ? PS_ROLE[scoped] : 'tunnel-client'
  // S1b: ONLY the SystemContext flavour reads the machine-global config.
  const psMg = kind === 'agent' && scoped === 'system' ? ' --machine-global' : ''
  const manualBin = kind === 'agent' ? 'roomlerd' : 'roomler'
  const manualName = kind === 'agent' ? '"$(hostname)"' : '"My laptop"'

  const shOneLiner = (os: EnrollOs): CommandBlock => ({
    id: `${kind}-${os}-script`,
    label: 'Recommended — one-line install',
    // --system is Linux-only (install.sh rejects it on macOS).
    command: `curl -fsSL ${base}/api/setup/install.sh | sh -s -- --role ${shRole}${os === 'linux' && machineWide ? ' --system' : ''} --token ${tok} --server ${base} --name ${manualName}`,
  })

  // macOS installs the daemon inside an .app bundle, and there is no bare
  // `roomlerd` on PATH for the enrolling user — so the generic `roomlerd
  // enroll` this used to print could not work on a Mac. FR-46 P5b renamed the
  // bundle, so the path below carries the current name like every other OS.
  const macAgentBin = '/Library/Roomler/roomlerd.app/Contents/MacOS/roomlerd'

  const manual = (os: EnrollOs): CommandBlock => ({
    id: `${kind}-${os}-manual`,
    label: 'Already installed? Enroll manually',
    // Linux machine-wide mirrors install.sh --system's own "enroll later"
    // hint: root enroll into the system unit's /etc/roomler config.
    command:
      os === 'linux' && machineWide
        ? `sudo roomlerd --config /etc/roomler/config.toml enroll --server ${base} --token ${tok} --name ${manualName}`
        : os === 'macos' && kind === 'agent'
          ? `${macAgentBin} enroll --server ${base} --token ${tok} --name ${manualName}`
          : `${manualBin} enroll --server ${base} --token ${tok} --name ${manualName}`,
  })

  const wizard = (os: EnrollOs, platform: string): CommandBlock => ({
    id: `${kind}-${os}-wizard`,
    label: 'Graphical installer (Roomler Setup)',
    command: `${base}/api/setup/${platform}`,
    isDownload: true,
  })
  // FR-97 D5 — off Windows the wizard had no agent role to install, so an
  // agent got the script and the manual path only. FR-129 B1-s: the server
  // says when the released wizard installs the daemon there, and since B1-s2
  // it says so per OS; until it has said `true` for this OS, the D5 rule
  // stands. A tunnel client it always installed everywhere.
  const serverOffersDaemon: Record<'linux' | 'macos', boolean> = {
    linux: options.wizardLinuxDaemon === true,
    macos: options.wizardMacosDaemon === true,
  }
  const wizardWhereItInstalls = (os: 'linux' | 'macos', platform: string): CommandBlock[] =>
    kind === 'agent' && !serverOffersDaemon[os] ? [] : [wizard(os, platform)]

  return [
    {
      os: 'windows',
      title: 'Windows',
      note: machineWide
        ? 'Machine-wide install — run the commands below from an Administrator PowerShell (Terminal (Admin)).'
        : undefined,
      blocks: [
        {
          id: `${kind}-windows-script`,
          label: 'Recommended — one-line install (PowerShell)',
          command: `& ([scriptblock]::Create((irm ${base}/api/setup/install.ps1))) -Role ${psRole} -Token ${tok} -Server ${base}`,
        },
        wizard('windows', 'windows'),
        {
          id: `${kind}-windows-manual`,
          label: 'Already installed? Enroll manually',
          command: `${manualBin} enroll --server ${base} --token ${tok} --name "$env:COMPUTERNAME"${psMg}`,
        },
        // FR-104 — only a machine-wide install has a SYSTEM service to anchor.
        ...(kind === 'agent' && machineWide && anchor
          ? [
              {
                id: 'agent-windows-anchor',
                label: `Then anchor it, from the same Administrator PowerShell (replace ${DEVICE_ID_PLACEHOLDER} with the id on the device's page)`,
                command: `roomlerd anchor create --server ${base} --token ${tok} --device ${DEVICE_ID_PLACEHOLDER}`,
              },
            ]
          : []),
      ],
    },
    {
      os: 'linux',
      title: 'Linux',
      note: machineWide
        ? "Installs a root systemd system service (sudo will prompt). Screen capture needs a logged-in session — pick 'This user only' for desktops."
        : undefined,
      blocks: [shOneLiner('linux'), ...wizardWhereItInstalls('linux', 'linux'), manual('linux')],
    },
    {
      os: 'macos',
      title: 'macOS',
      // macOS needs TWO processes for full functionality and cannot merge
      // them: capture and input only work inside a GUI login session, while
      // creating a utun for the overlay needs root. They also cannot share one
      // enrollment — the hub keys on agent_id, so the second control-WS
      // connection displaces the first — hence a second token and a second
      // device row. Say so here rather than letting someone discover that
      // their Mac never appears in the mesh.
      note:
        kind === 'agent'
          ? 'Installs the per-user half (screen sharing + input). To also join the overlay mesh, mint a second enrollment token and append --daemon-token <token> — that adds a root LaunchDaemon and a second device row for this Mac.'
          : undefined,
      blocks: [shOneLiner('macos'), ...wizardWhereItInstalls('macos', 'macos'), manual('macos')],
    },
  ]
}

/** One OS's install card on the homepages: `LandingView.vue` and the static `/`. */
export interface InstallCard {
  os: EnrollOs
  title: string
  /** The one-line install: the card's terminal form. */
  command: string
  /**
   * The graphical installer where it installs the agent, else null — read
   * from the same blocks as the enroll dialog. Windows always (FR-97 D5: off
   * Windows Roomler Setup had only the tunnel client to install); Linux and
   * macOS each once the server advertises that OS on
   * `GET /api/setup/features` (FR-129 B1-s, per OS since B1-s2), which
   * decides it now. The homepages kept their own per-OS map until P0c-4, so
   * they offered a visitor on a Mac or Linux a wizard that could not set up
   * the device they came for — the same mistake the server's switches now
   * prevent while a released wizard is not field-proven on that OS.
   */
  wizardUrl: string | null
}

/** The homepages' install cards, one per OS, from the agent's commands. With
 *  no `options` (the server unknown, or every switch off) the cards are
 *  exactly what they were before FR-129. */
export function installCards(origin: string, options: EnrollOptions = {}): InstallCard[] {
  return enrollCommands('agent', origin, null, 'system', false, options).map((os) => ({
    os: os.os,
    title: os.title,
    command: os.blocks.find((b) => !b.isDownload)!.command,
    wizardUrl: os.blocks.find((b) => b.isDownload)?.command ?? null,
  }))
}
