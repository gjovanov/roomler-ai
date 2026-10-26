// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #2929 — two uploads started together both failed, and nothing arrived: the
// agent keeps ONE active transfer per files channel, and nothing queued the
// viewer's calls. `createUploadQueue` is the queue `uploadOne` takes a place
// in. The fake agent below keeps the agent's rule as the field log showed it:
// a `files:begin` replaces the open transfer, and every chunk lands in
// whichever transfer is open. So the same two uploads, overlapped, fail
// without the queue and complete with it.
import { describe, it, expect } from 'vitest'
import { createUploadQueue, type UploadPlace } from '@/composables/useRemoteControl'

type Transfer = { id: string; size: number; received: number }

/** The agent's one-transfer rule. */
function fakeAgent() {
  const wire: string[] = []
  const failures: string[] = []
  const completed: string[] = []
  let active: Transfer | null = null
  return {
    wire,
    failures,
    completed,
    begin(id: string, size: number) {
      wire.push(`begin ${id}`)
      active = { id, size, received: 0 }
    },
    chunk(id: string, bytes: number) {
      wire.push(`chunk ${id}`)
      if (!active) return void failures.push(`chunk ${id}: no active transfer`)
      active.received += bytes
      if (active.received > active.size) {
        failures.push(`chunk failed id=${active.id}: received ${active.received} bytes, expected ${active.size}`)
        active = null
      }
    },
    end(id: string) {
      wire.push(`end ${id}`)
      if (!active) return void failures.push(`end failed id=${id}: no active transfer to end`)
      if (active.id !== id) return void failures.push(`end failed id=${id}: active transfer is ${active.id}`)
      if (active.received === active.size) completed.push(id)
      active = null
    },
  }
}

const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0))

/** One 42-byte upload, as `uploadOne` runs it: wait for its turn, begin, one
 *  chunk, end, and call `done` when it has settled. */
async function upload(agent: ReturnType<typeof fakeAgent>, id: string, place?: UploadPlace) {
  try {
    if (place) await place.turn
    agent.begin(id, 42)
    await tick()
    agent.chunk(id, 42)
    await tick()
    agent.end(id)
  } finally {
    place?.done()
  }
}

describe('createUploadQueue (#2929)', () => {
  it('without it, two overlapping uploads both fail, as in the field', async () => {
    const agent = fakeAgent()
    await Promise.all([upload(agent, 'A'), upload(agent, 'B')])
    expect(agent.wire.slice(0, 2)).toEqual(['begin A', 'begin B'])
    expect(agent.completed).toEqual([])
    expect(agent.failures).toContain('chunk failed id=B: received 84 bytes, expected 42')
  })

  it('with it, the second upload begins only after the first has ended, and both complete', async () => {
    const agent = fakeAgent()
    const take = createUploadQueue()
    await Promise.all([upload(agent, 'A', take()), upload(agent, 'B', take())])
    expect(agent.wire).toEqual(['begin A', 'chunk A', 'end A', 'begin B', 'chunk B', 'end B'])
    expect(agent.completed).toEqual(['A', 'B'])
    expect(agent.failures).toEqual([])
  })

  it('the first upload goes at once', async () => {
    const take = createUploadQueue()
    let started = false
    await take().turn.then(() => {
      started = true
    })
    expect(started).toBe(true)
  })

  it('a failed or cancelled upload still lets the next one go', async () => {
    const take = createUploadQueue()
    const a = take()
    const b = take()
    let bStarted = false
    const bTurn = b.turn.then(() => {
      bStarted = true
    })
    await tick()
    expect(bStarted).toBe(false)
    // `uploadOne` calls `done` on a reject as on a resolve.
    a.done()
    await bTurn
    expect(bStarted).toBe(true)
  })

  it('an upload cancelled while queued does not let the one behind it jump an upload still on the wire', async () => {
    const take = createUploadQueue()
    const a = take() // on the wire
    const b = take() // queued
    const c = take() // queued behind b
    b.done() // b cancelled while queued: settled before its turn came
    let cStarted = false
    const cTurn = c.turn.then(() => {
      cStarted = true
    })
    await tick()
    await tick()
    expect(cStarted).toBe(false)
    a.done()
    await cTurn
    expect(cStarted).toBe(true)
  })

  it('three overlapping uploads keep their order', async () => {
    const agent = fakeAgent()
    const take = createUploadQueue()
    await Promise.all(['A', 'B', 'C'].map((id) => upload(agent, id, take())))
    expect(agent.wire.filter((w) => w.startsWith('begin'))).toEqual(['begin A', 'begin B', 'begin C'])
    expect(agent.completed).toEqual(['A', 'B', 'C'])
    expect(agent.failures).toEqual([])
  })
})
