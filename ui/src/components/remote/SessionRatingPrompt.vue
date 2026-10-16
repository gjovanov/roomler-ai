<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- FR-102 P9b — the post-session rating: a thumb, then optional reason
       chips, then one Send. Inline in the viewer's ended state — not a
       dialog, no autofocus, nothing captured: the Connect button beside it
       works as before, and the × puts it away for the day. No text field,
       by design: free text is where personal data hides. -->
  <div
    class="rc-rating"
    role="group"
    :aria-label="t('remote.session.rating.question')"
    data-testid="rc-rating"
  >
    <template v-if="status === 'sent'">
      <span class="text-body-2" data-testid="rc-rating-thanks">{{ t('remote.session.rating.thanks') }}</span>
    </template>
    <template v-else>
      <div class="d-flex align-center flex-wrap justify-center" style="gap: 4px">
        <span class="text-body-2 mr-1">{{ t('remote.session.rating.question') }}</span>
        <v-btn
          icon
          size="small"
          variant="text"
          :color="picked === 'up' ? 'success' : undefined"
          :aria-label="t('remote.session.rating.thumbsUp')"
          :aria-pressed="picked === 'up'"
          data-testid="rc-rating-up"
          @click="pick('up')"
        >
          <v-icon>{{ picked === 'up' ? 'mdi-thumb-up' : 'mdi-thumb-up-outline' }}</v-icon>
        </v-btn>
        <v-btn
          icon
          size="small"
          variant="text"
          :color="picked === 'down' ? 'error' : undefined"
          :aria-label="t('remote.session.rating.thumbsDown')"
          :aria-pressed="picked === 'down'"
          data-testid="rc-rating-down"
          @click="pick('down')"
        >
          <v-icon>{{ picked === 'down' ? 'mdi-thumb-down' : 'mdi-thumb-down-outline' }}</v-icon>
        </v-btn>
        <v-btn
          icon
          size="x-small"
          variant="text"
          :aria-label="t('remote.session.rating.dismiss')"
          :title="t('remote.session.rating.dismiss')"
          data-testid="rc-rating-dismiss"
          @click="emit('dismiss')"
        >
          <v-icon>mdi-close</v-icon>
        </v-btn>
      </div>
      <div v-if="picked" class="mt-2 d-flex align-center flex-wrap justify-center" style="gap: 6px">
        <span class="text-caption text-medium-emphasis">{{ t('remote.session.rating.hint') }}</span>
        <v-chip
          v-for="r in RATING_REASONS"
          :key="r"
          size="small"
          :variant="reasons.includes(r) ? 'flat' : 'outlined'"
          :color="reasons.includes(r) ? 'primary' : undefined"
          :aria-pressed="reasons.includes(r)"
          :data-testid="`rc-rating-reason-${r}`"
          @click="toggle(r)"
        >
          {{ t(`remote.session.rating.reasons.${r}`) }}
        </v-chip>
        <v-btn
          size="small"
          variant="tonal"
          color="primary"
          data-testid="rc-rating-send"
          @click="send"
        >
          {{ t('remote.session.rating.send') }}
        </v-btn>
      </div>
    </template>
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { RATING_REASONS, type Rating, type RatingReason } from '@/composables/rcSessionRating'

defineProps<{
  /** `ask` until a rating was sent; `sent` shows the thanks line. */
  status: 'ask' | 'sent'
}>()
const emit = defineEmits<{
  submit: [rating: Rating, reasons: RatingReason[]]
  dismiss: []
}>()

const { t } = useI18n()
const picked = ref<Rating | null>(null)
const reasons = ref<RatingReason[]>([])

function pick(r: Rating) {
  picked.value = r
}

function toggle(r: RatingReason) {
  reasons.value = reasons.value.includes(r)
    ? reasons.value.filter((x) => x !== r)
    : [...reasons.value, r]
}

function send() {
  if (!picked.value) return
  // In vocabulary order, deduplicated — what the server stores anyway.
  const ordered = RATING_REASONS.filter((r) => reasons.value.includes(r))
  emit('submit', picked.value, ordered)
}
</script>

<style scoped>
.rc-rating {
  display: inline-block;
  padding: 8px 12px;
  border: 1px solid rgba(var(--v-border-color), var(--v-border-opacity));
  border-radius: 8px;
  max-width: 100%;
}
</style>
