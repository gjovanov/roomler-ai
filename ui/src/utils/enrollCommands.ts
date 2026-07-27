/**
 * Enrollment command templates — ONE source for every "how do I enroll
 * this machine" string the UI renders (unified EnrollmentDialog, S4).
 *
 * Derives everything from the CURRENT origin (never a hardcoded server
 * URL) and the freshly-issued single-use token. The exact flag
 * vocabulary mirrors the shipped installers:
 *   - scripts/install.sh   — roles `daemon` | `tunnel`
 *   - scripts/install.ps1  — roles `daemon-user` | `daemon-machine` |
 *                            `daemon-system` | `tunnel-client`
 *   - roomlerd / roomler   — `enroll --server … --token … --name …`
 *   - /api/setup/{platform} — wizard EXE download proxy
 *     (windows | linux | macos per setup_release.rs::normalise_platform)
 *
 * Locked by `ui/src/__tests__/utils/enrollCommands.spec.ts` so a binary
 * or flag rename fails unit tests instead of shipping stale copy.
 */

export type EnrollKind = 'agent' | 'tunnel'
export type EnrollOs = 'windows' | 'linux' | 'macos'

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
}

const TOKEN_PLACEHOLDER = '<token>'

/**
 * Build the per-OS command matrix for one enrollment kind. `token` may
 * be null while the token is still being issued — the placeholder keeps
 * the layout stable and nothing secret renders.
 */
export function enrollCommands(
  kind: EnrollKind,
  origin: string,
  token: string | null,
): OsCommands[] {
  const base = origin.replace(/\/+$/, '')
  const tok = token || TOKEN_PLACEHOLDER
  const shRole = kind === 'agent' ? 'daemon' : 'tunnel'
  const psRole = kind === 'agent' ? 'daemon-user' : 'tunnel-client'
  const manualBin = kind === 'agent' ? 'roomlerd' : 'roomler'
  const manualName = kind === 'agent' ? '"$(hostname)"' : '"My laptop"'

  const shOneLiner = (os: EnrollOs): CommandBlock => ({
    id: `${kind}-${os}-script`,
    label: 'Recommended — one-line install',
    command: `curl -fsSL ${base}/api/setup/install.sh | sh -s -- --role ${shRole} --token ${tok} --server ${base} --name ${manualName}`,
  })

  const manual = (os: EnrollOs): CommandBlock => ({
    id: `${kind}-${os}-manual`,
    label: 'Already installed? Enroll manually',
    command: `${manualBin} enroll --server ${base} --token ${tok} --name ${manualName}`,
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
          command: `${manualBin} enroll --server ${base} --token ${tok} --name "$env:COMPUTERNAME"`,
        },
      ],
    },
    {
      os: 'linux',
      title: 'Linux',
      blocks: [shOneLiner('linux'), wizard('linux', 'linux'), manual('linux')],
    },
    {
      os: 'macos',
      title: 'macOS',
      blocks: [shOneLiner('macos'), wizard('macos', 'macos'), manual('macos')],
    },
  ]
}
