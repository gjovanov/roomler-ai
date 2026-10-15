// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-88 (#1790) P3 — the click-to-load video facade every static page draws: a
 * docs page's `:::video <id>` and the homepage.
 *
 * It is our own poster, a play button and a link to the video on YouTube. With
 * JavaScript, `docs.js` swaps the link for the `youtube-nocookie.com` player on
 * the click; without it, the link opens the video on YouTube. Nothing is
 * requested from any YouTube host before that click.
 *
 * The SPA draws the same thing with `ui/src/components/common/VideoFacade.vue`.
 */
import { clockOf, minutesOf, watchUrl, type SiteVideo } from '../../src/utils/videos.ts'

export interface VideoPoster {
  url: string
  width: number
  height: number
}

function esc(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')
}

/** A play triangle in a circle, drawn in CSS colours (`currentColor`). */
const PLAY =
  '<svg viewBox="0 0 24 24" width="34" height="34" aria-hidden="true" focusable="false">' +
  '<path fill="currentColor" d="M8 5.14v13.72a1 1 0 0 0 1.52.85l10.6-6.86a1 1 0 0 0 0-1.7L9.52 4.29A1 1 0 0 0 8 5.14z"/></svg>'

/**
 * @param caption HTML for the caption (already rendered or escaped); the
 *   video's title when absent.
 */
export function videoFacade(v: SiteVideo, poster: VideoPoster | null, caption?: string): string {
  const label = `Play the video "${v.title}" (${minutesOf(v.duration)} minutes, plays from YouTube)`
  return (
    `<figure class="video-facade">` +
    `<a class="video-facade__link" href="${esc(watchUrl(v.id))}" data-video-id="${esc(v.id)}" ` +
    `data-video-title="${esc(v.title)}" aria-label="${esc(label)}">` +
    (poster
      ? `<img class="video-facade__poster" src="${esc(poster.url)}" width="${poster.width}" height="${poster.height}" ` +
        `alt="" loading="lazy" decoding="async">`
      : '') +
    `<span class="video-facade__play">${PLAY}</span>` +
    `<span class="video-facade__time">${clockOf(v.duration)}</span>` +
    `</a>` +
    `<figcaption class="video-facade__caption">${caption ?? esc(v.title)}` +
    `<span class="video-facade__note">Plays from YouTube when you press play.</span></figcaption>` +
    `</figure>`
  )
}
