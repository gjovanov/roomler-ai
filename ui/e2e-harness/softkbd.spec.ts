// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2888 — every key reaches the host EXACTLY ONCE, in a real Chromium.
//
// The page under test is `softkbd.html` (see `softkbd.ts`): the real
// `MobileKeyboard.vue` plus a window-level capture listener running the real
// `routeKeyEvent` / `applyKeyRoute` / `routeDeferredPaste`, both recording
// what the host would get and WHO sent it. The states are the viewer's: the
// soft keyboard open (its textarea focused) with the pointer outside the
// stage — what a tablet or a phone with a hardware keyboard looks like, since
// a finger leaves the stage when it lifts — or inside it, or under keyboard
// lock; the soft keyboard closed with the stage or the body focused; one of
// the viewer's own inputs focused. The keys are pressed as a hardware
// keyboard presses them (`page.keyboard.press`), typed as the soft keyboard
// types them (`insertText`, which raises `input` and no keydown), composed
// through the IME (CDP `Input.imeSetComposition`), and pasted.
//
// One owner per key, and WHICH owner is part of the contract: a HID frame (a
// named key, a chord, a modifier) is the listener's; a printable character
// aimed at the textarea is the textarea's, through its `input` handler — the
// one place the toolbar's sticky Ctrl / Alt / Win become a chord. Before the
// fix, with the soft keyboard open and the pointer outside, a printable
// character, Enter, Tab, Escape and the arrows had two owners, and Ctrl+V
// typed the browser's clipboard on top of the host's paste.
import { test, expect, type Page } from '@playwright/test'
import type { FrameVia, HarnessState, HostFrame } from './softkbd'

const CAPTURE = 'textarea[data-rc-capture]'

type Press = { count: number; via: Set<string> }

async function open(page: Page) {
  await page.goto('/e2e-harness/softkbd.html')
  await page.waitForFunction(() => !!window.__harness)
}

/** Open the soft keyboard (the component focuses its textarea), then set the
 *  viewer state and clear the record. */
async function openSoftKeyboard(page: Page, state: Partial<HarnessState> = {}) {
  await page.evaluate(() => window.__harness.open())
  await expect(page.locator(CAPTURE)).toBeFocused()
  await page.evaluate((s) => window.__harness.reset(s), state)
}

/** What reached the host — after the deferral's fallback timer and any `input`
 *  event had their chance. */
async function frames(page: Page): Promise<HostFrame[]> {
  await page.waitForTimeout(150)
  return page.evaluate(() => window.__harness.sent)
}

/** How many times the host would see each key go DOWN — a `key_text` is one
 *  press of its text — and who sent them. */
function presses(sent: HostFrame[]): Map<string, Press> {
  const out = new Map<string, Press>()
  for (const f of sent) {
    let id: string | null = null
    if (f.t === 'key_text') id = `text:${f.text}`
    else if (f.t === 'key' && f.down) id = `hid:0x${f.code.toString(16)}`
    if (id === null) continue
    const e = out.get(id) ?? { count: 0, via: new Set<string>() }
    e.count++
    e.via.add(f.via)
    out.set(id, e)
  }
  return out
}

/** Per HID code, downs minus ups — zero everywhere means nothing is left held
 *  and nothing was released twice. */
function hidBalance(sent: HostFrame[]): Record<string, number> {
  const bal: Record<string, number> = {}
  for (const f of sent) {
    if (f.t !== 'key') continue
    const k = `0x${f.code.toString(16)}`
    bal[k] = (bal[k] ?? 0) + (f.down ? 1 : -1)
  }
  for (const k of Object.keys(bal)) if (bal[k] === 0) delete bal[k]
  return bal
}

/** One line per key for the report: `[L]` the listener, `[S]` the soft
 *  keyboard component, `[P]` the deferred paste's flush; an upload by `[D]`
 *  the files drawer or `[F]` the deferred Ctrl+V's files path (#2910). */
function describeFrames(sent: HostFrame[]): string {
  const tag = (via: string) => (via === 'listener' ? 'L' : via === 'soft-keyboard' ? 'S' : 'P')
  return sent
    .map((f) => {
      switch (f.t) {
        case 'key':
          return `[${tag(f.via)}]key:0x${f.code.toString(16)}${f.down ? '↓' : '↑'}${f.mods ? `(mods=${f.mods})` : ''}`
        case 'key_text':
          return `[${tag(f.via)}]text:${JSON.stringify(f.text)}`
        case 'paste':
          return `[P]paste(${JSON.stringify(f.text)},files=${f.files})`
        case 'upload':
          return `[${f.via === 'drawer' ? 'D' : 'F'}]upload(files=${f.files})`
        case 'release-held':
          return `[L]release-held(${f.keys.map((k) => '0x' + k.toString(16)).join(',')})`
        default:
          return `[L]${f.t}`
      }
    })
    .join(' ')
}

/** The expectation every key shares: each press once, by exactly the owner the
 *  contract names, every HID down matched by one up, the textarea left empty.
 *  A character the textarea owns raises exactly one `input` event; nothing
 *  else may raise any. */
async function expectOneOwner(
  page: Page,
  sent: HostFrame[],
  expected: Record<string, FrameVia>,
  label: string,
) {
  const got = presses(sent)
  const detail = `${label}: ${describeFrames(sent)}`
  expect([...got.keys()].sort(), detail).toEqual(Object.keys(expected).sort())
  for (const [id, p] of got) {
    expect(p.count, `${id} reached the host ${p.count} times — ${detail}`).toBe(1)
    expect([...p.via], `${id} was sent by ${[...p.via].join(' and ')} — ${detail}`).toEqual([expected[id]])
  }
  expect(hidBalance(sent), `held or doubly released — ${detail}`).toEqual({})
  const typedByTextarea = Object.values(expected).filter((v) => v === 'soft-keyboard').length
  expect(
    await page.evaluate(() => window.__harness.inputEvents()),
    `input events on the textarea — ${detail}`,
  ).toBe(typedByTextarea)
  expect(await page.evaluate(() => window.__harness.captureValue())).toBe('')
}

const report: string[] = []
function note(state: string, key: string, sent: HostFrame[]) {
  report.push(`| ${state} | \`${key}\` | ${describeFrames(sent) || '(nothing)'} |`)
}

/** A hardware key, and the one press — with its owner — the host must see
 *  for it while the soft keyboard is open. */
const KEYS: { press: string; expected: Record<string, FrameVia> }[] = [
  { press: 'a', expected: { 'text:a': 'soft-keyboard' } },
  { press: 'Shift+A', expected: { 'hid:0xe1': 'listener', 'text:A': 'soft-keyboard' } },
  { press: 'Space', expected: { 'text: ': 'soft-keyboard' } },
  { press: 'Enter', expected: { 'hid:0x28': 'listener' } },
  { press: 'Tab', expected: { 'hid:0x2b': 'listener' } },
  { press: 'Escape', expected: { 'hid:0x29': 'listener' } },
  { press: 'ArrowLeft', expected: { 'hid:0x50': 'listener' } },
  { press: 'ArrowUp', expected: { 'hid:0x52': 'listener' } },
  { press: 'Backspace', expected: { 'hid:0x2a': 'listener' } },
  { press: 'Delete', expected: { 'hid:0x4c': 'listener' } },
  { press: 'Home', expected: { 'hid:0x4a': 'listener' } },
  { press: 'F2', expected: { 'hid:0x3b': 'listener' } },
  { press: 'Control+c', expected: { 'hid:0xe0': 'listener', 'hid:0x6': 'listener' } },
]

const SOFT_STATES: { name: string; state: Partial<HarnessState> }[] = [
  { name: 'pointer outside the stage', state: {} },
  { name: 'pointer inside the stage', state: { pointerInside: true } },
  { name: 'keyboard lock', state: { keyboardLocked: true } },
]

/** The listener's frames for a hardware key with the soft keyboard CLOSED:
 *  nothing editable is focused, so a printable character is the listener's. */
const CLOSED_EXPECTED: Record<string, FrameVia> = {
  'text:a': 'listener',
  'hid:0x28': 'listener',
  'hid:0x2b': 'listener',
  'hid:0x2a': 'listener',
}

test.beforeEach(async ({ page }) => {
  await open(page)
})

test.afterAll(() => {
  if (report.length === 0) return
  // The matrix, ready for the issue: state × key → what the host received.
  console.log(['', '| state | key | host receives |', '|---|---|---|', ...report, ''].join('\n'))
})

for (const s of SOFT_STATES) {
  test.describe(`soft keyboard open, hardware keyboard, ${s.name}`, () => {
    for (const k of KEYS) {
      test(`${k.press} reaches the host once`, async ({ page }) => {
        await openSoftKeyboard(page, s.state)
        await page.keyboard.press(k.press)
        const sent = await frames(page)
        note(`soft open, ${s.name}`, k.press, sent)
        await expectOneOwner(page, sent, k.expected, `${s.name}, ${k.press}`)
      })
    }
  })
}

test.describe('soft keyboard open, hardware keyboard — the chords with a second effect', () => {
  test('Ctrl+C is forwarded once and mirrors the host clipboard once', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.keyboard.press('Control+c')
    const sent = await frames(page)
    expect(sent.filter((f) => f.t === 'clipboard-mirror')).toHaveLength(1)
    await expectOneOwner(page, sent, { 'hid:0xe0': 'listener', 'hid:0x6': 'listener' }, 'Ctrl+C')
  })

  test('Ctrl+V pastes on the host once and the textarea does not type the clipboard too', async ({ page }) => {
    await openSoftKeyboard(page)
    const written = await page.evaluate(() =>
      navigator.clipboard.writeText('from the browser').then(
        () => true,
        () => false,
      ),
    )
    await page.keyboard.press('Control+v')
    const sent = await frames(page)
    note('soft open, pointer outside', 'Control+v', sent)
    const got = presses(sent)
    expect([...got.keys()].sort(), describeFrames(sent)).toEqual(['hid:0x19', 'hid:0xe0'])
    // The V is the deferral's: flushed once, after the paste event (or its
    // fallback timer), never as a plain forwarded keystroke.
    expect(got.get('hid:0x19'), describeFrames(sent)).toEqual({ count: 1, via: new Set(['paste-flush']) })
    expect(got.get('hid:0xe0')).toEqual({ count: 1, via: new Set(['listener']) })
    expect(hidBalance(sent)).toEqual({})
    // Nothing typed through the textarea: no `key_text`, no `input` event.
    // This is what `routeDeferredPaste`'s `preventDefault` buys — remove it
    // and the browser pastes into the textarea, whose `input` handler types
    // the clipboard on the host a second time.
    expect(sent.filter((f) => f.via === 'soft-keyboard'), describeFrames(sent)).toEqual([])
    expect(await page.evaluate(() => window.__harness.inputEvents())).toBe(0)
    if (written) {
      // The paste event carried the browser's clipboard to the deferral.
      const paste = sent.find((f) => f.t === 'paste')
      test.info().annotations.push({ type: 'paste', description: paste ? JSON.stringify(paste) : 'no paste event (timer flush)' })
    }
  })

  test('Ctrl+Alt+End fires the SAS once and forwards no End', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.keyboard.press('Control+Alt+End')
    const sent = await frames(page)
    note('soft open, pointer outside', 'Control+Alt+End', sent)
    expect(sent.filter((f) => f.t === 'sas')).toHaveLength(1)
    expect(sent.filter((f) => f.t === 'key' && f.code === 0x4d)).toEqual([])
    expect(sent.filter((f) => f.via === 'soft-keyboard')).toEqual([])
  })

  test('Ctrl→Cmd on a Mac host: the Control frames carry Cmd, once each', async ({ page }) => {
    await openSoftKeyboard(page, { macCtrlAsCmd: true })
    await page.keyboard.press('Control+c')
    const sent = await frames(page)
    note('soft open, Mac host', 'Control+c', sent)
    await expectOneOwner(page, sent, { 'hid:0xe3': 'listener', 'hid:0x6': 'listener' }, 'Ctrl→Cmd')
    expect(sent.filter((f) => f.t === 'key' && (f.code === 0xe0 || f.code === 0xe4))).toEqual([])
  })
})

test.describe("the soft keyboard's own paths still reach the host once", () => {
  test('text typed by the OS keyboard (insertText: an input event, no keydown)', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.keyboard.insertText('hello')
    const sent = await frames(page)
    note('soft open, OS keyboard', 'insertText("hello")', sent)
    expect(sent, describeFrames(sent)).toEqual([{ via: 'soft-keyboard', t: 'key_text', text: 'hello' }])
    expect(await page.evaluate(() => window.__harness.captureValue())).toBe('')
  })

  test("an iOS-style letter — a real keydown with no physical code, then the insertion — is the textarea's, once", async ({ page }) => {
    // WebKit's software keyboard fires `keydown` (key "a", keyCode 65, code "")
    // for every letter and then inserts it. The listener must leave that
    // keydown alone: before the fix it forwarded it as text (`decideKeyAction`
    // reads `key`, not `code`) AND the insertion typed it — twice.
    await openSoftKeyboard(page)
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
      ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', code: '', keyCode: 65, bubbles: true, cancelable: true }))
    })
    await page.keyboard.insertText('a')
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
      ta.dispatchEvent(new KeyboardEvent('keyup', { key: 'a', code: '', keyCode: 65, bubbles: true, cancelable: true }))
    })
    const sent = await frames(page)
    note('soft open, iOS-style', 'keydown(a, code "") + insertText("a")', sent)
    expect(sent, describeFrames(sent)).toEqual([{ via: 'soft-keyboard', t: 'key_text', text: 'a' }])
  })

  test("the toolbar's sticky Ctrl then a hardware c: the host gets Ctrl+C once, and the modifier is cleared", async ({ page }) => {
    // The only chord mechanism a phone has: `consumeModBits` in the
    // component's `input` handler. A listener that owned printable keys would
    // type the "c" and leave Ctrl armed for the next toolbar key.
    await openSoftKeyboard(page)
    const ctrl = page.locator('.mkb-btn', { hasText: 'Ctrl' })
    await ctrl.tap()
    await expect(ctrl).toHaveClass(/mkb-btn-mod-active/)
    await expect(page.locator(CAPTURE)).toBeFocused()
    await page.keyboard.press('c')
    const sent = await frames(page)
    note('soft open, toolbar Ctrl armed', 'c', sent)
    expect(sent, describeFrames(sent)).toEqual([
      { via: 'soft-keyboard', t: 'key', code: 0x06, down: true, mods: 0x01 },
      { via: 'soft-keyboard', t: 'key', code: 0x06, down: false, mods: 0x01 },
    ])
    await expect(ctrl).not.toHaveClass(/mkb-btn-mod-active/)
    // Sticky-once: the next letter is plain text.
    await page.evaluate(() => window.__harness.reset())
    await page.keyboard.press('a')
    const next = await frames(page)
    expect(next, describeFrames(next)).toEqual([{ via: 'soft-keyboard', t: 'key_text', text: 'a' }])
  })

  test("a dead key (´ then e): the dead keydown keeps its default and sends no HID frame; the composed é arrives once", async ({ page }) => {
    // On a viewer whose browser composes dead keys (macOS, Linux, iPadOS — the
    // tablet case) a prevented dead keydown kills the composition. The keydown
    // is dispatched as the browser would; what the OS then does on such a
    // viewer — compose ´ with the next key — is driven through CDP, since no
    // headless Chromium composes on its own.
    await openSoftKeyboard(page)
    const prevented = await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
      const kd = new KeyboardEvent('keydown', { key: 'Dead', code: 'Backquote', bubbles: true, cancelable: true })
      ta.dispatchEvent(kd)
      ta.dispatchEvent(new KeyboardEvent('keyup', { key: 'Dead', code: 'Backquote', bubbles: true, cancelable: true }))
      return kd.defaultPrevented
    })
    expect(prevented, 'the dead keydown must keep its default: the browser composes it').toBe(false)
    const cdp = await page.context().newCDPSession(page)
    let viaCdp = true
    try {
      await cdp.send('Input.imeSetComposition', { text: '´', selectionStart: 1, selectionEnd: 1 })
      await cdp.send('Input.insertText', { text: 'é' })
    } catch {
      viaCdp = false
      await page.evaluate(() => {
        const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
        ta.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }))
        ta.value = '´'
        ta.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: '´' }))
        ta.value = 'é'
        ta.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: 'é' }))
        ta.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: 'é' }))
      })
    }
    const sent = await frames(page)
    note('soft open, dead key', viaCdp ? 'Dead(Backquote) + composed é' : 'Dead(Backquote) + synthetic composition', sent)
    // No HID frame for the dead key's position (0x35), nothing from the listener at all.
    expect(sent, describeFrames(sent)).toEqual([{ via: 'soft-keyboard', t: 'key_text', text: 'é' }])
    expect(await page.evaluate(() => window.__harness.captureValue())).toBe('')
  })

  test('an IME composition arrives as one key_text on compositionend', async ({ page }) => {
    await openSoftKeyboard(page)
    const cdp = await page.context().newCDPSession(page)
    let viaCdp = true
    try {
      await cdp.send('Input.imeSetComposition', { text: 'に', selectionStart: 1, selectionEnd: 1 })
      await cdp.send('Input.imeSetComposition', { text: 'にほ', selectionStart: 2, selectionEnd: 2 })
      await cdp.send('Input.insertText', { text: '日本' })
    } catch {
      // No IME in this Chromium: the same events, synthesised on the textarea.
      viaCdp = false
      await page.evaluate(() => {
        const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
        ta.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }))
        ta.value = 'にほ'
        ta.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: 'にほ' }))
        ta.value = '日本'
        ta.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertCompositionText', data: '日本' }))
        ta.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '日本' }))
      })
    }
    const sent = await frames(page)
    note('soft open, IME', viaCdp ? 'imeSetComposition + insertText' : 'synthetic composition', sent)
    test.info().annotations.push({ type: 'ime', description: viaCdp ? 'CDP Input.imeSetComposition' : 'synthetic events' })
    expect(sent, describeFrames(sent)).toEqual([{ via: 'soft-keyboard', t: 'key_text', text: '日本' }])
    expect(await page.evaluate(() => window.__harness.captureValue())).toBe('')
  })

  test('a toolbar key (Esc) is the component\'s alone', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.locator('.mkb-btn', { hasText: 'Esc' }).tap()
    const sent = await frames(page)
    note('soft open, toolbar', 'Esc button', sent)
    expect(sent, describeFrames(sent)).toEqual([
      { via: 'soft-keyboard', t: 'key', code: 0x29, down: true, mods: 0 },
      { via: 'soft-keyboard', t: 'key', code: 0x29, down: false, mods: 0 },
    ])
  })

  test('an Enter with no physical code (the OS keyboard\'s own) has one owner: the component', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
      ta.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Enter', code: '', keyCode: 13, bubbles: true, cancelable: true }),
      )
      ta.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter', code: '', keyCode: 13, bubbles: true, cancelable: true }))
    })
    const sent = await frames(page)
    note('soft open, OS keyboard', 'Enter (code "")', sent)
    expect(sent, describeFrames(sent)).toEqual([
      { via: 'soft-keyboard', t: 'key', code: 0x28, down: true, mods: 0 },
      { via: 'soft-keyboard', t: 'key', code: 0x28, down: false, mods: 0 },
    ])
  })

  // #2909 — the listener maps by `code`, so it drops these; the always-empty
  // textarea deletes nothing, so no `input` event carries them either.
  for (const [key, keyCode, hid] of [
    ['Backspace', 8, 0x2a],
    ['Delete', 46, 0x4c],
  ] as const) {
    test(`a ${key} with no physical code (the OS keyboard's own) has one owner: the component`, async ({ page }) => {
      await openSoftKeyboard(page)
      await page.evaluate(
        ([k, kc]) => {
          const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
          ta.dispatchEvent(new KeyboardEvent('keydown', { key: k, code: '', keyCode: kc, bubbles: true, cancelable: true }))
          ta.dispatchEvent(new KeyboardEvent('keyup', { key: k, code: '', keyCode: kc, bubbles: true, cancelable: true }))
        },
        [key, keyCode] as const,
      )
      const sent = await frames(page)
      note('soft open, OS keyboard', `${key} (code "")`, sent)
      expect(sent, describeFrames(sent)).toEqual([
        { via: 'soft-keyboard', t: 'key', code: hid, down: true, mods: 0 },
        { via: 'soft-keyboard', t: 'key', code: hid, down: false, mods: 0 },
      ])
    })
  }

  test('a Backspace an IME is composing with (keyCode 229) is not the component\'s: nothing reaches the host from the keydown', async ({ page }) => {
    await openSoftKeyboard(page)
    await page.evaluate(() => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')!
      ta.dispatchEvent(new KeyboardEvent('keydown', { key: 'Backspace', code: '', keyCode: 229, bubbles: true, cancelable: true }))
    })
    const sent = await frames(page)
    note('soft open, OS keyboard', 'Backspace (code "", keyCode 229)', sent)
    expect(sent, describeFrames(sent)).toEqual([])
  })
})

test.describe('a files paste has ONE owner, the drawer open or shut (#2910)', () => {
  /** A Ctrl+V over the stage, then the paste event a clipboard of files
   *  raises. Both synthetic: a real Ctrl+V pastes the browser's clipboard,
   *  which a page cannot fill with files — and this keydown has no default
   *  action, so the dispatched paste is the only one. */
  async function ctrlVFilesOverStage(page: Page, files: number, withCtrlV = true) {
    await page.evaluate(
      ([n, ctrlV]) => {
        const stage = document.getElementById('stage')!
        stage.focus()
        if (ctrlV) {
          stage.dispatchEvent(
            new KeyboardEvent('keydown', { key: 'v', code: 'KeyV', ctrlKey: true, bubbles: true, cancelable: true }),
          )
        }
        const dt = new DataTransfer()
        for (let i = 0; i < n; i++) dt.items.add(new File([`file ${i}`], `f${i}.txt`, { type: 'text/plain' }))
        stage.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }))
      },
      [files, withCtrlV] as const,
    )
  }

  test('the drawer open: uploaded once, into the drawer — the deferred Ctrl+V uploads nothing and sends no keystroke', async ({ page }) => {
    await page.evaluate(() => window.__harness.reset({ pointerInside: true, drawerOpen: true }))
    await ctrlVFilesOverStage(page, 2)
    const sent = await frames(page)
    note('drawer open, pointer inside', 'Ctrl+V, 2 files', sent)
    expect(sent, describeFrames(sent)).toEqual([{ via: 'drawer', t: 'upload', files: 2 }])
  })

  test('the drawer shut: uploaded once, by the deferred Ctrl+V — no keystroke', async ({ page }) => {
    await page.evaluate(() => window.__harness.reset({ pointerInside: true, drawerOpen: false }))
    await ctrlVFilesOverStage(page, 2)
    const sent = await frames(page)
    note('drawer shut, pointer inside', 'Ctrl+V, 2 files', sent)
    expect(sent, describeFrames(sent)).toEqual([{ via: 'paste-files', t: 'upload', files: 2 }])
  })

  test("the drawer open, a files paste with no Ctrl+V before it (the browser's own menu): the drawer's alone", async ({ page }) => {
    await page.evaluate(() => window.__harness.reset({ pointerInside: true, drawerOpen: true }))
    await ctrlVFilesOverStage(page, 1, false)
    const sent = await frames(page)
    note('drawer open, no Ctrl+V', 'paste, 1 file', sent)
    expect(sent, describeFrames(sent)).toEqual([{ via: 'drawer', t: 'upload', files: 1 }])
  })
})

test.describe('the soft keyboard closed', () => {
  for (const focus of ['stage', 'body'] as const) {
    test(`hardware keys with the ${focus} focused reach the host once`, async ({ page }) => {
      await page.evaluate((f) => {
        window.__harness.reset()
        if (f === 'stage') window.__harness.focusStage()
        else window.__harness.focusBody()
      }, focus)
      for (const k of ['a', 'Enter', 'Tab', 'Backspace']) await page.keyboard.press(k)
      const sent = await frames(page)
      note(`soft closed, ${focus} focused`, 'a Enter Tab Backspace', sent)
      await expectOneOwner(page, sent, CLOSED_EXPECTED, `closed, ${focus}`)
    })
  }

  test('keys typed into the files drawer\'s path field stay there (#2869)', async ({ page }) => {
    await page.evaluate(() => {
      window.__harness.reset()
      window.__harness.focusPath()
    })
    await page.keyboard.type('ab')
    await page.keyboard.press('Enter')
    const sent = await frames(page)
    note('path field focused', 'a b Enter', sent)
    expect(sent, describeFrames(sent)).toEqual([])
    expect(await page.locator('#path').inputValue()).toBe('ab')
  })
})
