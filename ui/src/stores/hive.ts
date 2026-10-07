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

  return { sessions, total, loading, error, fetchSessions, fetchSession, start, stop, upsert }
})
