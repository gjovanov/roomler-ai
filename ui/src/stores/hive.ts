// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
import { defineStore } from 'pinia'
import { ref } from 'vue'
import { api } from '@/api/client'

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
  }
})
