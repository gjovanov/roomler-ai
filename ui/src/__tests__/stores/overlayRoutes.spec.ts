import { describe, expect, it } from 'vitest'
import { deriveOverlayV6 } from '@/stores/overlayRoutes'

// The TS mirror of `overlay/router.rs::derive_overlay_v6` — its output must
// match Rust's `Ipv6Addr` Display exactly (the Rust side pins
// `derive_overlay_v6(100.64.3.129) == "fd72:6f6f:6d6c::6440:381"` in a test).
describe('deriveOverlayV6', () => {
  it('embeds the overlay v4 in the ULA /96 like the Rust derivation', () => {
    expect(deriveOverlayV6('100.64.3.129')).toBe('fd72:6f6f:6d6c::6440:381')
    expect(deriveOverlayV6('100.64.0.2')).toBe('fd72:6f6f:6d6c::6440:2')
    expect(deriveOverlayV6('100.127.255.255')).toBe('fd72:6f6f:6d6c::647f:ffff')
  })

  it('folds a zero high segment into the :: (Rust Display parity)', () => {
    expect(deriveOverlayV6('0.0.0.9')).toBe('fd72:6f6f:6d6c::9')
    expect(deriveOverlayV6('0.0.0.0')).toBe('fd72:6f6f:6d6c::')
  })

  it('rejects malformed input', () => {
    expect(deriveOverlayV6('')).toBeNull()
    expect(deriveOverlayV6('100.64.0')).toBeNull()
    expect(deriveOverlayV6('100.64.0.256')).toBeNull()
    expect(deriveOverlayV6('not-an-ip')).toBeNull()
  })
})
