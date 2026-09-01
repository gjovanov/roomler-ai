// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-41 (#965) — the product demo: two machines, one browser tab.
 *
 * Windows 11 → macOS, each reached from the same tab, each pinging the OTHER
 * over the overlay so the mesh is shown working rather than asserted, and each
 * terminal dragged around so the recording shows input reaching the far end
 * rather than a still frame.
 *
 * `SHOTS` is a list — adding Fedora or Ubuntu back is a two-line change, and an
 * earlier take ran all four. Two is the cut that keeps it short and lets each
 * machine breathe.
 *
 * ⚠️ Captions are inline rather than imported from JSON. The sibling
 * `record-intro.spec.ts` uses `import … with { type: 'json' }`, which is
 * bun-only syntax that kills Playwright's collection under plain node — the
 * reason the nightly e2e lane copies `ui/` *minus* this directory.
 *
 * ⚠️ NOTHING in a device scene may touch page JS once the stream starts. The
 * page's main thread is saturated decoding and painting, so `evaluate`,
 * `boundingBox` and every locator query hang rather than answer — four takes
 * were lost to checks that could never return while the desktop was streaming
 * perfectly. Captions and geometry are taken BEFORE the click; everything after
 * it is CDP input only.
 *
 * ⚠️ `roomler peers` is deliberately NOT filmed. These machines are enrolled in
 * a second organization as well, and `peers` prints EVERY org — so the frame
 * would carry the whole real fleet, hostnames and all. `ping <overlay-ip>`
 * makes the same point in one line that leaks nothing.
 */
import { test, expect, type Page } from '@playwright/test'

const USERNAME = process.env.E2E_USERNAME || ''
const PASSWORD = process.env.E2E_PASSWORD || ''
const TENANT_ID = process.env.E2E_TENANT_ID || ''

/**
 * Clicking Connect to a painted desktop. Measured at ~9 s across takes; 11 s
 * leaves headroom without paying for it four times.
 *
 * ⚠️ This is the single biggest lever on runtime. Four connects at 15 s put a
 * third of the take into spinner, which is what pushed it past two minutes.
 */
const CONNECT_WAIT_MS = 11_000

type Shot = {
  id: string
  name: string
  caption: string
  /** Title-bar centre, as a FRACTION of the remote surface — resolution-independent. */
  grab: [number, number]
  /** Optional click before the drag (fraction): play a video, focus a shell. */
  poke?: [number, number]
  /**
   * Optional line typed after `poke` — kept short, it is read at a glance.
   * `{PEER}` is substituted with `peer`'s CURRENT overlay address (see
   * `overlayIp`); write the address literally and it goes stale on the next
   * renumber with nothing failing.
   */
  type?: string
  /** The node `{PEER}` resolves to, by name. */
  peer?: string
  /**
   * Pause between the focus click and typing.
   *
   * WARNING: not cosmetic. On the Mac the remote input pipeline needs a beat
   * after a click before it accepts keystrokes: at 700 ms it swallowed the
   * first NINETEEN characters of the ping command, leaving the shell with
   * `0.65.12.3` and a `command not found`. Filmed clean, reported ok, and
   * completely wrong - the same class as every other take lost here.
   */
  settleMs?: number
  /**
   * Per-machine connect wait. ⚠️ Not cosmetic: the Mac answers a CONSENT prompt
   * before the session starts ("Waiting for the agent to allow the
   * connection…"), so a take typed into a session that had not begun and the
   * keystrokes went nowhere — the terminal filmed empty while everything
   * reported fine. Machines that prompt need the longer wait.
   */
  waitMs?: number
}

/**
 * The overlay addresses the two machines ping, RESOLVED FROM THE SERVER at
 * record time rather than hardcoded.
 *
 * ⚠️ **They change under you, silently.** The first take shot `100.64.0.2` /
 * `100.64.0.3`; FR-47 then carved the demo org its own block and they became
 * `100.65.12.2` / `100.65.12.3`. Nothing failed anywhere — the harness would
 * have typed the old addresses, filmed two *unreachable* pings, and reported
 * every scene `ok`, because a scene is "no exception and no hang", never "the
 * content is right". That is the trap that cost eleven takes the first time.
 *
 * ⚠️ It cannot be checked by pinging from the recording box, which was the
 * first thing tried: this box drives a BROWSER and is not on the demo mesh at
 * all, so that check refuses every valid run. The two machines ping each
 * other, not us.
 *
 * ⚠️ Nor can the spec verify the ping SUCCEEDED — the terminal is pixels
 * inside a remote-desktop video stream, so there is no text to read. Deriving
 * the address is therefore not a nicety; it is the only place correctness can
 * be established at all.
 *
 * The env vars stay as an override for a machine that is not in the mesh yet.
 */
const MAC_NODE = process.env.E2E_MAC_NODE || 'macbook-daemon'
const WIN_NODE = process.env.E2E_WIN_NODE || 'windows-11'

/** Resolve a node's current overlay IPv4 by name, or fail loudly. */
async function overlayIp(page: Page, nodeName: string): Promise<string> {
  const res = await page.request.get(`/api/tenant/${TENANT_ID}/overlay-node`)
  expect(res.ok(), `overlay-node list failed: ${res.status()}`).toBeTruthy()
  const body = await res.json()
  const items: Array<Record<string, unknown>> = body.items ?? body ?? []
  const hit = items.find(
    (n) => String(n.name ?? n.machine_name ?? '').toLowerCase() === nodeName.toLowerCase(),
  )
  const ip = String(hit?.overlay_ip ?? hit?.ip ?? '')
  // A wrong-but-plausible address is the failure this whole function exists to
  // prevent, so an unresolved name stops the take instead of filming a miss.
  expect(ip, `no overlay IPv4 for node ${nodeName} — names: ${items
    .map((n) => n.name ?? n.machine_name)
    .join(', ')}`).toMatch(/^\d+\.\d+\.\d+\.\d+$/)
  return ip
}

/**
 * Scouted once against each live desktop. Fractions, not pixels, because the
 * remote resolutions differ wildly (2880x1800, 3024x1968, 1920x1080).
 */
const SHOTS: Shot[] = [
  {
    id: process.env.E2E_WINDOWS_ID || '6a9597b83d54d39b773c292f',
    name: 'windows-11',
    caption: 'A Windows 11 laptop — in a browser tab',
    grab: [0.415, 0.307],
    poke: [0.415, 0.541],
    // ⚠️ The OS `ping`, not `roomler ping`. Two reasons, both measured:
    //  · `roomler ping` talks to whichever daemon owns the LocalAPI socket. On
    //    the Mac that is the per-USER daemon, which has no overlay — the
    //    privileged one does. The OS ping just uses the TUN, so it works from
    //    any account with no sudo.
    //  · The target is the DAEMON node, not the GUI worker. `macbook-pro` is
    //    the per-user node and reads `offline` in the mesh; `macbook-daemon`
    //    is the one actually on the overlay. Pinging the worker fails.
    type: 'clear; ping -n 4 {PEER}', // → the MacBook
    peer: MAC_NODE,
  },
  {
    id: process.env.E2E_MACOS_ID || '6a95b6a13d54d39b773c5366',
    name: 'macbook-pro',
    caption: 'A MacBook — same tab, nothing installed here',
    grab: [0.641, 0.463],
    poke: [0.641, 0.667],
    type: 'clear; ping -c 4 {PEER}', // → the Windows box
    peer: WIN_NODE,
    settleMs: 2500,
    waitMs: 24_000, // consent gate — see waitMs above
  },
]

const SAY = {
  devices: 'Two machines. Two operating systems.',
  network: 'One private encrypted network between them',
  outro: 'roomler.ai — open source, self-hostable',
} as const

// ---------------------------------------------------------------------------

async function injectOverlay(page: Page) {
  await page.evaluate(() => {
    if (document.getElementById('rm-cap')) return
    const el = document.createElement('div')
    el.id = 'rm-cap'
    Object.assign(el.style, {
      position: 'fixed', bottom: '44px', left: '50%', transform: 'translateX(-50%)',
      zIndex: '2147483647', background: 'rgba(10,32,29,.93)', color: '#E6F5F2',
      padding: '15px 34px', borderRadius: '10px', fontSize: '23px',
      fontFamily: "'Segoe UI', system-ui, -apple-system, sans-serif",
      fontWeight: '600', letterSpacing: '-.01em', maxWidth: '80%', textAlign: 'center',
      border: '1px solid rgba(0,150,136,.45)', boxShadow: '0 10px 40px rgba(0,0,0,.45)',
      opacity: '0', transition: 'opacity .35s ease', pointerEvents: 'none', whiteSpace: 'nowrap',
    })
    document.body.appendChild(el)
  })
}

async function say(page: Page, text: string, holdMs = 2600) {
  await injectOverlay(page)
  await page.evaluate((t) => {
    const el = document.getElementById('rm-cap')
    if (el) { el.textContent = t; el.style.opacity = '1' }
  }, text)
  await page.waitForTimeout(holdMs)
}

async function hush(page: Page) {
  await page.evaluate(() => {
    const el = document.getElementById('rm-cap')
    if (el) el.style.opacity = '0'
  }).catch(() => {})
  await page.waitForTimeout(300)
}

/** Blur anything shaped like a JWT before it is filmed. */
async function redactSecrets(page: Page) {
  const n = await page.evaluate(() => {
    let hit = 0
    for (const el of Array.from(document.querySelectorAll<HTMLElement>('body *'))) {
      if (el.children.length) continue
      const t = el.textContent || ''
      if (t.includes('eyJ') && t.length > 40) { el.style.filter = 'blur(7px)'; hit++ }
    }
    return hit
  }).catch(() => 0)
  console.log(`  redacted ${n} token element(s)`)
}

/**
 * Suppress transient toasts for the whole recording.
 *
 * ⚠️ Clicking them away does not work, and a take proved it. The notices that
 * matter — "Connected in 9.1 s, slower than usual", the clipboard-permission
 * prompt — appear DURING the stream, which is precisely when the page's main
 * thread is too busy to answer a locator query. So they are suppressed by CSS
 * installed before the app's own JS runs, through an init script that survives
 * every navigation, rather than chased after the fact.
 */
async function suppressToasts(page: Page) {
  await page.addInitScript(() => {
    const install = () => {
      const s = document.createElement('style')
      s.textContent = '.v-snackbar,.v-snackbar__wrapper{display:none !important}'
      ;(document.head || document.documentElement).appendChild(s)
    }
    if (document.head) install()
    else document.addEventListener('DOMContentLoaded', install, { once: true })
  })
}

/**
 * A scene that hangs is skipped like one that throws.
 *
 * ⚠️ The catch alone is not enough and a take was lost proving it: a frozen
 * renderer never reaches the catch, so the whole test ran into its ceiling and
 * recorded minutes of a stuck page. Racing the budget bounds the take by
 * construction.
 */
async function scene(name: string, fn: () => Promise<void>, budgetMs = 60_000): Promise<boolean> {
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    await Promise.race([
      fn(),
      new Promise<never>((_, rej) => {
        timer = setTimeout(() => rej(new Error(`budget ${budgetMs}ms exceeded`)), budgetMs)
      }),
    ])
    console.log(`  scene ok      ${name}`)
    return true
  } catch (e) {
    console.log(`  scene SKIPPED ${name}: ${(e as Error).message.split('\n')[0]}`)
    return false
  } finally {
    if (timer) clearTimeout(timer)
  }
}

// ---------------------------------------------------------------------------

test.describe('Roomler demo recording', () => {
  test('record the product demo', async ({ page }) => {
    test.setTimeout(420_000)

    expect(USERNAME, 'E2E_USERNAME is required').not.toBe('')
    expect(PASSWORD, 'E2E_PASSWORD is required').not.toBe('')
    expect(TENANT_ID, 'E2E_TENANT_ID is required').not.toBe('')

    const ran: Record<string, boolean> = {}

    await suppressToasts(page)
    await page.goto('/login')
    await page.waitForLoadState('networkidle')
    const user = page.locator('input').first()
    await user.click()
    await user.pressSequentially(USERNAME, { delay: 40 })
    const pass = page.locator('input[type="password"]')
    await pass.click()
    await pass.pressSequentially(PASSWORD, { delay: 40 })
    await page.getByRole('button', { name: /sign in|log in|login/i }).click()
    await page.waitForTimeout(3000)

    // Resolve every ping target NOW, while the page still answers and before
    // any stream starts. A stale address is the one defect this take cannot
    // detect for itself — the terminal is pixels in a video, so there is no
    // text to assert on — which is why it is established here instead.
    const peerIp: Record<string, string> = {}
    for (const shot of SHOTS) {
      if (shot.peer) {
        peerIp[shot.name] = await overlayIp(page, shot.peer)
        // eslint-disable-next-line no-console
        console.log(`  ping target for ${shot.name}: ${shot.peer} = ${peerIp[shot.name]}`)
      }
    }

    // --- the fleet ---------------------------------------------------------
    ran.devices = await scene('devices', async () => {
      await page.goto(`/tenant/${TENANT_ID}/devices`)
      await page.waitForLoadState('networkidle')
      await page.waitForTimeout(1000)
      await redactSecrets(page)          // belt and braces; nothing here should carry one
      await say(page, SAY.devices, 2400)
      await hush(page)
    })

    // --- one scene per machine --------------------------------------------
    for (const shot of SHOTS) {
      ran[shot.name] = await scene(shot.name, async () => {
        await page.goto(`/tenant/${TENANT_ID}/agent/${shot.id}/remote`)
        await page.waitForLoadState('networkidle')
        await page.waitForTimeout(1200)
        await say(page, shot.caption, 2000)

        // Geometry BEFORE the stream starts, while the page still answers.
        // Falls back to the known 1280x720 layout if the query is slow.
        let box = { x: 240, y: 90, width: 1040, height: 630 }
        try {
          const b = await page.locator('canvas, .rc-surface, main').first().boundingBox({ timeout: 4000 })
          if (b && b.width > 400) box = b
        } catch { /* keep the fallback */ }

        const at = ([fx, fy]: [number, number]) =>
          [box.x + box.width * fx, box.y + box.height * fy] as const

        const connect = page.getByRole('button', { name: /^connect$/i }).first()
        if (await connect.isVisible().catch(() => false)) await connect.click()
        await hush(page)

        // ── from here: CDP input only, no page queries ──────────────────────
        await page.waitForTimeout(shot.waitMs ?? CONNECT_WAIT_MS)

        if (shot.poke) {
          const [px, py] = at(shot.poke)
          await page.mouse.click(px, py)
          await page.waitForTimeout(shot.settleMs ?? 700)
          // A throwaway Enter: harmless at any shell prompt, and it proves
          // the session is taking input before the line that matters is
          // typed.
          await page.keyboard.press('Enter')
          await page.waitForTimeout(500)
        }
        if (shot.type) {
          // `{PEER}` was resolved before the stream started — nothing may query
          // the page from here on (see the header note on the saturated main
          // thread), so the lookup cannot happen at this point.
          await page.keyboard.type(shot.type.replace('{PEER}', peerIp[shot.name] ?? ''), {
            delay: 45,
          })
          await page.keyboard.press('Enter')
          await page.waitForTimeout(6500)
        }

        // Drag the window by its title bar — a short circuit, then back, so the
        // motion reads as deliberate rather than as a twitch.
        const [gx, gy] = at(shot.grab)
        await page.mouse.move(gx, gy, { steps: 18 })
        await page.mouse.down()
        for (const [dx, dy] of [[90, 60], [-40, 130], [-140, 20], [30, -90], [0, 0]]) {
          await page.mouse.move(gx + dx, gy + dy, { steps: 16 })
          await page.waitForTimeout(260)
        }
        await page.mouse.up()
        await page.waitForTimeout(900)
      })
    }

    // --- the mesh ----------------------------------------------------------
    ran.network = await scene('network', async () => {
      await page.goto(`/tenant/${TENANT_ID}/network/dns`)
      await page.waitForLoadState('networkidle')
      await page.waitForTimeout(1200)
      await say(page, SAY.network, 2200)
      await hush(page)
    })

    // ⚠️ NOT `/landing`. A signed-in session redirects it to the Dashboard,
    // which renders a card per organization — so the closing frame of the first
    // four-device take showed two unrelated org names. Ending on the demo org's
    // own device list is both safe and a better last shot: the four machines
    // the video just visited, sitting there online.
    await scene('outro', async () => {
      await page.goto(`/tenant/${TENANT_ID}/devices`)
      await page.waitForLoadState('networkidle')
      await page.waitForTimeout(1200)
      await say(page, SAY.outro, 2800)
      await page.waitForTimeout(500)
    })

    const skipped = Object.entries(ran).filter(([, ok]) => !ok).map(([k]) => k)
    console.log(skipped.length ? `\n  ⚠️  skipped: ${skipped.join(', ')}` : '\n  all scenes recorded')

    // At least three of the four machines must have filmed, or the take does
    // not make its own argument and is not worth publishing.
    const machines = SHOTS.filter((s) => ran[s.name]).length
    expect(machines, `only ${machines}/${SHOTS.length} machines recorded — the take is unusable`).toBe(SHOTS.length)
  })
})
