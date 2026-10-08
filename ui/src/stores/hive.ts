// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api, ApiError } from '@/api/client'

/**
 * FR-90 — agent sessions: the server's RECORD of each, never what is in one.
 * The transcript lives on the device that runs the session and reaches the
 * browser over a viewer peer (`composables/useHiveViewer.ts`).
 */

/** Mirrors the Rust `SessionStatus` (snake_case). */
export type HiveSessionStatus =
  | 'starting'
  | 'idle'
  | 'running'
  | 'awaiting_approval'
  | 'stopping'
  | 'ended'
  | 'refused'
  | 'lost'

/** `SessionView` — `crates/modules/hive/src/model.rs`. */
export interface HiveSession {
  id: string
  owner_id: string
  /** P1c — who besides the owner drives it; absent from an older server. */
  drivers?: string[]
  title: string
  /** The session's secret chat room (P0d-1); absent on an older record. */
  room_id?: string
  harness: string
  harness_session: string
  device_id: string
  device_name: string
  folder: string
  /** The local account the device runs it as — the device's claim. */
  account?: string
  status: HiveSessionStatus
  fence: number
  refusal?: string
  end_reason?: string
  detail?: string
  created_at: string
  updated_at: string
  accepted_at?: string
  ended_at?: string
}

/** `POST …/hive/session` — always 200 for a well-formed request. */
export interface HiveStartResult {
  outcome: 'accepted' | 'refused' | 'pending'
  reason?: string
  message?: string
  session?: HiveSession
}

export interface HiveStopResult {
  outcome: 'stopping' | 'queued' | 'ended'
  session: HiveSession
}

/** P1c — someone's part in a session: its owner, a driver (prompts it and
 *  answers its approvals), or a reader (reads it and talks in its room). */
export type HiveRole = 'owner' | 'driver' | 'reader'

export interface HiveParticipant {
  user_id: string
  display_name: string
  role: HiveRole
}

/** `GET/PUT/DELETE …/hive/session/{id}/participant[/{user}]`. */
export interface HiveParticipants {
  items: HiveParticipant[]
  /** Only the owner changes who takes part. */
  may_manage: boolean
}

interface ListResponse {
  items: HiveSession[]
  total: number
  page: number
  per_page: number
  total_pages: number
}

/**
 * P1e — core memory: facts people keep for the org's agent sessions. A session
 * gets a frozen snapshot of them when it starts — and only on a device whose
 * owner turned on `hive_core_memory`, because Claude Code reads them as the
 * user's own instructions.
 */
export type BrainScope = 'org' | 'user' | 'device'
export type FactKind = 'preference' | 'convention' | 'path' | 'gotcha' | 'decision' | 'warning'
export const FACT_KINDS: FactKind[] = ['convention', 'preference', 'path', 'gotcha', 'decision', 'warning']
/** The longest fact, in characters (`MAX_FACT_CHARS`, `brain.rs`). */
export const MAX_FACT_CHARS = 500

/** `FactView` — `crates/modules/hive/src/brain.rs`. */
export interface BrainFact {
  id: string
  scope: BrainScope
  owner_id?: string
  text: string
  kind: FactKind
  version: number
  created_by: string
  created_at: string
  updated_by: string
  updated_at: string
}

export interface BrainBudget {
  scope: BrainScope
  owner_id?: string
  used: number
  budget: number
}

/** `GET …/hive/brain`. */
export interface BrainView {
  brain_rev: number
  facts: BrainFact[]
  budgets: BrainBudget[]
}

/** A session that can still change. */
export function isLive(s: Pick<HiveSession, 'status'>): boolean {
  return !['ended', 'refused', 'lost'].includes(s.status)
}

export const useHiveStore = defineStore('hive', () => {
  const sessions = ref<HiveSession[]>([])
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)
  let fetchSeq = 0

  function base(tenantId: string): string {
    return `/tenant/${tenantId}/hive/session`
  }

  /** The caller's own sessions, newest first. */
  async function fetchSessions(tenantId: string, page = 1, perPage = 50): Promise<void> {
    const seq = ++fetchSeq
    loading.value = true
    error.value = null
    try {
      const res = await api.get<ListResponse>(`${base(tenantId)}?page=${page}&per_page=${perPage}`)
      if (seq !== fetchSeq) return
      sessions.value = res.items
      total.value = res.total
    } catch (e) {
      if (seq !== fetchSeq) return
      error.value = (e as Error).message
    } finally {
      if (seq === fetchSeq) loading.value = false
    }
  }

  function upsert(s: HiveSession): void {
    const i = sessions.value.findIndex((x) => x.id === s.id)
    if (i >= 0) sessions.value[i] = s
    else sessions.value = [s, ...sessions.value]
  }

  async function fetchSession(tenantId: string, sessionId: string): Promise<HiveSession> {
    const s = await api.get<HiveSession>(`${base(tenantId)}/${sessionId}`)
    upsert(s)
    return s
  }

  /** Start a session on a device. The answer is the device's own words. */
  async function start(
    tenantId: string,
    body: { device_id: string; folder: string; title?: string },
  ): Promise<HiveStartResult> {
    const res = await api.post<HiveStartResult>(base(tenantId), body)
    if (res.session) upsert(res.session)
    return res
  }

  async function stop(tenantId: string, sessionId: string): Promise<HiveStopResult> {
    const res = await api.post<HiveStopResult>(`${base(tenantId)}/${sessionId}/stop`, {})
    upsert(res.session)
    return res
  }

  /** P1c — who takes part in a session; anyone who may read it may ask. */
  function fetchParticipants(tenantId: string, sessionId: string): Promise<HiveParticipants> {
    return api.get<HiveParticipants>(`${base(tenantId)}/${sessionId}/participant`)
  }

  /** P1c — the owner names someone a driver or a reader. A driver needs
   *  "Run agent sessions" (HIVE_RUN): the server says so with a 403. */
  function setParticipant(
    tenantId: string,
    sessionId: string,
    userId: string,
    role: Exclude<HiveRole, 'owner'>,
  ): Promise<HiveParticipants> {
    return api.put<HiveParticipants>(`${base(tenantId)}/${sessionId}/participant/${userId}`, { role })
  }

  /** P1c — the owner takes someone out: out of the room, driving nothing. */
  function removeParticipant(tenantId: string, sessionId: string, userId: string): Promise<HiveParticipants> {
    return api.delete<HiveParticipants>(`${base(tenantId)}/${sessionId}/participant/${userId}`)
  }

  /** P1g — whether agent sessions serve each organization, as the server
   *  answered (`GET …/hive`); absent until it has. */
  const served = ref<Record<string, boolean>>({})
  const servedAsks = new Map<string, Promise<boolean>>()

  /**
   * P1g — ask once per organization. `200` is served and `404` is not, both
   * kept; anything else is no answer — not served for now, and asked again
   * next time — because the module is hidden everywhere until the server
   * names it.
   */
  function checkServed(tenantId: string): Promise<boolean> {
    if (tenantId in served.value) return Promise.resolve(served.value[tenantId])
    const asked = servedAsks.get(tenantId)
    if (asked) return asked
    const ask = api
      .get<{ enabled?: boolean }>(`/tenant/${tenantId}/hive`)
      .then((r) => {
        const on = r.enabled === true
        served.value = { ...served.value, [tenantId]: on }
        return on
      })
      .catch((e: unknown) => {
        if (e instanceof ApiError && e.status === 404) {
          served.value = { ...served.value, [tenantId]: false }
        } else {
          console.warn('[hive] could not ask whether agent sessions serve this organization', e)
        }
        return false
      })
      .finally(() => servedAsks.delete(tenantId))
    servedAsks.set(tenantId, ask)
    return ask
  }

  function brainBase(tenantId: string): string {
    return `/tenant/${tenantId}/hive/brain`
  }

  /** P1e — the facts the caller may read: the org's, their own, and with
   *  `deviceId` that device's, each scope with its budget. */
  function fetchBrain(tenantId: string, deviceId?: string): Promise<BrainView> {
    const q = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : ''
    return api.get<BrainView>(`${brainBase(tenantId)}${q}`)
  }

  /** P1e — keep a fact. A 403 (who may write which scope) and a 409
   *  (`over_budget`, with the numbers) come back in the server's words. */
  function keepFact(
    tenantId: string,
    body: { scope: BrainScope; owner_id?: string; text: string; kind?: FactKind },
  ): Promise<BrainFact> {
    return api.post<BrainFact>(brainBase(tenantId), body)
  }

  /** P1e — change a fact at the version read; a stale one is a 409. */
  function editFact(
    tenantId: string,
    factId: string,
    body: { text: string; kind?: FactKind; version: number },
  ): Promise<BrainFact> {
    return api.put<BrainFact>(`${brainBase(tenantId)}/${factId}`, body)
  }

  /** P1e — archive a fact: out of every future session's memory and out of
   *  its budget, kept as a record. */
  function archiveFact(tenantId: string, factId: string): Promise<{ archived: boolean }> {
    return api.delete<{ archived: boolean }>(`${brainBase(tenantId)}/${factId}`)
  }

  return {
    sessions,
    total,
    loading,
    error,
    fetchSessions,
    fetchSession,
    start,
    stop,
    upsert,
    fetchParticipants,
    setParticipant,
    removeParticipant,
    served,
    checkServed,
    fetchBrain,
    keepFact,
    editFact,
    archiveFact,
  }
})
