import { describe, it, expect } from 'vitest'
import { asSentence } from '@/utils/sentence'

describe('asSentence', () => {
  // The words a 0.4.131 agent sends on a KDE Wayland desktop (input/reach.rs),
  // shown after the banner's "…do not reach this device's desktop."
  it('starts a lower-case reason with a capital and ends it with a full stop', () => {
    expect(asSentence('this is a Wayland desktop whose Xwayland sends XTest through KWin')).toBe(
      'This is a Wayland desktop whose Xwayland sends XTest through KWin.',
    )
  })

  it('leaves a finished sentence as it is', () => {
    expect(asSentence('XTest reaches no window at all.')).toBe('XTest reaches no window at all.')
    expect(asSentence('Is anyone at the screen?')).toBe('Is anyone at the screen?')
    expect(asSentence('Asked the person at the screen…')).toBe('Asked the person at the screen…')
  })

  it('keeps the inner words and trims the edges', () => {
    expect(asSentence('  no X display, no uinput. Set drm_capture  ')).toBe('No X display, no uinput. Set drm_capture.')
  })

  it('shows nothing for no reason', () => {
    expect(asSentence('')).toBe('')
    expect(asSentence('   ')).toBe('')
  })
})
