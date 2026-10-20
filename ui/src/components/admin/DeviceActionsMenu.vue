<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <!-- ONE action menu for a device row, used by the desktop grid and the phone
       card alike. Each used to carry its own copy, and the copies drifted: the
       phone's lost "Edit name & tags" and its two overlay links lost the
       tenant id (`/tenant//network/acl`). The menu only says WHICH action was
       picked; the page that owns the dialogs does the rest. -->
  <v-menu v-if="row.kind === 'agent' && agent">
    <template #activator="{ props: menuProps }">
      <v-btn
        v-bind="menuProps"
        icon="mdi-dots-vertical"
        size="small"
        variant="text"
        :aria-label="`Actions for ${label}`"
      />
    </template>
    <v-list density="compact" min-width="230">
      <v-list-subheader>Maintenance</v-list-subheader>
      <v-list-item
        prepend-icon="mdi-update"
        title="Update now"
        :subtitle="updateBlocked"
        :disabled="updateBusy || !!updateBlocked"
        @click="emit('action', 'update')"
      />
      <v-list-item
        v-if="networkMounted"
        prepend-icon="mdi-key-change"
        title="Rotate overlay key…"
        :disabled="rotateKeyBusy"
        @click="emit('action', 'rotate-key')"
      />
      <v-list-subheader>Diagnostics</v-list-subheader>
      <v-list-item
        prepend-icon="mdi-alert-circle-outline"
        title="Crash reports"
        @click="emit('action', 'crashes')"
      />
      <v-list-item
        prepend-icon="mdi-text-box-search-outline"
        title="Agent logs"
        @click="emit('action', 'logs')"
      />
      <v-list-item
        prepend-icon="mdi-console"
        title="Device console"
        @click="emit('action', 'console')"
      />
      <v-list-subheader>Access</v-list-subheader>
      <v-list-item
        prepend-icon="mdi-rename-box"
        title="Edit name &amp; tags"
        @click="emit('action', 'edit')"
      />
      <v-list-item
        prepend-icon="mdi-domain-plus"
        title="Add to another organization"
        @click="emit('action', 'join-org')"
      />
      <v-list-item
        prepend-icon="mdi-account-switch"
        title="Reassign owner"
        @click="emit('action', 'reassign')"
      />
      <v-list-item
        prepend-icon="mdi-shield-key-outline"
        title="Execution policy"
        @click="emit('action', 'exec-policy')"
      />
      <v-list-item
        v-if="networkMounted"
        prepend-icon="mdi-console-network-outline"
        title="SSH policy"
        @click="emit('action', 'ssh-policy')"
      />
      <!-- FR-52 gate 2. Distinct from every policy above it: those decide
           what a COLLEAGUE may do, this one admits someone outside the
           organization. -->
      <v-list-item
        prepend-icon="mdi-account-arrow-right-outline"
        title="External access"
        @click="emit('action', 'external-access')"
      />
      <!-- Writes an INTENT the device may refuse (step 5 of
           docs/remote-config.md) — unlike the two policies above, which take
           effect the moment they save. The dialog leads with that
           difference. -->
      <v-list-item
        prepend-icon="mdi-cog-transfer-outline"
        title="Device configuration"
        @click="emit('action', 'remote-config')"
      />
      <v-list-subheader>Network</v-list-subheader>
      <v-list-item
        prepend-icon="mdi-ip-network-outline"
        title="Tunnel mesh routes"
        @click="emit('action', 'routes')"
      />
      <v-list-item
        v-if="node"
        prepend-icon="mdi-lan-disconnect"
        :title="node.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
        @click="emit('action', 'evict')"
      />
      <v-list-item
        v-if="node"
        prepend-icon="mdi-lan-connect"
        title="Overlay routes"
        :to="{ path: `/tenant/${tenantId}/network/subnet-routes`, query: { node: node.id } }"
      />
      <v-list-item
        prepend-icon="mdi-shield-lock-outline"
        title="Overlay ACL"
        :to="{ path: `/tenant/${tenantId}/network/acl`, query: { tab: 'overlay' } }"
      />
      <v-divider />
      <v-list-item
        prepend-icon="mdi-delete"
        title="Delete device"
        base-color="error"
        @click="emit('action', 'delete')"
      />
    </v-list>
  </v-menu>
  <v-menu v-else-if="row.kind === 'tunnel_client'">
    <template #activator="{ props: menuProps }">
      <v-btn
        v-bind="menuProps"
        icon="mdi-dots-vertical"
        size="small"
        variant="text"
        :aria-label="`Actions for ${label}`"
      />
    </template>
    <v-list density="compact" min-width="230">
      <v-list-item
        prepend-icon="mdi-rename-box"
        title="Edit name &amp; tags"
        @click="emit('action', 'edit')"
      />
      <v-list-subheader>Network</v-list-subheader>
      <v-list-item
        v-if="node"
        prepend-icon="mdi-lan-disconnect"
        :title="node.will_rejoin ? 'Evict / reassign overlay address' : 'Remove from mesh'"
        @click="emit('action', 'evict')"
      />
      <v-divider />
      <v-list-item
        v-if="client"
        prepend-icon="mdi-delete"
        title="Delete tunnel client"
        base-color="error"
        @click="emit('action', 'delete-tunnel')"
      />
    </v-list>
  </v-menu>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import type { Agent } from '@/stores/agents'
import type { DeviceRow } from '@/stores/devices'
import type { OverlayNode } from '@/stores/overlayRoutes'
import type { TunnelClient } from '@/stores/tunnelClients'

/** What the menu can ask for. The page maps each to its dialog or request. */
export type DeviceAction =
  | 'update'
  | 'rotate-key'
  | 'crashes'
  | 'logs'
  | 'console'
  | 'edit'
  | 'join-org'
  | 'reassign'
  | 'exec-policy'
  | 'ssh-policy'
  | 'external-access'
  | 'remote-config'
  | 'routes'
  | 'evict'
  | 'delete'
  | 'delete-tunnel'

const props = defineProps<{
  tenantId: string
  row: DeviceRow
  /** The full agent, for an agent row — no agent, no menu (its actions all
   *  need one). */
  agent?: Agent
  /** The tunnel client, for a tunnel row — its delete needs the record. */
  client?: TunnelClient
  /** The device's overlay node, when it holds an address. */
  node?: OverlayNode
  /** FR-69 P9 — the overlay-key rotation and the SSH policy are the network
   *  module's routes; a menu item that leads to a 404 is worse than none. */
  networkMounted?: boolean
  updateBusy?: boolean
  rotateKeyBusy?: boolean
  /** Why "Update now" will not take, when it will not (FR-67 P7). */
  updateBlocked?: string
}>()

const emit = defineEmits<{ action: [action: DeviceAction] }>()

const label = computed(() => props.row.display_name || props.row.name)
</script>
