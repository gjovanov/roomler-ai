<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.network') }}</h1>

    <v-row>
      <v-col cols="12" md="3">
        <!-- Mirrors AdminPanel: each section is a real child route so the
             URL reflects the active tab, back/forward works, and deep
             links are bookmarkable. -->
        <v-list density="compact" nav>
          <v-list-item
            v-for="item in sections"
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

const sections = [
  { name: 'network-machines',       icon: 'mdi-server-network', title: 'Machines' },
  { name: 'network-tunnel-clients', icon: 'mdi-tunnel',         title: 'Tunnel clients' },
  { name: 'network-acl',            icon: 'mdi-shield-key',     title: 'Tunnel ACL' },
  { name: 'network-subnet-routes',  icon: 'mdi-lan-connect',    title: 'Subnet routes' },
  { name: 'network-overlay-acl',    icon: 'mdi-shield-lock-outline', title: 'Overlay ACL' },
  { name: 'network-dns',            icon: 'mdi-dns',            title: 'MagicDNS' },
]
</script>
