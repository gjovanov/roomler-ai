// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * #3582 — the build names itself, once, in the two places a tab compares.
 *
 * The id is a hash of the names of the files the build emits. Vite names every
 * chunk, worker and asset after a hash of its own content, so the set of names
 * changes whenever the bundle's code, styles or assets do: a comment, a test or
 * a server-only change leaves it alone, and no tab reloads for those. Not seen:
 * a file copied as it is from `ui/public/`, and an edit to index.html alone;
 * the viewer's code is in neither.
 *
 * One value, computed once per build from the bundle (`idOf`), goes to:
 * - `version.json` beside the bundle (`generateBundle`), which nginx serves
 *   uncached from `location /` (`files/nginx-pod.conf`);
 * - `<meta name="roomler-build">` in index.html (`transformIndexHtml`, post:
 *   Vite runs it while it writes the shell, with the same bundle).
 *
 * Not a `define`: a constant baked into the JavaScript has to exist before the
 * bundle does, so it could not be a hash of the bundle, and anything that
 * changed it per commit (the git SHA, which this stage of the image build does
 * not have anyway) would rename every chunk at every promote. The shell is
 * served `no-cache` and names its own chunks, so the id in it always belongs to
 * the code that page runs. The reader is `ui/src/utils/buildId.ts`.
 */
import { createHash } from 'node:crypto'
import type { Plugin } from 'vite'
import { BUILD_META_NAME, VERSION_FILE } from './src/utils/buildId'

/** The files whose names carry no content hash: the shells, the id file itself, Vite's metadata. */
function namesContent(fileName: string): boolean {
  return !fileName.endsWith('.html') && fileName !== VERSION_FILE && !fileName.startsWith('.vite/')
}

/** The id of a build: 16 hex characters of a SHA-256 over its sorted file names. */
export function buildIdOf(fileNames: Iterable<string>): string {
  const names = [...fileNames].filter(namesContent).sort()
  const h = createHash('sha256')
  for (const n of names) h.update(n).update('\n')
  return h.digest('hex').slice(0, 16)
}

export function buildIdPlugin(): Plugin {
  let id: string | null = null
  // Whichever hook runs first computes it; the other reuses it, so the file
  // and the shell can never disagree within one build.
  const idOf = (bundle: Record<string, unknown>) => (id ??= buildIdOf(Object.keys(bundle)))
  return {
    name: 'roomler:build-id',
    apply: 'build',
    buildStart() {
      id = null
    },
    generateBundle: {
      order: 'pre',
      handler(_options, bundle) {
        this.emitFile({
          type: 'asset',
          fileName: VERSION_FILE,
          source: `${JSON.stringify({ build: idOf(bundle) })}\n`,
        })
      },
    },
    transformIndexHtml: {
      order: 'post',
      handler(_html, ctx) {
        if (!ctx.bundle) return
        return [{ tag: 'meta', attrs: { name: BUILD_META_NAME, content: idOf(ctx.bundle) }, injectTo: 'head' }]
      },
    },
  }
}
