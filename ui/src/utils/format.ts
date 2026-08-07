// Shared display formatters for the stats surfaces.
//
// `null` is deliberately distinct from `0` throughout: a class whose bytes
// were never measured must render an em dash, not a confident zero. The
// server signals this with `bytes_known`, and these helpers keep the
// distinction visible rather than coercing it away.

export function formatBytes(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—'
  if (v >= 1_073_741_824) return `${(v / 1_073_741_824).toFixed(1)} GiB`
  if (v >= 1_048_576) return `${(v / 1_048_576).toFixed(1)} MiB`
  if (v >= 1024) return `${(v / 1024).toFixed(1)} KiB`
  return `${Math.round(v)} B`
}

/** Minutes → `2h 15m` / `45m` / `38s`, so short sessions stay legible. */
export function formatMinutes(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v)) return '—'
  if (v <= 0) return '0m'
  if (v < 1) return `${Math.round(v * 60)}s`
  const h = Math.floor(v / 60)
  const m = Math.round(v % 60)
  return h > 0 ? `${h}h ${m}m` : `${m}m`
}

/** Seconds → the same shape as `formatMinutes`. */
export function formatDuration(secs: number | null | undefined): string {
  if (secs === null || secs === undefined || !Number.isFinite(secs)) return '—'
  return formatMinutes(secs / 60)
}
