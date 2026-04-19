import type { Agent } from '@/stores/agents'

export interface CodecChip {
  label: string
  color: string
  tooltip: string
}

/**
 * Render the agent's advertised codec capabilities as a list of chips
 * (e.g. "H.264 HW", "H.265 HW", "AV1 SW"). Combines `codecs` (the
 * codec names) with `hw_encoders` (the backend labels) so the chip
 * shows both what's supported and how. Pure function — exported as
 * a standalone module so vitest can cover it without mounting Vue.
 */
export function codecChips(a: Agent): CodecChip[] {
  const caps = a.capabilities
  if (!caps) return []
  // Group hw_encoders by codec stem ("h264", "h265", "av1") so we can
  // tell HW from SW. "openh264-sw" → h264 SW; "mf-h264-hw" → h264 HW;
  // "mf-h265-hw" → h265 HW; etc.
  const hwForCodec = new Map<string, boolean>()
  for (const enc of caps.hw_encoders ?? []) {
    const lower = enc.toLowerCase()
    const isHw = lower.includes('-hw')
    for (const codec of ['h264', 'h265', 'av1', 'vp9', 'vp8'] as const) {
      if (lower.includes(codec)) {
        hwForCodec.set(codec, hwForCodec.get(codec) || isHw)
      }
    }
  }
  return (caps.codecs ?? []).map((codec) => {
    const lower = codec.toLowerCase()
    const isHw = hwForCodec.get(lower) ?? false
    const display = lower
      .replace(/^h(\d{3})$/, (_m, n) => `H.${n}`)
      .toUpperCase()
    return {
      label: `${display} ${isHw ? 'HW' : 'SW'}`,
      color: isHw ? 'primary' : 'default',
      tooltip: isHw
        ? `Hardware-accelerated ${display} encoder available`
        : `Software ${display} encoder available`,
    }
  })
}
