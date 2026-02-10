<template>
  <v-app>
    <v-navigation-drawer v-model="drawer" :rail="rail" permanent>
      <v-list-item
        :prepend-icon="rail ? 'mdi-menu' : undefined"
        :title="rail ? '' : 'Roomler'"
        @click="rail = !rail"
      >
        <template v-if="!rail" #prepend>
          <v-icon color="primary">mdi-forum</v-icon>
        </template>
      </v-list-item>

      <v-divider />

      <!-- Tenant selector -->
      <v-list v-if="!rail" density="compact">
        <v-list-item
          v-for="t in tenantStore.tenants"
          :key="t.id"
          :title="t.name"
          :active="tenantStore.current?.id === t.id"
          @click="selectTenant(t)"
          prepend-icon="mdi-domain"
        />
      </v-list>

      <v-divider />

      <!-- Navigation -->
      <v-list density="compact" nav>
        <v-list-item
          v-for="item in navItems"
          :key="item.to"
          :to="item.to"
          :prepend-icon="item.icon"
          :title="item.title"
        />
      </v-list>

      <template #append>
        <v-list density="compact">
          <v-list-item
            prepend-icon="mdi-cog"
            title="Settings"
            :to="settingsRoute"
          />
        </v-list>
      </template>
    </v-navigation-drawer>

    <v-app-bar density="compact" flat>
      <v-app-bar-title>
        {{ pageTitle }}
      </v-app-bar-title>

      <template #append>
        <v-btn icon="mdi-magnify" size="small" />
        <v-btn icon="mdi-bell-outline" size="small" />
        <v-menu>
          <template #activator="{ props }">
            <v-btn icon v-bind="props" size="small">
              <v-avatar size="28" color="primary">
                <span class="text-caption">{{ initials }}</span>
              </v-avatar>
            </v-btn>
          </template>
          <v-list density="compact">
            <v-list-item prepend-icon="mdi-account" title="Profile" />
            <v-list-item prepend-icon="mdi-logout" title="Logout" @click="handleLogout" />
          </v-list>
        </v-menu>
      </template>
    </v-app-bar>

    <v-main>
      <router-view />
    </v-main>
  </v-app>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute } from 'vue-router'
import { useAuth } from '@/composables/useAuth'
import { useTenantStore } from '@/stores/tenant'

const { auth, logout: handleLogout } = useAuth()
const tenantStore = useTenantStore()
const route = useRoute()

const drawer = ref(true)
const rail = ref(false)

const tenantId = computed(() => tenantStore.current?.id || '')

const navItems = computed(() => {
  if (!tenantId.value) return []
  const base = `/tenant/${tenantId.value}`
  return [
    { icon: 'mdi-view-dashboard', title: 'Dashboard', to: base },
    { icon: 'mdi-pound', title: 'Channels', to: `${base}/channels` },
    { icon: 'mdi-video', title: 'Conferences', to: `${base}/conferences` },
    { icon: 'mdi-compass', title: 'Explore', to: `${base}/explore` },
    { icon: 'mdi-folder', title: 'Files', to: `${base}/files` },
    { icon: 'mdi-account-plus', title: 'Invites', to: `${base}/invites` },
  ]
})

const settingsRoute = computed(() =>
  tenantId.value ? `/tenant/${tenantId.value}/admin` : '/',
)

const pageTitle = computed(() => {
  const name = route.name as string
  if (name === 'channel-chat') return 'Chat'
  if (name === 'conference') return 'Meeting'
  return (route.meta.title as string) || 'Roomler'
})

const initials = computed(() => {
  const name = auth.user?.display_name || auth.user?.username || '?'
  return name.charAt(0).toUpperCase()
})

interface Tenant {
  id: string
  name: string
  slug: string
}

function selectTenant(t: Tenant) {
  tenantStore.setCurrent(t as never)
}

onMounted(() => {
  tenantStore.fetchTenants()
})
</script>
