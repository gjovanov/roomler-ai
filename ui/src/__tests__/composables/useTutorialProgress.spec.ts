import { describe, it, expect, beforeEach, vi } from 'vitest'
import {
  hasSeenTour,
  markTourSeen,
  readTourProgress,
  writeTourProgress,
  shouldAutoOpenTour,
  tourProgressKey,
  tourSeenKey,
  useTutorialProgress,
} from '@/composables/useTutorialProgress'
import { TUTORIAL_CHAPTERS, chapterById } from '@/views/tutorial/tutorialChapters'

const USER = 'u-123'

beforeEach(() => {
  localStorage.clear()
  vi.restoreAllMocks()
})

describe('tutorial seen flag', () => {
  it('is per user and sticks once marked', () => {
    expect(hasSeenTour(USER)).toBe(false)
    markTourSeen(USER)
    expect(hasSeenTour(USER)).toBe(true)
    // A different account on the same machine starts fresh.
    expect(hasSeenTour('someone-else')).toBe(false)
    expect(localStorage.getItem(tourSeenKey(USER))).toBeTruthy()
  })

  it('reads as SEEN when storage throws — never ambush a user we cannot remember', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('policy: storage disabled')
    })
    expect(hasSeenTour(USER)).toBe(true)
  })

  it('marking is non-fatal when storage throws', () => {
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('quota')
    })
    expect(() => markTourSeen(USER)).not.toThrow()
  })
})

describe('shouldAutoOpenTour', () => {
  it('fires exactly once for a never-seen user in an empty org', () => {
    expect(shouldAutoOpenTour({ userId: USER, devices: 0, rooms: 0 })).toBe(true)
    expect(shouldAutoOpenTour({ userId: USER, devices: 0, rooms: 1 })).toBe(true)
    markTourSeen(USER)
    expect(shouldAutoOpenTour({ userId: USER, devices: 0, rooms: 0 })).toBe(false)
  })

  it('never fires for an org that has been set up', () => {
    expect(shouldAutoOpenTour({ userId: USER, devices: 3, rooms: 0 })).toBe(false)
    expect(shouldAutoOpenTour({ userId: USER, devices: 0, rooms: 2 })).toBe(false)
  })

  it('does not navigate on missing evidence (counts not loaded, or no user)', () => {
    expect(shouldAutoOpenTour({ userId: USER, devices: null, rooms: 0 })).toBe(false)
    expect(shouldAutoOpenTour({ userId: USER, devices: 0, rooms: null })).toBe(false)
    expect(shouldAutoOpenTour({ userId: undefined, devices: 0, rooms: 0 })).toBe(false)
  })
})

describe('chapter progress', () => {
  it('toggles, dedupes and persists per user', () => {
    const p = useTutorialProgress(() => USER)
    expect(p.doneCount.value).toBe(0)

    p.toggle('devices')
    p.toggle('devices') // idempotent add path must not duplicate
    p.toggle('devices', true)
    expect(p.done.value).toEqual(['devices'])
    expect(readTourProgress(USER)).toEqual(['devices'])

    p.toggle('rooms')
    expect(p.doneCount.value).toBe(2)

    p.toggle('devices', false)
    expect(p.done.value).toEqual(['rooms'])

    p.reset()
    expect(p.doneCount.value).toBe(0)
    expect(readTourProgress(USER)).toEqual([])
  })

  it('survives corrupt storage', () => {
    localStorage.setItem(tourProgressKey(USER), '{not json')
    expect(readTourProgress(USER)).toEqual([])
    localStorage.setItem(tourProgressKey(USER), '{"nope":1}')
    expect(readTourProgress(USER)).toEqual([])
    // Non-string entries are dropped rather than rendered as [object Object].
    writeTourProgress(USER, ['devices'])
    localStorage.setItem(tourProgressKey(USER), JSON.stringify(['devices', 7, null]))
    expect(readTourProgress(USER)).toEqual(['devices'])
  })
})

describe('tutorial chapters (FR-12 content contract)', () => {
  const ROUTE_NAMES = new Set([
    'tenant-dashboard',
    'devices',
    'rooms',
    'explore',
    'files',
    'invites',
    'analytics',
    'network-acl',
    'network-dns',
    'network-subnet-routes',
    'admin-settings',
    'admin-members',
    'admin-roles',
    'audit-exec',
    'audit-ssh',
    'audit-ssh-activity',
    'tutorial',
  ])

  it('covers every capability the operator asked for', () => {
    const ids = TUTORIAL_CHAPTERS.map((c) => c.id)
    expect(ids).toEqual([
      'get-started',
      'devices',
      'remote-desktop',
      'network',
      'tunnels',
      'acl',
      'rooms',
      'calls',
    ])
    expect(new Set(ids).size).toBe(ids.length)
  })

  it('every chapter is renderable: title, lead, at least one step and a detail table', () => {
    for (const c of TUTORIAL_CHAPTERS) {
      expect(c.title.length).toBeGreaterThan(0)
      expect(c.lead.length).toBeGreaterThan(40)
      expect(c.steps.length).toBeGreaterThan(0)
      expect(c.detail.length).toBeGreaterThan(0)
      expect(c.icon.startsWith('mdi-')).toBe(true)
    }
  })

  it('every deep link names a route that actually exists', () => {
    // The bug this catches: a chapter promising a walk-through step that
    // dead-ends because the route was renamed underneath it.
    for (const c of TUTORIAL_CHAPTERS) {
      for (const s of c.steps) {
        if (!s.to) continue
        expect(ROUTE_NAMES.has(s.to.name), `${c.id}: unknown route "${s.to.name}"`).toBe(true)
      }
    }
  })

  it('every step offers something actionable — a link, a command, or plain instruction text', () => {
    for (const c of TUTORIAL_CHAPTERS) {
      for (const s of c.steps) {
        expect(s.text.length, `${c.id}: empty step text`).toBeGreaterThan(10)
      }
    }
  })

  it('chapterById resolves the URL hash and shrugs off an unknown one', () => {
    expect(chapterById('devices')?.title).toBe('Devices')
    expect(chapterById('nope')).toBeUndefined()
  })
})
