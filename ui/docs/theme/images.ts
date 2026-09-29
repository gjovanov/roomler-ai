// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — image dimensions, read from the file itself.
 *
 * An `<img>` without `width`/`height` reserves no space, so the text under it
 * jumps when it loads (Cumulative Layout Shift, a Core Web Vital). FR-60
 * hard-coded 960×420 on every hero while the heroes are 760×400 or 600×540,
 * which reserves the WRONG space: a jump on every hero page.
 *
 * The five formats the site uses are parsed by hand from their headers: a few
 * fixed offsets each, where `image-size`/`sharp` would be a dependency for ten
 * lines of arithmetic. An unrecognised or damaged file is an ERROR, because a
 * guessed size is the bug this file exists to remove.
 */

export interface ImageSize {
  width: number
  height: number
}

/** Refused at build time. The site serves images as they are (no resizing
 *  pipeline), so a camera original would ship to every phone at full weight. */
export const MAX_IMAGE_BYTES = 3 * 1024 * 1024

function ascii(b: Uint8Array, at: number, len: number): string {
  return String.fromCharCode(...b.subarray(at, at + len))
}

const PNG_SIGNATURE = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]

function png(b: Uint8Array, v: DataView): ImageSize | null {
  if (b.length < 8 || !PNG_SIGNATURE.every((x, i) => b[i] === x)) return null
  // IHDR is required to be the first chunk: length(4) type(4) width(4) height(4).
  if (b.length < 24 || ascii(b, 12, 4) !== 'IHDR') throw new Error('PNG whose first chunk is not IHDR')
  return { width: v.getUint32(16), height: v.getUint32(20) }
}

function gif(b: Uint8Array, v: DataView): ImageSize | null {
  const sig = ascii(b, 0, 6)
  if (sig !== 'GIF87a' && sig !== 'GIF89a') return null
  if (b.length < 10) throw new Error('GIF truncated before its screen descriptor')
  return { width: v.getUint16(6, true), height: v.getUint16(8, true) }
}

/** SOF0–SOF15 carry the frame size; C4 (DHT), C8 (JPG) and CC (DAC) share
 *  the range but are not frame headers. */
function isSof(marker: number): boolean {
  return marker >= 0xc0 && marker <= 0xcf && marker !== 0xc4 && marker !== 0xc8 && marker !== 0xcc
}

function jpeg(b: Uint8Array, v: DataView): ImageSize | null {
  if (b.length < 4 || b[0] !== 0xff || b[1] !== 0xd8) return null
  let i = 2
  while (i + 3 < b.length) {
    if (b[i] !== 0xff) throw new Error(`JPEG: expected a marker at byte ${i}`)
    const marker = b[i + 1]!
    if (marker === 0xff) {
      i += 1 // fill byte
      continue
    }
    // Standalone markers carry no length.
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd8)) {
      i += 2
      continue
    }
    if (marker === 0xd9 || marker === 0xda) break // EOI / start of scan: no SOF came first
    if (isSof(marker)) {
      if (i + 9 > b.length) break
      // FF Cn | length(2) | precision(1) | height(2) | width(2)
      return { height: v.getUint16(i + 5), width: v.getUint16(i + 7) }
    }
    i += 2 + v.getUint16(i + 2)
  }
  throw new Error('JPEG with no frame header before its image data')
}

function webp(b: Uint8Array, v: DataView): ImageSize | null {
  if (b.length < 16 || ascii(b, 0, 4) !== 'RIFF' || ascii(b, 8, 4) !== 'WEBP') return null
  const chunk = ascii(b, 12, 4)
  if (b.length < 30) throw new Error('WebP truncated before its frame header')
  if (chunk === 'VP8 ') {
    // Lossy: a 3-byte frame tag, the start code 9D 01 2A, then 14-bit sizes.
    if (b[23] !== 0x9d || b[24] !== 0x01 || b[25] !== 0x2a) throw new Error('WebP (VP8): bad start code')
    return { width: v.getUint16(26, true) & 0x3fff, height: v.getUint16(28, true) & 0x3fff }
  }
  if (chunk === 'VP8L') {
    // Lossless: signature 0x2F, then width-1 and height-1 as 14-bit fields,
    // packed least-significant bit first.
    if (b[20] !== 0x2f) throw new Error('WebP (VP8L): bad signature')
    const bits = v.getUint32(21, true)
    return { width: (bits & 0x3fff) + 1, height: ((bits >>> 14) & 0x3fff) + 1 }
  }
  if (chunk === 'VP8X') {
    // Extended: flags(1) reserved(3), then canvas width-1 and height-1, 24-bit LE.
    const w = b[24]! | (b[25]! << 8) | (b[26]! << 16)
    const h = b[27]! | (b[28]! << 8) | (b[29]! << 16)
    return { width: w + 1, height: h + 1 }
  }
  throw new Error(`WebP whose first chunk is "${chunk}"`)
}

/** A length in user units: `760`, `760px`. Anything relative (`100%`, `2em`)
 *  says nothing about the drawing's own size, so it is not a length here. */
function svgLength(raw: string | undefined): number | undefined {
  const m = /^\s*(\d+(?:\.\d+)?)\s*(?:px)?\s*$/.exec(raw ?? '')
  return m ? Number(m[1]) : undefined
}

function svg(b: Uint8Array): ImageSize | null {
  const text = new TextDecoder().decode(b)
  const root = /<svg\b[^>]*>/i.exec(text)
  if (!root) return null
  // `\s` before the name, so `stroke-width` never reads as `width`.
  const attr = (name: string) =>
    new RegExp(`\\s${name}\\s*=\\s*(?:"([^"]*)"|'([^']*)')`, 'i').exec(root[0])?.slice(1).find((x) => x !== undefined)

  let width = svgLength(attr('width'))
  let height = svgLength(attr('height'))
  if (width === undefined || height === undefined) {
    const box = (attr('viewBox') ?? '').trim().split(/[\s,]+/).map(Number)
    const [, , bw, bh] = box
    if (box.length === 4 && box.every(Number.isFinite) && bw! > 0 && bh! > 0) {
      if (width === undefined && height === undefined) {
        width = bw
        height = bh
      } else if (width === undefined) {
        width = (height! * bw!) / bh!
      } else {
        height = (width * bh!) / bw!
      }
    }
  }
  if (!width || !height) throw new Error('SVG with neither a numeric width and height nor a viewBox')
  return { width: Math.round(width), height: Math.round(height) }
}

/** Intrinsic size of a PNG, GIF, JPEG, WebP or SVG. Throws on anything else. */
export function imageSize(bytes: Uint8Array): ImageSize {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
  const size = png(bytes, v) ?? gif(bytes, v) ?? jpeg(bytes, v) ?? webp(bytes, v) ?? svg(bytes)
  if (!size) throw new Error('not an image this site knows how to size (PNG, GIF, JPEG, WebP or SVG)')
  if (!(size.width > 0 && size.height > 0)) throw new Error(`reads as ${size.width}×${size.height}`)
  return size
}
