// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #4257 — Shape 2 sites in files too large to mount (the remote-control view,
// the app shell, the Devices page). Each must ask its store whether it holds
// THIS org's rows, never whether its list is empty: a list that holds another
// org's rows is not empty. The store functions they call are tested in the
// stores' own specs (agents, rooms).
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Vitest runs from `ui/` (where its config lives). A checkout may hold Windows
// line ends; they are folded so the blocks below end where the code does.
const read = (path: string) =>
  readFileSync(resolve(process.cwd(), path), 'utf8').replace(/\r\n/g, '\n')

/** The text from `head` to the first `close` after it. */
function block(src: string, head: string, close: string): string {
  const start = src.indexOf(head)
  if (start < 0) throw new Error(`not found: ${head}`)
  const end = src.indexOf(close, start)
  if (end < 0) throw new Error(`no end for: ${head}`)
  return src.slice(start, end + close.length)
}

describe('Shape 2 sites in the shell, the remote-control view and the Devices page (#4257)', () => {
  it("RemoteControl's loadAgent asks the device store for this org's devices, not for any", () => {
    const body = block(read('src/views/remote/RemoteControl.vue'), 'async function loadAgent()', '\n}\n')
    expect(body).toContain('await agentStore.ensureAgents(tenantId.value)')
    expect(body).not.toMatch(/agents\.length/)
  })

  it("the welcome tour's check waits for this org's rooms, not for any rooms", () => {
    const body = block(
      read('src/components/layout/AppLayout.vue'),
      'async function maybeAutoOpenTour()',
      '\n}\n',
    )
    expect(body).toContain('roomStore.ensureRooms(tid)')
    expect(body).not.toContain('roomStore.rooms.length ||')
    expect(body).not.toContain('roomStore.fetchRooms(')
  })

  it("the Devices page reloads the org's members with its devices when the org changes", () => {
    const body = block(
      read('src/components/admin/AgentsSection.vue'),
      'watch(() => props.tenantId, (tid) => {',
      '\n})\n',
    )
    expect(body).toContain('agentStore.fetchTenantMembers(tid)')
  })

  // The review's F5: a superseded rooms fetch settles with the newest one, and
  // the shell's count chain had no catch, so a failed newest fetch was an
  // unhandled rejection; a reconnect and the mount awaited the fetch the same
  // way. Each handles a failed fetch. (The count runs over its own org's rooms
  // only: the rooms store's spec.)
  it("the shell's rooms fetches handle a failed fetch: on a change of org, on a reconnect and on mount", () => {
    const src = read('src/components/layout/AppLayout.vue')
    const onChange = block(
      src,
      "if (tid && prevTid !== undefined && prevTid !== tid && caps.has('chat')) {",
      '\n    }\n',
    )
    expect(onChange).toContain('roomStore.fetchAllUnreadCounts(tid)')
    expect(onChange).toContain('.catch(')
    const reconnect = block(src, 'async function onWsReconnected()', '\n}\n')
    expect(reconnect).toMatch(/try \{\s*await roomStore\.fetchRooms\(/)
    const mounted = block(src, 'onMounted(async () => {', '\n})\n')
    expect(mounted).toMatch(/try \{\s*await roomStore\.fetchRooms\(/)
  })
})
