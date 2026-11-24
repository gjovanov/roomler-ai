<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Copyright (C) 2026 G ROX EOOD -->
<template>
  <div>
    <v-list density="compact">
      <v-list-item
        v-for="member in members"
        :key="member.id"
        :title="member.display_name"
      >
        <template #prepend>
          <v-avatar size="28" color="primary" class="mr-2">
            <span class="text-caption">{{ initial(member) }}</span>
          </v-avatar>
        </template>
        <template #append>
          <v-chip
            v-for="role in memberRoles(member)"
            :key="role.id"
            :color="roleStore.colorHex(role.color)"
            size="x-small"
            class="ml-1"
          >
            {{ role.name }}
          </v-chip>
          <v-btn
            v-if="canManageRoles"
            icon="mdi-shield-plus"
            size="x-small"
            variant="text"
            @click="openAssignDialog(member)"
          />
        </template>
      </v-list-item>
    </v-list>

    <role-assign-dialog
      v-model="showAssignDialog"
      :tenant-id="tenantId"
      :user-id="selectedMember?.user_id || ''"
      :member-name="selectedMember?.display_name || ''"
      :current-role-ids="selectedMember?.role_ids"
      @assigned="onRoleAssigned"
    />
  </div>
</template>

<script setup lang="ts">
import { ref, watch } from 'vue'
import { useRoleStore } from '@/stores/role'
import RoleAssignDialog from './RoleAssignDialog.vue'

interface Member {
  id: string
  user_id: string
  display_name: string
  role_ids?: string[]
}

const props = defineProps<{
  tenantId: string
  members: Member[]
  canManageRoles?: boolean
}>()

const roleStore = useRoleStore()
const showAssignDialog = ref(false)
const selectedMember = ref<Member | null>(null)

function initial(member: Member) {
  return (member.display_name || '?').charAt(0).toUpperCase()
}

function memberRoles(member: Member) {
  if (!member.role_ids) return []
  return roleStore.roles.filter((r) => member.role_ids!.includes(r.id))
}

function openAssignDialog(member: Member) {
  selectedMember.value = member
  showAssignDialog.value = true
}

function onRoleAssigned(roleId: string) {
  if (selectedMember.value) {
    if (!selectedMember.value.role_ids) {
      selectedMember.value.role_ids = []
    }
    selectedMember.value.role_ids.push(roleId)
  }
}

// #4257 — the list follows its org, and names this org's roles, not any: a
// list that holds another org's roles is not empty. A role dialog opened on
// the previous org's member is closed: its assign would go to the new org.
watch(
  () => props.tenantId,
  (tenantId, previous) => {
    if (previous !== undefined) {
      showAssignDialog.value = false
      selectedMember.value = null
    }
    void roleStore.ensureRoles(tenantId)
  },
  { immediate: true },
)
</script>
