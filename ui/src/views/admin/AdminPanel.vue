<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.admin') }}</h1>

    <v-row>
      <v-col cols="12" md="3">
        <!-- Each section is a real route now (admin-settings, admin-members,
             admin-roles, admin-agents, admin-tasks, admin-audit-log) so the
             URL reflects the active tab, browser back/forward works, and
             deep-links are bookmarkable. The active state below is derived
             from the route name rather than a local ref. -->
        <v-list density="compact" nav>
          <v-list-item
            v-for="item in adminSections"
            :key="item.name"
            :prepend-icon="item.icon"
            :title="item.title"
            :to="{ name: item.name, params: { tenantId } }"
          />
        </v-list>
      </v-col>

      <v-col cols="12" md="9">
        <router-view />
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRoute } from 'vue-router'

const route = useRoute()
const tenantId = computed(() => route.params.tenantId as string)

const adminSections = [
  { name: 'admin-settings',  icon: 'mdi-cog',                  title: 'Settings'  },
  { name: 'admin-members',   icon: 'mdi-account-group',        title: 'Members'   },
  { name: 'admin-roles',     icon: 'mdi-shield-account',       title: 'Roles'     },
  { name: 'admin-agents',    icon: 'mdi-desktop-classic',      title: 'Agents'    },
  { name: 'admin-tasks',     icon: 'mdi-progress-clock',       title: 'Tasks'     },
  { name: 'admin-audit-log', icon: 'mdi-clipboard-text-clock', title: 'Audit Log' },
]
</script>
