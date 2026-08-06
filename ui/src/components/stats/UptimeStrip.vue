<template>
  <div class="uptime-wrap">
    <div v-if="!rows.length" class="uptime-empty text-medium-emphasis text-body-2">
      {{ emptyText }}
    </div>
    <template v-else>
      <div v-for="row in rows" :key="row.agent_id" class="uptime-row">
        <div class="uptime-label text-body-2" :title="row.name || row.agent_id">
          {{ row.name || row.agent_id.slice(-6) }}
        </div>
        <div class="uptime-bar">
          <div
            v-for="(seg, i) in row.segments"
            :key="i"
            class="uptime-seg"
            :style="{ left: seg.left, width: seg.width, background: seg.color }"
            :title="seg.title"
          />
        </div>
        <div class="uptime-pct text-caption" :title="`${row.onlinePct.toFixed(1)}% online`">
          {{ row.onlinePct.toFixed(0) }}%
        </div>
      </div>
      <div class="uptime-axis text-caption text-medium-emphasis">
        <span>{{ fmtAxis(windowFrom) }}</span>
        <span>now</span>
      </div>
      <div class="uptime-legend text-caption">
        <span v-for="p in LEGEND" :key="p.key" class="uptime-legend-item">
          <span class="uptime-dot" :style="{ background: colorFor(p.key) }" />{{ p.label }}
        </span>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
// Presence intervals → one horizontal bar per agent. Pure layout math on
// percentages (no d3 scales needed, and no measurement — jsdom-safe).
import { computed } from 'vue'
import { timeFormat } from 'd3-time-format'
import { useTheme } from 'vuetify'

export interface UptimeInterval {
  from: number
  to: number
  presence: string
}
export interface UptimeAgent {
  agent_id: string
  name?: string
  intervals: UptimeInterval[]
}

const props = withDefaults(
  defineProps<{
    agents: UptimeAgent[]
    emptyText?: string
  }>(),
  { emptyText: 'No presence history yet' },
)

const LEGEND = [
  { key: 'online', label: 'Online' },
  { key: 'stale', label: 'Stale' },
  { key: 'offline', label: 'Offline' },
  { key: 'unknown', label: 'No data' },
]

const theme = useTheme()
function colorFor(presence: string): string {
  const c = theme.current.value.colors
  switch (presence) {
    case 'online':
      return c.success
    // Stale = heartbeats stopped but the row hasn't aged out yet: a
    // degraded state, deliberately NOT a gap in the strip.
    case 'stale':
      return c.warning
    case 'offline':
      return c.error
    default:
      return 'rgba(var(--v-theme-on-surface), 0.12)'
  }
}

const windowFrom = computed(() =>
  Math.min(...props.agents.flatMap((a) => a.intervals.map((i) => i.from))),
)
const windowTo = computed(() =>
  Math.max(...props.agents.flatMap((a) => a.intervals.map((i) => i.to))),
)

const fmtTip = timeFormat('%b %d %H:%M')
function fmtAxis(t: number): string {
  return Number.isFinite(t) ? fmtTip(new Date(t * 1000)) : ''
}

interface Segment {
  left: string
  width: string
  color: string
  title: string
}
interface Row {
  agent_id: string
  name?: string
  segments: Segment[]
  onlinePct: number
}

const rows = computed<Row[]>(() => {
  const t0 = windowFrom.value
  const t1 = windowTo.value
  const span = t1 - t0
  if (!Number.isFinite(span) || span <= 0) return []
  return props.agents
    .filter((a) => a.intervals.length > 0)
    .map((a) => {
      let onlineSecs = 0
      const segments = a.intervals.map((iv) => {
        const dur = Math.max(iv.to - iv.from, 0)
        if (iv.presence === 'online') onlineSecs += dur
        return {
          left: `${((iv.from - t0) / span) * 100}%`,
          width: `${Math.max((dur / span) * 100, 0.4)}%`,
          color: colorFor(iv.presence),
          title: `${iv.presence} · ${fmtTip(new Date(iv.from * 1000))} → ${fmtTip(
            new Date(iv.to * 1000),
          )}`,
        }
      })
      return {
        agent_id: a.agent_id,
        name: a.name,
        segments,
        onlinePct: (onlineSecs / span) * 100,
      }
    })
    .sort((x, y) => y.onlinePct - x.onlinePct)
})
</script>

<style scoped>
.uptime-wrap {
  width: 100%;
}
.uptime-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 100px;
}
.uptime-row {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}
.uptime-label {
  flex: 0 0 140px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.uptime-bar {
  position: relative;
  flex: 1 1 auto;
  height: 14px;
  border-radius: 3px;
  overflow: hidden;
  background: rgba(var(--v-theme-on-surface), 0.06);
}
.uptime-seg {
  position: absolute;
  top: 0;
  bottom: 0;
}
.uptime-pct {
  flex: 0 0 38px;
  text-align: right;
  opacity: 0.75;
}
.uptime-axis {
  display: flex;
  justify-content: space-between;
  margin-left: 148px;
  margin-right: 46px;
}
.uptime-legend {
  display: flex;
  gap: 10px;
  margin-top: 6px;
  opacity: 0.75;
}
.uptime-legend-item {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.uptime-dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
}
</style>
