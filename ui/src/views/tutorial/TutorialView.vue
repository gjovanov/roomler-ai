<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <div class="d-flex align-center flex-wrap ga-2 mb-2 mb-md-4">
      <h1 class="text-h5 text-md-h4">{{ $t('nav.tutorial') }}</h1>
      <v-chip size="small" variant="tonal" class="ml-1">
        {{ doneCount }}/{{ chapters.length }} done
      </v-chip>
      <v-spacer />
      <v-btn
        v-if="doneCount > 0"
        size="small"
        variant="text"
        prepend-icon="mdi-restore"
        @click="reset"
      >
        Reset progress
      </v-btn>
    </div>

    <v-row>
      <!-- Chapter rail. On phones it becomes a horizontal chip row so the
           body still gets the full width. -->
      <v-col cols="12" md="3" lg="3">
        <v-card variant="outlined" class="tutorial-rail">
          <!-- Plain click + :active rather than v-list selection: the rail
               must track the URL hash (which back/forward and deep links
               also drive), not v-list's own selection state. -->
          <v-list density="compact" nav>
            <v-list-item
              v-for="c in chapters"
              :key="c.id"
              :active="c.id === activeId"
              color="primary"
              :prepend-icon="c.icon"
              :title="c.title"
              :subtitle="c.blurb"
              @click="go(c.id)"
            >
              <template #append>
                <v-icon v-if="isDone(c.id)" size="small" color="success">
                  mdi-check-circle
                </v-icon>
              </template>
            </v-list-item>
          </v-list>
        </v-card>
      </v-col>

      <v-col cols="12" md="9" lg="9">
        <v-card variant="outlined" class="pa-4 pa-md-6">
          <div class="d-flex align-center ga-2 mb-2">
            <v-icon :icon="active.icon" color="primary" />
            <h2 class="text-h6 text-md-h5">{{ active.title }}</h2>
          </div>

          <!-- The heroes are the README's own illustrations: a light
               palette on transparent. They are painted on a fixed light
               panel in BOTH themes — recolouring them per theme would mean
               forking four artworks. -->
          <div v-if="active.hero" class="tutorial-hero mb-4">
            <img :src="active.hero" :alt="active.heroAlt || active.title" />
          </div>

          <p class="text-body-1 mb-4">{{ active.lead }}</p>

          <h3 class="text-subtitle-1 font-weight-medium mb-2">Try it now</h3>
          <v-list class="tutorial-steps mb-4" density="comfortable">
            <v-list-item
              v-for="(s, i) in active.steps"
              :key="i"
              class="px-0"
            >
              <template #prepend>
                <v-avatar size="24" color="primary" variant="tonal" class="mr-3">
                  <span class="text-caption">{{ i + 1 }}</span>
                </v-avatar>
              </template>
              <div class="text-body-2">{{ s.text }}</div>
              <div v-if="s.code" class="tutorial-code mt-2">
                <code>{{ s.code }}</code>
                <v-btn
                  icon="mdi-content-copy"
                  size="x-small"
                  variant="text"
                  :aria-label="`Copy: ${s.code}`"
                  @click="copy(s.code)"
                />
              </div>
              <div v-if="s.to" class="mt-2">
                <v-btn
                  size="small"
                  variant="tonal"
                  color="primary"
                  append-icon="mdi-arrow-right"
                  :to="linkFor(s)"
                >
                  {{ s.linkLabel || 'Open' }}
                </v-btn>
              </div>
            </v-list-item>
          </v-list>

          <h3 class="text-subtitle-1 font-weight-medium mb-2">In detail</h3>
          <v-table density="compact" class="tutorial-detail mb-4">
            <tbody>
              <tr v-for="(d, i) in active.detail" :key="i">
                <th class="font-weight-medium">{{ d.label }}</th>
                <td>{{ d.text }}</td>
              </tr>
            </tbody>
          </v-table>

          <v-divider class="mb-4" />

          <div class="d-flex align-center flex-wrap ga-2">
            <v-btn
              :color="isDone(active.id) ? 'success' : 'primary'"
              :variant="isDone(active.id) ? 'tonal' : 'flat'"
              :prepend-icon="isDone(active.id) ? 'mdi-check-circle' : 'mdi-check'"
              size="small"
              @click="toggle(active.id)"
            >
              {{ isDone(active.id) ? 'Done' : 'Mark as done' }}
            </v-btn>
            <v-spacer />
            <v-btn
              v-if="prevChapter"
              size="small"
              variant="text"
              prepend-icon="mdi-chevron-left"
              @click="go(prevChapter.id)"
            >
              {{ prevChapter.title }}
            </v-btn>
            <v-btn
              v-if="nextChapter"
              size="small"
              variant="text"
              append-icon="mdi-chevron-right"
              @click="go(nextChapter.id)"
            >
              {{ nextChapter.title }}
            </v-btn>
          </div>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '@/stores/auth'
import { useSnackbar } from '@/composables/useSnackbar'
import { useTutorialProgress } from '@/composables/useTutorialProgress'
import { TUTORIAL_CHAPTERS, chapterById, type TutorialStep } from './tutorialChapters'

const route = useRoute()
const router = useRouter()
const auth = useAuthStore()
const { showSuccess } = useSnackbar()

const chapters = TUTORIAL_CHAPTERS
const tenantId = computed(() => route.params.tenantId as string)

// The URL hash IS the chapter: /tutorial#devices deep-links from the
// devices empty state, and back/forward moves between chapters.
const activeId = ref(chapterById(route.hash.replace('#', ''))?.id ?? chapters[0].id)
watch(
  () => route.hash,
  (h) => {
    const c = chapterById(h.replace('#', ''))
    if (c) activeId.value = c.id
  },
)

const active = computed(() => chapterById(activeId.value) ?? chapters[0])
const activeIndex = computed(() => chapters.findIndex((c) => c.id === active.value.id))
const prevChapter = computed(() => chapters[activeIndex.value - 1])
const nextChapter = computed(() => chapters[activeIndex.value + 1])

const { doneCount, isDone, toggle, reset } = useTutorialProgress(() => auth.user?.id)

function go(id: string) {
  activeId.value = id
  router.replace({ hash: `#${id}` })
  globalThis.scrollTo?.({ top: 0, behavior: 'smooth' })
}

/** Steps name a route; the tenant id comes from where the reader already
 *  is, so a link can never land on someone else's org. */
function linkFor(s: TutorialStep) {
  return { name: s.to!.name, params: { tenantId: tenantId.value }, query: s.to!.query }
}

async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text)
    showSuccess('Copied to clipboard')
  } catch {
    /* clipboard blocked (permissions / insecure context) — the command is
       visible and selectable either way, so this is not worth an error. */
  }
}
</script>

<style scoped>
.tutorial-rail {
  position: sticky;
  top: 76px;
}

/* The four heroes are light-palette artwork (README parity). Give them a
   fixed pale panel so the dark theme doesn't put dark ink on a dark
   surface — one artwork, legible in both themes. */
.tutorial-hero {
  background: #f5f7fa;
  border-radius: 8px;
  padding: 12px;
  text-align: center;
}
.tutorial-hero img {
  max-width: 100%;
  height: auto;
  max-height: 340px;
}

.tutorial-steps :deep(.v-list-item__content) {
  overflow: visible;
  white-space: normal;
}

.tutorial-code {
  display: flex;
  align-items: center;
  gap: 4px;
  background: rgba(var(--v-theme-on-surface), 0.06);
  border-radius: 6px;
  padding: 4px 4px 4px 10px;
  overflow-x: auto;
}
.tutorial-code code {
  font-size: 0.8125rem;
  white-space: nowrap;
}

.tutorial-detail th {
  width: 30%;
  min-width: 140px;
  vertical-align: top;
  text-align: left;
}
.tutorial-detail td {
  white-space: normal;
  padding-top: 6px;
  padding-bottom: 6px;
}
</style>
