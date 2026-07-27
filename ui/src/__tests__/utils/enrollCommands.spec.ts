import { describe, it, expect } from 'vitest'
import { enrollCommands } from '@/utils/enrollCommands'

/**
 * Locks the enrollment command templates to the shipped installer
 * contracts. If a binary name, flag, role vocabulary, or proxy route
 * changes, this spec fails instead of the admin UI shipping stale copy
 * (the pre-S4 dialogs hardcoded the prod URL and the retired
 * `roomler-agent --enroll` / `roomler-tunnel enroll` forms).
 */
describe('enrollCommands', () => {
  const ORIGIN = 'https://example.roomler.test'
  const TOKEN = 'eyJTOKEN'

  it('covers all three OSes with three blocks each, for both kinds', () => {
    for (const kind of ['agent', 'tunnel'] as const) {
      const all = enrollCommands(kind, ORIGIN, TOKEN)
      expect(all.map((o) => o.os)).toEqual(['windows', 'linux', 'macos'])
      for (const os of all) {
        expect(os.blocks).toHaveLength(3)
        for (const block of os.blocks) {
          if (!block.isDownload) expect(block.command).toContain(TOKEN)
          expect(block.command).toContain(ORIGIN)
        }
      }
    }
  })

  it('agent commands use the install-script role vocabulary + roomlerd', () => {
    const [windows, linux, macos] = enrollCommands('agent', ORIGIN, TOKEN)
    // install.ps1: ValidateSet daemon-user|daemon-machine|daemon-system|tunnel-client
    expect(windows!.blocks[0]!.command).toContain('/api/setup/install.ps1')
    expect(windows!.blocks[0]!.command).toContain('-Role daemon-user')
    expect(windows!.blocks[0]!.command).toContain(`-Token ${TOKEN}`)
    // install.sh: --role daemon|tunnel
    expect(linux!.blocks[0]!.command).toContain('/api/setup/install.sh')
    expect(linux!.blocks[0]!.command).toContain('--role daemon')
    expect(macos!.blocks[0]!.command).toContain('--role daemon')
    // Manual enrolls use the unified daemon binary.
    expect(windows!.blocks[2]!.command).toMatch(/^roomlerd enroll /)
    expect(linux!.blocks[2]!.command).toMatch(/^roomlerd enroll /)
    expect(linux!.blocks[2]!.command).toContain(`--server ${ORIGIN}`)
  })

  it('tunnel commands use the tunnel roles + the roomler CLI', () => {
    const [windows, linux] = enrollCommands('tunnel', ORIGIN, TOKEN)
    expect(windows!.blocks[0]!.command).toContain('-Role tunnel-client')
    expect(linux!.blocks[0]!.command).toContain('--role tunnel')
    expect(windows!.blocks[2]!.command).toMatch(/^roomler enroll /)
    expect(linux!.blocks[2]!.command).toMatch(/^roomler enroll /)
  })

  it('wizard downloads hit the setup proxy with valid platform slugs', () => {
    // setup_release.rs::normalise_platform accepts windows|linux|macos.
    const all = enrollCommands('agent', ORIGIN, TOKEN)
    const downloads = all.map((o) => o.blocks.find((b) => b.isDownload)!)
    expect(downloads.map((d) => d.command)).toEqual([
      `${ORIGIN}/api/setup/windows`,
      `${ORIGIN}/api/setup/linux`,
      `${ORIGIN}/api/setup/macos`,
    ])
  })

  it('never emits the retired binary names or a hardcoded server', () => {
    for (const kind of ['agent', 'tunnel'] as const) {
      for (const os of enrollCommands(kind, ORIGIN, TOKEN)) {
        for (const block of os.blocks) {
          expect(block.command).not.toContain('roomler-agent')
          expect(block.command).not.toContain('roomler-tunnel')
          expect(block.command).not.toContain('https://roomler.ai')
        }
      }
    }
  })

  it('renders a placeholder while the token is still being issued', () => {
    const [windows] = enrollCommands('agent', ORIGIN, null)
    expect(windows!.blocks[0]!.command).toContain('<token>')
  })

  it('normalises a trailing slash off the origin', () => {
    const [windows] = enrollCommands('agent', `${ORIGIN}/`, TOKEN)
    expect(windows!.blocks[0]!.command).not.toContain('.test//api')
  })
})
