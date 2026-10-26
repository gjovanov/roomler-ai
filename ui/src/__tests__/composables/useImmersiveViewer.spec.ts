// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// FR-125 P1 — immersive landscape on phones: when the mode applies, the setting
// that switches it off, and the one flag the app shell reads.
import { describe, it, expect, afterEach } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import {
  IMMERSIVE_MAX_HEIGHT,
  IMMERSIVE_STORAGE_KEY,
  immersiveViewer,
  isPhoneSideways,
  readImmersiveSetting,
  setImmersiveViewer,
  writeImmersiveSetting,
} from '@/composables/useImmersiveViewer'

/** A storage double: a Map behind the three methods the helpers call. */
function memoryStorage() {
  const m = new Map<string, string>()
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  }
}
const throwing = {
  getItem: () => {
    throw new Error('SecurityError')
  },
  setItem: () => {
    throw new Error('SecurityError')
  },
  removeItem: () => {
    throw new Error('SecurityError')
  },
}

describe('isPhoneSideways — the pages the mode reaches', () => {
  // The six sizes measured on production (FR-125 §2.1), and the tallest phone
  // held sideways: each one left the remote desktop 146–254 px.
  const phonesSideways: [string, number, number][] = [
    ['iPhone 14', 844, 390],
    ['iPhone 14, with Safari\'s bar', 844, 340],
    ['Pixel 7', 915, 412],
    ['Pixel 7, with Chrome\'s bar', 915, 356],
    ['small Android', 740, 360],
    ['small Android, with its bar', 740, 304],
    ['iPhone Pro Max', 932, 430],
  ]
  for (const [name, w, h] of phonesSideways) {
    it(`${name} (${w} × ${h}) with a finger: immersive`, () => {
      expect(isPhoneSideways(true, w, h)).toBe(true)
    })
  }

  it('the same phones held upright: not immersive', () => {
    for (const [, w, h] of phonesSideways) expect(isPhoneSideways(true, h, w)).toBe(false)
  })

  it('a tablet either way keeps its bars: it has the room', () => {
    expect(isPhoneSideways(true, 768, 1024)).toBe(false)
    expect(isPhoneSideways(true, 1024, 768)).toBe(false)
    expect(isPhoneSideways(true, 1024, 600)).toBe(false)
  })

  it(`the boundary: under ${IMMERSIVE_MAX_HEIGHT} px tall, not at it`, () => {
    expect(isPhoneSideways(true, 900, IMMERSIVE_MAX_HEIGHT - 1)).toBe(true)
    expect(isPhoneSideways(true, 900, IMMERSIVE_MAX_HEIGHT)).toBe(false)
  })

  it('a short desktop window with a mouse: not immersive — a fine pointer never is', () => {
    expect(isPhoneSideways(false, 900, 400)).toBe(false)
    expect(isPhoneSideways(false, 844, 390)).toBe(false)
  })

  it('a square page is not sideways', () => {
    expect(isPhoneSideways(true, 400, 400)).toBe(false)
  })
})

describe('the setting — the kill switch, per browser', () => {
  it('on when this browser never chose', () => {
    expect(readImmersiveSetting(memoryStorage())).toBe(true)
  })

  it('off survives a reload; on again removes the key', () => {
    const s = memoryStorage()
    writeImmersiveSetting(false, s)
    expect(s.m.get(IMMERSIVE_STORAGE_KEY)).toBe('off')
    expect(readImmersiveSetting(s)).toBe(false)
    writeImmersiveSetting(true, s)
    expect(s.m.has(IMMERSIVE_STORAGE_KEY)).toBe(false)
    expect(readImmersiveSetting(s)).toBe(true)
  })

  it('blocked site data reads as on, and a write does not throw', () => {
    expect(readImmersiveSetting(throwing)).toBe(true)
    expect(() => writeImmersiveSetting(false, throwing)).not.toThrow()
    expect(readImmersiveSetting(undefined)).toBe(true)
  })
})

describe('the flag the app shell reads', () => {
  afterEach(() => setImmersiveViewer(false))

  it('follows the viewer, and starts off', () => {
    expect(immersiveViewer.value).toBe(false)
    setImmersiveViewer(true)
    expect(immersiveViewer.value).toBe(true)
    setImmersiveViewer(false)
    expect(immersiveViewer.value).toBe(false)
  })

  it('the app shell hides its bar on it, and the viewer clears it when it unmounts', () => {
    // Vitest runs from `ui/`; the SFCs are read as text: the bar's binding and
    // the unmount reset are what keep every other page's bar.
    const shell = readFileSync(resolve(process.cwd(), 'src/components/layout/AppLayout.vue'), 'utf8')
    expect(shell).toMatch(/<v-app-bar[^>]*:model-value="!immersiveViewer"/)
    const viewer = readFileSync(resolve(process.cwd(), 'src/views/remote/RemoteControl.vue'), 'utf8')
    expect(viewer).toMatch(/onBeforeUnmount\(\(\) => \{[^}]*setImmersiveViewer\(false\)/)
  })
})
