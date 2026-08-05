<template>
  <div ref="wrap" class="ts-chart">
    <div v-if="!hasData" class="ts-empty text-medium-emphasis text-body-2">
      {{ emptyText }}
    </div>
    <svg
      v-else
      :width="width"
      :height="height"
      role="img"
      @mousemove="onMove"
      @mouseleave="hoverIdx = null"
    >
      <g :transform="`translate(${margin.l},${margin.t})`">
        <g v-for="tick in yTicks" :key="`y${tick}`">
          <line class="ts-grid" :x1="0" :x2="innerW" :y1="yPos(tick)" :y2="yPos(tick)" />
          <text class="ts-tick" :x="-8" :y="yPos(tick)" text-anchor="end" dominant-baseline="middle">
            {{ fmtY(tick) }}
          </text>
        </g>
        <g v-for="tick in xTicks" :key="`x${tick.getTime()}`">
          <text class="ts-tick" :x="xPos(tick)" :y="innerH + 16" text-anchor="middle">
            {{ xFormat(tick) }}
          </text>
        </g>
        <path
          v-for="p in paths"
          :key="p.key"
          :d="p.d"
          :fill="p.fill"
          :stroke="p.stroke"
          :stroke-width="p.fill === 'none' ? 2 : 0"
          :opacity="p.opacity"
        />
        <template v-if="hoverIdx !== null && hoverPoint">
          <line
            class="ts-hover-line"
            :x1="xPos(hoverDate)"
            :x2="xPos(hoverDate)"
            :y1="0"
            :y2="innerH"
          />
        </template>
      </g>
    </svg>
    <div v-if="hoverIdx !== null && hoverPoint && hasData" class="ts-tooltip" :style="tooltipStyle">
      <div class="ts-tooltip-time">{{ tooltipTime }}</div>
      <div v-for="s in series" :key="s.key" class="ts-tooltip-row">
        <span class="ts-dot" :style="{ background: colorOf(s) }" />
        <span class="ts-tooltip-label">{{ s.label }}</span>
        <span class="ts-tooltip-value">{{ tooltipValue(s.key) }}</span>
      </div>
    </div>
    <div class="ts-legend" v-if="hasData && series.length > 1">
      <span v-for="s in series" :key="s.key" class="ts-legend-item">
        <span class="ts-dot" :style="{ background: colorOf(s) }" />{{ s.label }}
      </span>
    </div>
  </div>
</template>

<script setup lang="ts">
// d3 supplies the math (scales, shapes, ticks); Vue renders the SVG —
// no d3-selection, so the chart stays reactive and jsdom-testable
// (sizing comes from props/ResizeObserver, never getBBox).
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { bisector, extent, max } from 'd3-array'
import { scaleLinear, scaleTime } from 'd3-scale'
import { area as d3area, line as d3line, stack as d3stack } from 'd3-shape'
import { timeFormat } from 'd3-time-format'
import { useTheme } from 'vuetify'

export interface SeriesDef {
  key: string
  label: string
  color?: string
}
export interface SeriesPoint {
  /** unix seconds */
  t: number
  [k: string]: number | null | undefined
}

const props = withDefaults(
  defineProps<{
    points: SeriesPoint[]
    series: SeriesDef[]
    height?: number
    /** fill under each line */
    area?: boolean
    /** stack the series (implies area) */
    stacked?: boolean
    yFormat?: (v: number) => string
    emptyText?: string
  }>(),
  {
    height: 220,
    area: false,
    stacked: false,
    emptyText: 'No data yet',
  },
)

// Function-typed prop defaults can't reference local declarations inside
// withDefaults — resolve the fallback at use time instead.
const fmtY = computed(() => props.yFormat ?? defaultFormat)

function defaultFormat(v: number): string {
  const a = Math.abs(v)
  if (a >= 1_000_000_000) return `${(v / 1_000_000_000).toFixed(1)}G`
  if (a >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (a >= 1_000) return `${(v / 1_000).toFixed(1)}k`
  return `${Math.round(v * 10) / 10}`
}

const theme = useTheme()
const palette = computed(() => {
  const c = theme.current.value.colors
  return [c.primary, c.secondary, c.info, c.warning, c.error, c.success]
})
function colorOf(s: SeriesDef): string {
  if (s.color) return s.color
  const i = props.series.findIndex((x) => x.key === s.key)
  return palette.value[i % palette.value.length]
}

const wrap = ref<HTMLElement | null>(null)
const width = ref(600)
let ro: ResizeObserver | null = null
onMounted(() => {
  if (typeof ResizeObserver !== 'undefined' && wrap.value) {
    ro = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect?.width
      if (w && w > 80) width.value = w
    })
    ro.observe(wrap.value)
  }
  if (wrap.value?.clientWidth) width.value = Math.max(wrap.value.clientWidth, 120)
})
onBeforeUnmount(() => ro?.disconnect())

const margin = { l: 44, r: 8, t: 8, b: 22 }
const innerW = computed(() => Math.max(width.value - margin.l - margin.r, 10))
const innerH = computed(() => Math.max(props.height - margin.t - margin.b, 10))

const pts = computed(() => [...props.points].sort((a, b) => a.t - b.t))
const hasData = computed(() => pts.value.length > 0 && props.series.length > 0)

const xScale = computed(() => {
  const [lo, hi] = extent(pts.value, (d) => new Date(d.t * 1000)) as [Date, Date]
  return scaleTime()
    .domain([lo ?? new Date(), hi ?? new Date()])
    .range([0, innerW.value])
})
const yMax = computed(() => {
  if (props.stacked) {
    return (
      max(pts.value, (d) => props.series.reduce((s, def) => s + (num(d[def.key]) ?? 0), 0)) ?? 1
    )
  }
  return max(pts.value, (d) => max(props.series, (def) => num(d[def.key]) ?? 0) ?? 0) ?? 1
})
const yScale = computed(() =>
  scaleLinear()
    .domain([0, yMax.value <= 0 ? 1 : yMax.value * 1.1])
    .range([innerH.value, 0])
    .nice(),
)

function num(v: number | null | undefined): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}
function xPos(d: Date): number {
  return xScale.value(d)
}
function yPos(v: number): number {
  return yScale.value(v)
}
const yTicks = computed(() => yScale.value.ticks(4))
const xTicks = computed(() => xScale.value.ticks(Math.max(2, Math.floor(innerW.value / 110))))

const spanMs = computed(() => {
  const d = xScale.value.domain()
  return d[1].getTime() - d[0].getTime()
})
const xFormat = computed(() =>
  spanMs.value > 3 * 86_400_000 ? timeFormat('%b %d') : timeFormat('%H:%M'),
)

interface PathDef {
  key: string
  d: string
  fill: string
  stroke: string
  opacity: number
}
const paths = computed<PathDef[]>(() => {
  if (!hasData.value) return []
  if (props.stacked) {
    const st = d3stack<SeriesPoint>()
      .keys(props.series.map((s) => s.key))
      .value((d, key) => num(d[key]) ?? 0)(pts.value)
    return st.map((layer, i) => {
      const def = props.series[i]
      const a = d3area<(typeof layer)[number]>()
        .x((d) => xScale.value(new Date(d.data.t * 1000)))
        .y0((d) => yScale.value(d[0]))
        .y1((d) => yScale.value(d[1]))
      return {
        key: def.key,
        d: a(layer) ?? '',
        fill: colorOf(def),
        stroke: 'none',
        opacity: 0.75,
      }
    })
  }
  return props.series.flatMap((def) => {
    const defined = (d: SeriesPoint) => num(d[def.key]) !== null
    const out: PathDef[] = []
    if (props.area) {
      const a = d3area<SeriesPoint>()
        .defined(defined)
        .x((d) => xScale.value(new Date(d.t * 1000)))
        .y0(innerH.value)
        .y1((d) => yScale.value(num(d[def.key]) ?? 0))
      out.push({
        key: `${def.key}-a`,
        d: a(pts.value) ?? '',
        fill: colorOf(def),
        stroke: 'none',
        opacity: 0.18,
      })
    }
    const l = d3line<SeriesPoint>()
      .defined(defined)
      .x((d) => xScale.value(new Date(d.t * 1000)))
      .y((d) => yScale.value(num(d[def.key]) ?? 0))
    out.push({ key: def.key, d: l(pts.value) ?? '', fill: 'none', stroke: colorOf(def), opacity: 1 })
    return out
  })
})

// Hover: nearest point by time.
const hoverIdx = ref<number | null>(null)
const bisectT = bisector<SeriesPoint, number>((d) => d.t).center
function onMove(ev: MouseEvent) {
  if (!hasData.value) return
  const rect = (ev.currentTarget as SVGElement).getBoundingClientRect()
  const px = ev.clientX - rect.left - margin.l
  const t = xScale.value.invert(px).getTime() / 1000
  hoverIdx.value = Math.max(0, Math.min(pts.value.length - 1, bisectT(pts.value, t)))
}
const hoverPoint = computed(() =>
  hoverIdx.value === null ? null : (pts.value[hoverIdx.value] ?? null),
)
const hoverDate = computed(() => new Date((hoverPoint.value?.t ?? 0) * 1000))
const tooltipTime = computed(() =>
  hoverPoint.value ? timeFormat('%b %d %H:%M')(new Date(hoverPoint.value.t * 1000)) : '',
)
function tooltipValue(key: string): string {
  const v = hoverPoint.value ? num(hoverPoint.value[key]) : null
  return v === null ? '—' : fmtY.value(v)
}
const tooltipStyle = computed(() => {
  const x = xScale.value(hoverDate.value) + margin.l
  const flip = x > width.value * 0.6
  return {
    left: flip ? 'auto' : `${x + 12}px`,
    right: flip ? `${width.value - x + 12}px` : 'auto',
    top: '8px',
  }
})
</script>

<style scoped>
.ts-chart {
  position: relative;
  width: 100%;
}
.ts-empty {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 120px;
}
.ts-grid {
  stroke: rgba(var(--v-border-color), var(--v-border-opacity));
  stroke-dasharray: 2 3;
}
.ts-tick {
  fill: rgba(var(--v-theme-on-surface), 0.55);
  font-size: 10px;
}
.ts-hover-line {
  stroke: rgba(var(--v-theme-on-surface), 0.35);
  stroke-dasharray: 3 3;
}
.ts-tooltip {
  position: absolute;
  pointer-events: none;
  background: rgb(var(--v-theme-surface));
  border: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  border-radius: 6px;
  padding: 6px 10px;
  font-size: 12px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.25);
  z-index: 2;
  max-width: 260px;
}
.ts-tooltip-time {
  opacity: 0.6;
  margin-bottom: 2px;
}
.ts-tooltip-row {
  display: flex;
  align-items: center;
  gap: 6px;
}
.ts-tooltip-value {
  margin-left: auto;
  font-weight: 600;
}
.ts-dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
}
.ts-legend {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  font-size: 12px;
  opacity: 0.75;
  margin-top: 2px;
}
.ts-legend-item {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
</style>
