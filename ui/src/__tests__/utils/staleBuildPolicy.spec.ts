// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//
// #3582 — a tab that outlives a promote. These lock the rules: when a
// mismatch counts, what the page does about it, and the resume a reload
// hands to the next page load.
import { describe, expect, it } from 'vitest'
import {
  CONFIRM_GAP_MS,
  NO_TRACK,
  RC_RESUME_KEY,
  RC_RESUME_MAX_AGE_MS,
  bannerHint,
  decideStaleBuildAction,
  resumeArgsFor,
  takeRcResumeIntent,
  trackServedBuild,
  writeRcResumeIntent,
  type RcResumeIntent,
  type ServedBuildTrack,
  type StaleBuildSituation,
} from '@/utils/staleBuildPolicy'

const OWN = 'aaaaaaaaaaaaaaaa'
const NEW = 'bbbbbbbbbbbbbbbb'
const NEWER = 'cccccccccccccccc'
const AGENT = '69f0000000000000000000c3'
const TENANT = '69f0000000000000000000a1'
const PATH = `/tenant/${TENANT}/agent/${AGENT}/remote`
const PERMS = 'VIEW | INPUT | CLIPBOARD | FILES | RECORD'

function memoryStorage(seed: Record<string, string> = {}) {
  const m = new Map(Object.entries(seed))
  return {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
    has: (k: string) => m.has(k),
  }
}

/** Fold a sequence of `[ms, served]` reads, from no knowledge. */
function reads(...seq: Array<[number, string | null]>): ServedBuildTrack {
  return seq.reduce((t, [at, served]) => trackServedBuild(t, OWN, served, at), NO_TRACK)
}

describe('trackServedBuild — a mismatch counts once it reads the same twice, 30 s apart', () => {
  it('a first read of another build is only pending', () => {
    const t = reads([0, NEW])
    expect(t.pending).toEqual({ build: NEW, since: 0 })
    expect(t.confirmed).toBeNull()
  })

  it('the same build again inside 30 s is still pending; at 30 s it is confirmed', () => {
    expect(reads([0, NEW], [CONFIRM_GAP_MS - 1, NEW]).confirmed).toBeNull()
    expect(reads([0, NEW], [CONFIRM_GAP_MS, NEW]).confirmed).toBe(NEW)
  })

  it('a roll in progress, one pod old and one new, never confirms', () => {
    // The old pod serves this page's build: that read clears the count.
    const t = reads([0, NEW], [20_000, OWN], [40_000, NEW], [60_000, OWN], [80_000, NEW])
    expect(t.confirmed).toBeNull()
    expect(t.pending).toEqual({ build: NEW, since: 80_000 })
  })

  it('another build than the pending one starts the count over', () => {
    const t = reads([0, NEW], [40_000, NEWER])
    expect(t.confirmed).toBeNull()
    expect(t.pending).toEqual({ build: NEWER, since: 40_000 })
  })

  it('a confirmed build stays confirmed while reads agree; a newer one has to be confirmed again', () => {
    const confirmed = reads([0, NEW], [31_000, NEW])
    expect(trackServedBuild(confirmed, OWN, NEW, 900_000)).toBe(confirmed)
    const newer = trackServedBuild(confirmed, OWN, NEWER, 900_000)
    expect(newer.confirmed).toBeNull()
    expect(newer.pending).toEqual({ build: NEWER, since: 900_000 })
  })

  it("a read that taught nothing (offline, a failed fetch, an old image's SPA shell) changes nothing", () => {
    const pending = reads([0, NEW])
    expect(trackServedBuild(pending, OWN, null, 50_000)).toBe(pending)
    // …so it cannot stand in for the second read either.
    expect(reads([0, NEW], [40_000, null]).confirmed).toBeNull()
  })

  it("the page's own build served again (a rollback) clears everything", () => {
    expect(reads([0, NEW], [31_000, NEW], [60_000, OWN])).toBe(NO_TRACK)
  })
})

function quiet(over: Partial<StaleBuildSituation> = {}): StaleBuildSituation {
  return {
    online: true,
    confirmed: true,
    settled: true,
    rc: 'none',
    rcExternal: false,
    rcWork: false,
    meetingLive: false,
    unsavedInput: false,
    dialogOpen: false,
    hiddenIdle: false,
    busy: false,
    reloadAllowed: true,
    ...over,
  }
}

describe('decideStaleBuildAction — the decision table', () => {
  it('nothing until a new build is confirmed, and nothing offline', () => {
    expect(decideStaleBuildAction(quiet({ confirmed: false }))).toBe('none')
    expect(decideStaleBuildAction(quiet({ online: false }))).toBe('none')
    expect(decideStaleBuildAction(quiet({ online: false, rc: 'redialing' }))).toBe('none')
  })

  it('a page with nothing live, nothing typed and no dialog reloads now', () => {
    expect(decideStaleBuildAction(quiet())).toBe('reload')
  })

  it('a remote session cut and waiting on its redial is reloaded and resumed', () => {
    expect(decideStaleBuildAction(quiet({ rc: 'redialing' }))).toBe('reload-and-resume')
  })

  it('a live remote session, an external one, a call: the banner, never a reload in the middle of them', () => {
    expect(decideStaleBuildAction(quiet({ rc: 'live' }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'live', rcExternal: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ meetingLive: true }))).toBe('banner')
    // Hidden and idle is no excuse for any of them: a call listened to in a
    // background tab has no input, and neither does a session someone watches.
    expect(decideStaleBuildAction(quiet({ rc: 'live', hiddenIdle: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ meetingLive: true, hiddenIdle: true }))).toBe('banner')
  })

  it('an external session is never resumed: it would need the device password again', () => {
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', rcExternal: true }))).toBe('banner')
  })

  it('a redial carrying work only this page holds, or beside a call, typed text or a dialog, waits', () => {
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', rcWork: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', meetingLive: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', unsavedInput: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', dialogOpen: true }))).toBe('banner')
  })

  it("never on a socket a roll may still be cutting: the page's own /ws must have been steady 30 s", () => {
    expect(decideStaleBuildAction(quiet({ settled: false }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', settled: false }))).toBe('banner')
  })

  it('one automatic reload in 10 minutes: past that, or where storage cannot keep the count, the banner', () => {
    expect(decideStaleBuildAction(quiet({ reloadAllowed: false }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ rc: 'redialing', reloadAllowed: false }))).toBe('banner')
  })

  it('typed text or an open dialog waits, until the page has been hidden and idle a long while', () => {
    expect(decideStaleBuildAction(quiet({ unsavedInput: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ dialogOpen: true }))).toBe('banner')
    expect(decideStaleBuildAction(quiet({ unsavedInput: true, hiddenIdle: true }))).toBe('reload')
    expect(decideStaleBuildAction(quiet({ dialogOpen: true, hiddenIdle: true }))).toBe('reload')
  })

  it("waits for the person's hands to rest", () => {
    expect(decideStaleBuildAction(quiet({ busy: true }))).toBe('banner')
  })

  it('a session that has ended is a quiet moment, the external one included', () => {
    expect(decideStaleBuildAction(quiet({ rc: 'none' }))).toBe('reload')
    expect(decideStaleBuildAction(quiet({ rc: 'none', rcExternal: true }))).toBe('reload')
  })
})

describe('bannerHint — what Reload does to what is live', () => {
  it('names the session it resumes, the external one it ends, the call it leaves', () => {
    expect(bannerHint({ rc: 'live', rcExternal: false, meetingLive: false })).toBe('rcResume')
    expect(bannerHint({ rc: 'redialing', rcExternal: false, meetingLive: false })).toBe('rcResume')
    expect(bannerHint({ rc: 'live', rcExternal: true, meetingLive: false })).toBe('rcExternal')
    expect(bannerHint({ rc: 'none', rcExternal: false, meetingLive: true })).toBe('call')
    expect(bannerHint({ rc: 'none', rcExternal: false, meetingLive: false })).toBeNull()
  })
})

describe('the resume a reload hands to the next page load', () => {
  const args = { agentId: AGENT, permissions: PERMS, orgId: TENANT }

  it('round-trips, and is taken out of storage as it is read', () => {
    const s = memoryStorage()
    expect(writeRcResumeIntent(s, args, 1_000, PATH)).toBe(true)
    const intent = takeRcResumeIntent(s)
    expect(intent).toEqual({ v: 1, ...args, at: 1_000, path: PATH })
    expect(s.has(RC_RESUME_KEY)).toBe(false)
    expect(takeRcResumeIntent(s)).toBeNull()
  })

  it('is removed even when it is not well formed', () => {
    for (const raw of ['{', '[]', JSON.stringify({ v: 2, ...args, at: 1, path: PATH }), JSON.stringify({ v: 1, ...args, at: 1, path: 'x' })]) {
      const s = memoryStorage({ [RC_RESUME_KEY]: raw })
      expect(takeRcResumeIntent(s)).toBeNull()
      expect(s.has(RC_RESUME_KEY)).toBe(false)
    }
  })

  it('holds ids and permissions by shape only', () => {
    const s = memoryStorage()
    expect(writeRcResumeIntent(s, { ...args, agentId: 'not-an-id' }, 1, PATH)).toBe(false)
    expect(writeRcResumeIntent(s, { ...args, orgId: '../x' }, 1, PATH)).toBe(false)
    expect(writeRcResumeIntent(s, { ...args, permissions: 'VIEW; DROP' }, 1, PATH)).toBe(false)
    expect(s.has(RC_RESUME_KEY)).toBe(false)
    // Without an org it is still a resume: connect() falls back to the URL's.
    expect(writeRcResumeIntent(s, { agentId: AGENT, permissions: 'VIEW' }, 1, PATH)).toBe(true)
    expect(takeRcResumeIntent(s)).toEqual({ v: 1, agentId: AGENT, permissions: 'VIEW', at: 1, path: PATH })
  })

  it('where storage cannot hold it, says so (and then nothing reloads)', () => {
    const full = { getItem: () => null, setItem: () => { throw new Error('QuotaExceededError') }, removeItem: () => {} }
    expect(writeRcResumeIntent(full, args, 1, PATH)).toBe(false)
    expect(writeRcResumeIntent(undefined, args, 1, PATH)).toBe(false)
    const refused = { getItem: () => { throw new Error('SecurityError') }, setItem: () => {}, removeItem: () => {} }
    expect(takeRcResumeIntent(refused)).toBeNull()
  })

  const intent: RcResumeIntent = { v: 1, ...args, at: 100_000, path: PATH }
  const here = { agentId: AGENT, tenantId: TENANT, path: PATH, firstRoute: true }

  it('resumes the same device, in the same org, at the same path, under a minute later', () => {
    expect(resumeArgsFor(intent, here, 100_000 + 2_000)).toEqual(args)
    expect(resumeArgsFor(intent, here, 100_000 + RC_RESUME_MAX_AGE_MS - 1)).toEqual(args)
  })

  it('a minute or more later, nothing', () => {
    expect(resumeArgsFor(intent, here, 100_000 + RC_RESUME_MAX_AGE_MS)).toBeNull()
    // A clock far behind the record is not this reload either.
    expect(resumeArgsFor(intent, here, 100_000 - 60_000)).toBeNull()
  })

  it('another device, another org, another path: nothing', () => {
    expect(resumeArgsFor(intent, { ...here, agentId: '69f0000000000000000000ff' }, 101_000)).toBeNull()
    expect(resumeArgsFor(intent, { ...here, tenantId: '69f0000000000000000000ee' }, 101_000)).toBeNull()
    expect(resumeArgsFor(intent, { ...here, path: '/tenant/x/devices' }, 101_000)).toBeNull()
  })

  it('only on the first page the load rendered: a sign-in in between starts over', () => {
    expect(resumeArgsFor(intent, { ...here, firstRoute: false }, 101_000)).toBeNull()
    expect(resumeArgsFor(null, here, 101_000)).toBeNull()
  })
})
