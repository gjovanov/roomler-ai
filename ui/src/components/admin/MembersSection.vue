<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Members</span>
      <v-spacer />
      <span class="text-body-2 text-medium-emphasis">{{ membersStore.total }} total</span>
    </v-card-title>

    <v-card-text>
      <v-alert
        v-if="membersStore.error"
        type="error"
        variant="tonal"
        closable
        class="mb-4"
        @click:close="membersStore.error = null"
      >
        {{ membersStore.error }}
      </v-alert>

      <!-- Role chips + the assignment dialog need the role list; a failed
           role fetch must not masquerade as "member has no roles". -->
      <v-alert
        v-if="roleStore.error"
        type="error"
        variant="tonal"
        closable
        class="mb-4"
        @click:close="roleStore.error = null"
      >
        Loading roles failed: {{ roleStore.error }}
      </v-alert>

      <p class="text-body-2 text-medium-emphasis mb-4">
        Assign roles to grant permissions — device access (remote control,
        device management, audit) is role-driven. Changing roles needs the
        <span class="font-weight-medium">Manage roles</span> permission. New
        people join via Invites.
      </p>

      <div v-if="membersStore.loading && membersStore.items.length === 0" class="d-flex justify-center pa-8">
        <v-progress-circular indeterminate />
      </div>

      <p
        v-else-if="membersStore.items.length === 0"
        class="text-medium-emphasis pa-4 pa-md-6"
      >
        No members found.
      </p>

      <template v-else>
        <v-table density="compact">
          <thead>
            <tr>
              <th>Member</th>
              <th>Roles</th>
              <th>Joined</th>
              <th class="text-right">Actions</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="member in membersStore.items" :key="member.id">
              <td>
                <div class="font-weight-medium">{{ member.display_name || '(unknown)' }}</div>
                <div v-if="member.nickname" class="text-caption text-medium-emphasis">
                  {{ member.nickname }}
                </div>
              </td>
              <td>
                <template v-if="member.role_ids.length">
                  <v-chip
                    v-for="role in rolesOf(member)"
                    :key="role.id"
                    size="x-small"
                    variant="tonal"
                    class="mr-1 mb-1"
                    :style="chipStyle(role)"
                  >
                    {{ role.name }}
                  </v-chip>
                </template>
                <span v-else class="text-caption text-medium-emphasis">—</span>
              </td>
              <td class="text-no-wrap">{{ joinedLabel(member.joined_at) }}</td>
              <td class="text-right text-no-wrap">
                <v-btn
                  size="small"
                  variant="text"
                  prepend-icon="mdi-shield-account"
                  @click="openRolesDialog(member)"
                >
                  Roles
                </v-btn>
              </td>
            </tr>
          </tbody>
        </v-table>

        <div v-if="membersStore.totalPages > 1" class="d-flex justify-center mt-4">
          <v-pagination
            :model-value="membersStore.page"
            :length="membersStore.totalPages"
            density="compact"
            total-visible="7"
            @update:model-value="(p: number) => membersStore.fetchMembers(props.tenantId, p)"
          />
        </div>
      </template>
    </v-card-text>
  </v-card>

  <!-- Per-member role assignment -->
  <v-dialog v-model="rolesDialog" max-width="500">
    <v-card>
      <v-card-title>
        Roles — {{ dialogMember?.display_name }}
      </v-card-title>
      <v-card-text>
        <v-alert v-if="dialogError" type="error" variant="tonal" density="compact" class="mb-4">
          {{ dialogError }}
        </v-alert>

        <p v-if="roleStore.error" class="text-error">
          Roles couldn't be loaded: {{ roleStore.error }}
        </p>
        <p v-else-if="roleStore.roles.length === 0" class="text-medium-emphasis">
          No roles exist yet — create one in the Roles section first.
        </p>

        <v-list v-else density="compact">
          <v-list-item v-for="role in sortedRoles" :key="role.id" class="px-0">
            <template #prepend>
              <!-- v-checkbox-btn has no `loading` prop (silent no-op) — the
                   in-flight row shows an explicit spinner instead. -->
              <v-progress-circular
                v-if="busyRoleId === role.id"
                indeterminate
                size="20"
                width="2"
                class="mx-3"
              />
              <v-checkbox-btn
                v-else
                :model-value="memberHasRole(role.id)"
                :disabled="busyRoleId !== null"
                @update:model-value="toggleRole(role, $event as boolean)"
              />
            </template>
            <v-list-item-title>
              <span
                class="role-dot mr-2"
                :style="{ backgroundColor: roleStore.colorHex(role.color) || 'rgb(var(--v-theme-on-surface-variant))' }"
              />
              {{ role.name }}
              <v-chip v-if="role.is_default" size="x-small" variant="tonal" class="ml-1">default</v-chip>
            </v-list-item-title>
            <v-list-item-subtitle v-if="role.description">
              {{ role.description }}
            </v-list-item-subtitle>
          </v-list-item>
        </v-list>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="rolesDialog = false">Done</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useMembersStore, type Member } from '@/stores/members'
import { useRoleStore, type Role } from '@/stores/role'

const props = defineProps<{ tenantId: string }>()

const membersStore = useMembersStore()
const roleStore = useRoleStore()

const sortedRoles = computed(() =>
  [...roleStore.roles].sort((a, b) => a.position - b.position || a.name.localeCompare(b.name)),
)

function rolesOf(member: Member): Role[] {
  return sortedRoles.value.filter((r) => member.role_ids.includes(r.id))
}

function chipStyle(role: Role) {
  const color = roleStore.colorHex(role.color)
  return color ? { color } : undefined
}

function joinedLabel(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? '—' : d.toLocaleDateString()
}

// ── per-member role dialog ─────────────────────────────────────────
// Toggles apply IMMEDIATELY (one API call each, matching the endpoint
// granularity); the members store mirrors the change locally so the table
// row updates without a refetch.

const rolesDialog = ref(false)
const dialogMember = ref<Member | null>(null)
const dialogError = ref<string | null>(null)
const busyRoleId = ref<string | null>(null)

function openRolesDialog(member: Member) {
  dialogMember.value = member
  dialogError.value = null
  rolesDialog.value = true
}

function memberHasRole(roleId: string): boolean {
  return dialogMember.value?.role_ids.includes(roleId) ?? false
}

async function toggleRole(role: Role, on: boolean) {
  const member = dialogMember.value
  if (!member || busyRoleId.value) return
  busyRoleId.value = role.id
  dialogError.value = null
  try {
    if (on) {
      await roleStore.assignRole(props.tenantId, role.id, member.user_id)
    } else {
      await roleStore.unassignRole(props.tenantId, role.id, member.user_id)
    }
    membersStore.setMemberRole(member.user_id, role.id, on)
  } catch (e) {
    dialogError.value = (e as Error).message
  } finally {
    busyRoleId.value = null
  }
}

onMounted(() => {
  void membersStore.fetchMembers(props.tenantId)
  // Role names/colors for the chips — cheap and idempotent if the Roles
  // section already loaded them.
  void roleStore.fetchRoles(props.tenantId)
})
</script>

<style scoped>
.role-dot {
  display: inline-block;
  width: 10px;
  height: 10px;
  border-radius: 50%;
}
</style>
