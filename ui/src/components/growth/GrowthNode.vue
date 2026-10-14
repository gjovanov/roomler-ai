<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 — one node of a digest section, by shape. Every string is
       rendered as TEXT ({{ }}), never v-html: UTM values, org names and
       referrer hosts are client-chosen. -->
  <v-chip
    v-if="isNotMeasured(value)"
    size="small"
    variant="tonal"
    color="grey"
    class="text-medium-emphasis"
  >
    {{ notMeasuredLabel(value) }}
  </v-chip>

  <span v-else-if="isFigure(value)">
    <span class="text-h6">{{ value.day }}</span>
    <span class="text-caption text-medium-emphasis ml-1">7d {{ value.d7 }} · 28d {{ value.d28 }}</span>
  </span>

  <template v-else-if="Array.isArray(value)">
    <span v-if="value.length === 0" class="text-caption text-medium-emphasis">none</span>
    <v-table v-else-if="isObjectRow(value[0])" density="compact" class="growth-table">
      <thead>
        <tr>
          <th v-for="k in columns(value)" :key="k" class="text-left">{{ humanKey(k) }}</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, i) in value" :key="i">
          <td v-for="k in columns(value)" :key="k">{{ cell((row as Record<string, unknown>)[k]) }}</td>
        </tr>
      </tbody>
    </v-table>
    <span v-else>{{ value.map(cell).join(', ') }}</span>
  </template>

  <dl v-else-if="isObjectRow(value)" class="growth-dl">
    <template v-for="(v, k) in value" :key="k">
      <dt class="text-caption text-medium-emphasis">{{ humanKey(String(k)) }}</dt>
      <dd><growth-node :value="v" /></dd>
    </template>
  </dl>

  <span v-else>{{ cell(value) }}</span>
</template>

<script setup lang="ts">
import { isFigure, isNotMeasured, notMeasuredLabel, humanKey } from '@/stores/growth'

defineOptions({ name: 'GrowthNode' })
defineProps<{ value: unknown }>()

function isObjectRow(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

/** The union of the rows' keys, first-seen order. */
function columns(rows: unknown[]): string[] {
  const out: string[] = []
  for (const r of rows) {
    if (!isObjectRow(r)) continue
    for (const k of Object.keys(r)) if (!out.includes(k)) out.push(k)
  }
  return out
}

/** A table cell: primitives as text, nested shapes as compact JSON. */
function cell(v: unknown): string {
  if (v === null || v === undefined) return '—'
  if (typeof v === 'number') return Number.isInteger(v) ? String(v) : v.toFixed(1)
  if (typeof v === 'string' || typeof v === 'boolean') return String(v)
  if (isNotMeasured(v)) return notMeasuredLabel(v)
  if (isFigure(v)) return `${v.day} (7d ${v.d7} · 28d ${v.d28})`
  return JSON.stringify(v)
}
</script>

<style scoped>
.growth-dl {
  display: grid;
  grid-template-columns: max-content 1fr;
  column-gap: 16px;
  row-gap: 4px;
  align-items: baseline;
  margin: 0;
}
.growth-dl dd {
  margin: 0;
  min-width: 0;
}
.growth-table {
  max-width: 100%;
  overflow-x: auto;
}
</style>
