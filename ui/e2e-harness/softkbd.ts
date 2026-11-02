// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2888 — the soft-keyboard harness: the viewer's key path in a REAL browser.
//
// The composable cannot be instantiated outside the app (it needs the WS
// store, a peer connection and an open input data channel), and jsdom has no
// default actions: a keydown there never inserts a character, so the `input`
// event the soft keyboard types through never fires, and the second owner of
// a key is invisible. This page mounts the two things that matter and
// nothing else:
//
//   - the real `MobileKeyboard.vue` — the off-screen textarea and its toolbar
//     — with its `key` / `keyText` emits recorded as the frames `sendKey` /
//     `sendKeyText` would send;
//   - a window-level CAPTURE key listener that runs the real `routeKeyEvent`
//     and `applyKeyRoute`, exactly as `onKey` does, with effects that record
//     the frames `sendInput` would send;
//   - the viewer's TWO window `paste` listeners in the viewer's order (#2910):
//     the files drawer's first (the real `drawerPasteFiles`, registered at
//     mount in `RemoteControl.vue`), then the deferred Ctrl+V's `onPaste` (the
//     real `routeDeferredPaste`, registered later by `attachInput`), each
//     recording the uploads it would start.
//
// Playwright (`softkbd.spec.ts`) presses real keys, types through
// `insertText` (the soft keyboard's own path), composes through the IME and
// pastes, and counts what reached the "host" per key and per state. The
// recorder keeps WHO sent each frame, so a key with two owners is a finding,
// not a count that happens to add up.
//
// `window.__harness` is the spec's handle: the recorded frames, the viewer
// state it can set (pointer inside, keyboard lock, Ctrl→Cmd), open / close.
import { createApp, defineComponent, h, onMounted, ref } from 'vue'
import MobileKeyboard from '@/components/remote/MobileKeyboard.vue'
import {
  applyKeyRoute,
  createHeldInputTracker,
  DEFERRED_PASTE_FALLBACK_MS,
  deferredPasteFrames,
  drawerPasteFiles,
  routeDeferredPaste,
  routeKeyEvent,
  translateModifierForHost,
  type KeyRouteEffects,
} from '@/composables/useRemoteControl'

/** Who produced a frame: the viewer's window listener, the soft keyboard
 *  component's own emits, or the deferred paste's flush — and, for an upload,
 *  the files drawer's paste handler or the deferred Ctrl+V's `onPaste`. */
export type FrameVia = 'listener' | 'soft-keyboard' | 'paste-flush' | 'drawer' | 'paste-files'

export type HostFrame =
  | { via: FrameVia; t: 'key'; code: number; down: boolean; mods: number }
  | { via: FrameVia; t: 'key_text'; text: string }
  | { via: 'listener'; t: 'sas' }
  | { via: 'listener'; t: 'clipboard-mirror' }
  | { via: 'listener'; t: 'release-held'; keys: number[] }
  | { via: 'paste-flush'; t: 'paste'; text: string; files: number }
  | { via: 'drawer' | 'paste-files'; t: 'upload'; files: number }

export interface HarnessState {
  pointerInside: boolean
  keyboardLocked: boolean
  keyboardPaused: boolean
  /** `hostIsMac && ctrlAsCmd` — FR-13's Ctrl→Cmd substitution. */
  macCtrlAsCmd: boolean
  /** The viewer's Remote files drawer is open (#2910). */
  drawerOpen: boolean
}

export interface Harness {
  sent: HostFrame[]
  state: HarnessState
  /** Clear the record (and any pending deferral); optionally set state. */
  reset(next?: Partial<HarnessState>): void
  /** Open the soft keyboard: the component focuses its textarea. */
  open(): void
  close(): void
  /** `input` events the capture textarea raised since the last reset. */
  inputEvents(): number
  /** The capture textarea's current value (always empty when healthy). */
  captureValue(): string
  focusStage(): void
  focusPath(): void
  focusBody(): void
}

declare global {
  interface Window {
    __harness: Harness
  }
}

const sent: HostFrame[] = []
const state: HarnessState = {
  pointerInside: false,
  keyboardLocked: false,
  keyboardPaused: false,
  macCtrlAsCmd: false,
  drawerOpen: false,
}
const ctrlSubState = { ctrlHeldAsCmd: false }
const heldInputs = createHeldInputTracker()
let pendingCtrlV: { mods: number; timer: ReturnType<typeof setTimeout> | null } | null = null
let inputEvents = 0
let stageEl: HTMLElement | null = null
const open = ref(false)

/** The deferral's state is `attachInput`'s own (a stash and a fallback timer);
 *  what it SENDS and what the paste event DECIDES are the composable's exported
 *  seams, used here as production uses them. */
function flushPendingCtrlV() {
  if (!pendingCtrlV) return
  const mods = pendingCtrlV.mods
  if (pendingCtrlV.timer) clearTimeout(pendingCtrlV.timer)
  pendingCtrlV = null
  for (const frame of deferredPasteFrames(mods)) sent.push({ via: 'paste-flush', ...frame } as HostFrame)
}

/** The effects `onKey` gives `applyKeyRoute`, recording instead of sending. */
const keyEffects: KeyRouteEffects = {
  send: (msg) => sent.push({ via: 'listener', ...msg } as HostFrame),
  hostKeyCode: (code, down) =>
    translateModifierForHost(code, down, state.macCtrlAsCmd, ctrlSubState),
  held: (code, down) => heldInputs.key(code, down),
  heldCount: () => heldInputs.size(),
  releaseHeld: () => {
    const held = heldInputs.releaseAll()
    sent.push({ via: 'listener', t: 'release-held', keys: held.keys })
    for (const code of held.keys) {
      sent.push({ via: 'listener', t: 'key', code, down: false, mods: 0 })
    }
  },
  sendCtrlAltDel: () => sent.push({ via: 'listener', t: 'sas' }),
  deferPaste(mods) {
    if (pendingCtrlV?.timer) clearTimeout(pendingCtrlV.timer)
    const timer = setTimeout(() => {
      flushPendingCtrlV()
    }, DEFERRED_PASTE_FALLBACK_MS)
    pendingCtrlV = { mods, timer }
  },
  mirrorClipboard: () => sent.push({ via: 'listener', t: 'clipboard-mirror' }),
}

function onKey(ev: KeyboardEvent, down: boolean) {
  applyKeyRoute(
    routeKeyEvent(
      ev,
      down,
      {
        pointerInside: state.pointerInside,
        keyboardLocked: state.keyboardLocked,
        keyboardPaused: state.keyboardPaused,
        surface: stageEl,
      },
      (k) => ev.getModifierState(k),
    ),
    ev,
    keyEffects,
  )
}

/** `onPaste` minus the clipboard channel: the SAME `routeDeferredPaste`
 *  decides (and consumes the event when the deferral has it — the one
 *  `preventDefault`, in the composable, not a copy here). Files go to the
 *  upload sink (`onFilesPasted`) and a paste the drawer already took is
 *  dropped — no keystroke either way, as in the composable; otherwise the
 *  deferred keystroke is flushed. */
function onPaste(ev: ClipboardEvent) {
  const route = routeDeferredPaste(ev, pendingCtrlV !== null, true)
  if (route.kind === 'ignore') return
  if (route.kind === 'taken' || route.kind === 'files') {
    if (pendingCtrlV?.timer) clearTimeout(pendingCtrlV.timer)
    pendingCtrlV = null
    if (route.kind === 'files') sent.push({ via: 'paste-files', t: 'upload', files: route.files.length })
    return
  }
  if (route.kind === 'content') {
    sent.push({ via: 'paste-flush', t: 'paste', text: route.text, files: 0 })
  }
  flushPendingCtrlV()
}

/** The files drawer's window `paste` handler (`onWindowPasteForDrawer`): the
 *  SAME `drawerPasteFiles` decides; it consumes what it uploads. */
function onDrawerPaste(ev: ClipboardEvent) {
  const files = drawerPasteFiles(ev, state.drawerOpen)
  if (!files) return
  ev.preventDefault()
  ev.stopPropagation()
  sent.push({ via: 'drawer', t: 'upload', files: files.length })
}

const HarnessApp = defineComponent({
  name: 'SoftKeyboardHarness',
  setup() {
    const stage = ref<HTMLElement | null>(null)
    onMounted(() => {
      stageEl = stage.value
    })
    return () =>
      h('div', { id: 'harness' }, [
        h(
          'div',
          {
            id: 'stage',
            ref: stage,
            tabindex: -1,
            style: 'width: 320px; height: 180px; background: #222; color: #ccc; margin: 8px;',
          },
          'stage',
        ),
        // The files drawer's path field — one of the viewer's OWN inputs (#2869).
        h('input', { id: 'path', type: 'text', placeholder: 'Path (Enter to go)' }),
        h(MobileKeyboard, {
          open: open.value,
          onClose: () => {
            open.value = false
          },
          onKey: (code: number, down: boolean, mods: number) =>
            sent.push({ via: 'soft-keyboard', t: 'key', code, down, mods }),
          onKeyText: (text: string) => sent.push({ via: 'soft-keyboard', t: 'key_text', text }),
        }),
      ])
  },
})

createApp(HarnessApp).mount('#app')

// The drawer's paste handler first: `RemoteControl.vue` registers it at mount,
// before a session connects and `attachInput` registers its own (#2910).
window.addEventListener('paste', onDrawerPaste)
// As `attachInput` registers them: window, CAPTURE phase; paste on the window.
window.addEventListener('keydown', (e) => onKey(e, true), { capture: true })
window.addEventListener('keyup', (e) => onKey(e, false), { capture: true })
window.addEventListener('paste', onPaste)
document.addEventListener(
  'input',
  (e) => {
    if ((e.target as Element | null)?.closest?.('[data-rc-capture]')) inputEvents++
  },
  true,
)

const captureTextarea = () =>
  document.querySelector<HTMLTextAreaElement>('textarea[data-rc-capture]')

window.__harness = {
  sent,
  state,
  reset(next) {
    sent.length = 0
    inputEvents = 0
    if (pendingCtrlV?.timer) clearTimeout(pendingCtrlV.timer)
    pendingCtrlV = null
    ctrlSubState.ctrlHeldAsCmd = false
    heldInputs.releaseAll()
    Object.assign(state, next ?? {})
  },
  open() {
    open.value = true
  },
  close() {
    open.value = false
  },
  inputEvents: () => inputEvents,
  captureValue: () => captureTextarea()?.value ?? '',
  focusStage() {
    stageEl?.focus()
  },
  focusPath() {
    document.getElementById('path')?.focus()
  },
  focusBody() {
    ;(document.activeElement as HTMLElement | null)?.blur()
  },
}
