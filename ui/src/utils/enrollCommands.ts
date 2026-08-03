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

/**
 * Build the per-OS command matrix for one enrollment kind. `token` may
 * be null while the token is still being issued — the placeholder keeps
 * the layout stable and nothing secret renders. `scope` only applies to
 * the agent kind (the tunnel CLI is always per-user).
 */
export function enrollCommands(
  kind: EnrollKind,
  origin: string,
  token: string | null,
  scope: DaemonScope = 'system',
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

  const manual = (os: EnrollOs): CommandBlock => ({
    id: `${kind}-${os}-manual`,
    label: 'Already installed? Enroll manually',
    // Linux machine-wide mirrors install.sh --system's own "enroll later"
    // hint: root enroll into the system unit's /etc/roomler config.
    command:
      os === 'linux' && machineWide
        ? `sudo roomlerd --config /etc/roomler/config.toml enroll --server ${base} --token ${tok} --name ${manualName}`
        : `${manualBin} enroll --server ${base} --token ${tok} --name ${manualName}`,
  })

  const wizard = (os: EnrollOs, platform: string): CommandBlock => ({
    id: `${kind}-${os}-wizard`,
    label: 'Graphical installer (Roomler Setup)',
    command: `${base}/api/setup/${platform}`,
    isDownload: true,
  })

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
      ],
    },
    {
      os: 'linux',
      title: 'Linux',
      note: machineWide
        ? "Installs a root systemd system service (sudo will prompt). Screen capture needs a logged-in session — pick 'This user only' for desktops."
        : undefined,
      blocks: [shOneLiner('linux'), wizard('linux', 'linux'), manual('linux')],
    },
    {
      os: 'macos',
      title: 'macOS',
      note: machineWide
        ? 'macOS installs per-user (LaunchAgent) — the commands below are the per-user flow.'
        : undefined,
      blocks: [shOneLiner('macos'), wizard('macos', 'macos'), manual('macos')],
    },
  ]
}
