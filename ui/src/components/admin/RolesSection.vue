<template>
  <v-card>
    <v-card-title class="d-flex align-center">
      <span>Roles &amp; Permissions</span>
      <v-spacer />
      <v-btn
        prepend-icon="mdi-plus"
        color="primary"
        variant="flat"
        size="small"
        @click="openCreateDialog"
      >
        New Role
      </v-btn>
    </v-card-title>

    <v-card-text>
      <v-alert
        v-if="roleStore.error"
        type="error"
        variant="tonal"
        closable
        class="mb-4"
        @click:close="roleStore.error = null"
      >
        {{ roleStore.error }}
      </v-alert>

      <p class="text-body-2 text-medium-emphasis mb-4">
        Roles bundle permissions; members can hold several. Mutations need the
        <span class="font-weight-medium">Manage roles</span> permission — the
        server rejects anything else. Assign roles to people in the Members
        section.
      </p>

      <div v-if="roleStore.loading && roleStore.roles.length === 0" class="d-flex justify-center pa-8">
        <v-progress-circular indeterminate />
      </div>

      <p
        v-else-if="roleStore.roles.length === 0"
        class="text-medium-emphasis pa-4 pa-md-6"
      >
        No roles yet — create one to start delegating permissions.
      </p>

      <v-table v-else density="compact">
        <thead>
          <tr>
            <th>Role</th>
            <th>Permissions</th>
            <th class="text-right">Actions</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="role in sortedRoles" :key="role.id">
            <td>
              <div class="d-flex align-center">
                <span
                  class="role-dot mr-2"
                  :style="{ backgroundColor: roleStore.colorHex(role.color) || 'rgb(var(--v-theme-on-surface-variant))' }"
                />
                <div>
                  <div class="font-weight-medium">
                    {{ role.name }}
                    <v-chip v-if="role.is_default" size="x-small" variant="tonal" class="ml-1">default</v-chip>
                    <v-chip v-if="role.is_managed" size="x-small" variant="tonal" class="ml-1">managed</v-chip>
                  </div>
                  <div v-if="role.description" class="text-caption text-medium-emphasis">
                    {{ role.description }}
                  </div>
                </div>
              </div>
            </td>
            <td>
              <v-chip
                v-if="isAdministrator(role.permissions)"
                size="x-small"
                color="warning"
                variant="tonal"
              >
                Administrator (all)
              </v-chip>
              <v-tooltip v-else location="bottom" max-width="420">
                <template #activator="{ props: tip }">
                  <span v-bind="tip" class="text-body-2">
                    {{ permissionSummary(role.permissions) }}
                  </span>
                </template>
                <span>{{ describePermissions(role.permissions).join(' · ') || 'No permissions' }}</span>
              </v-tooltip>
            </td>
            <td class="text-right text-no-wrap">
              <v-btn
                icon="mdi-pencil"
                size="small"
                variant="text"
                :disabled="role.is_managed"
                @click="openEditDialog(role)"
              />
              <v-btn
                icon="mdi-delete"
                size="small"
                variant="text"
                color="error"
                :disabled="role.is_default || role.is_managed"
                @click="openDeleteDialog(role)"
              />
            </td>
          </tr>
        </tbody>
      </v-table>
    </v-card-text>
  </v-card>

  <!-- Create / edit dialog -->
  <v-dialog v-model="editDialog" max-width="900" persistent>
    <v-card>
      <v-card-title>{{ editingId ? 'Edit Role' : 'New Role' }}</v-card-title>
      <v-card-text>
        <v-alert v-if="formError" type="error" variant="tonal" density="compact" class="mb-4">
          {{ formError }}
        </v-alert>

        <v-row dense>
          <v-col cols="12" md="6">
            <v-text-field
              v-model="form.name"
              label="Name"
              variant="outlined"
              density="compact"
              hide-details
            />
          </v-col>
          <v-col cols="12" md="6">
            <v-text-field
              v-model.number="form.position"
              label="Position (lower sorts first)"
              type="number"
              min="0"
              variant="outlined"
              density="compact"
              hide-details
            />
          </v-col>
          <v-col cols="12">
            <v-text-field
              v-model="form.description"
              label="Description (optional)"
              variant="outlined"
              density="compact"
              hide-details
            />
          </v-col>
        </v-row>

        <div class="text-caption text-medium-emphasis mt-4 mb-1">Color</div>
        <div class="d-flex align-center flex-wrap ga-1">
          <!-- 0 is the "no color" sentinel: the server stores it verbatim and
               colorHex(0) renders as colorless, which makes clearing an
               existing color possible (an omitted field is left unchanged). -->
          <v-btn
            v-for="swatch in COLOR_SWATCHES"
            :key="swatch"
            class="swatch"
            :style="swatch !== 0 ? { backgroundColor: hex(swatch) } : undefined"
            :icon="swatch === 0 ? 'mdi-water-off' : form.color === swatch ? 'mdi-check' : undefined"
            :variant="swatch === 0 ? 'outlined' : 'flat'"
            :class="{ 'swatch-selected': form.color === swatch }"
            @click="form.color = swatch"
          />
        </div>

        <div class="d-flex align-center mt-4 mb-1">
          <span class="text-caption text-medium-emphasis">Permissions</span>
          <v-spacer />
          <v-btn size="x-small" variant="text" @click="applyPreset(DEFAULT_MEMBER)">Member defaults</v-btn>
          <v-btn size="x-small" variant="text" @click="applyPreset(DEFAULT_ADMIN)">Admin defaults</v-btn>
          <v-btn size="x-small" variant="text" @click="applyPreset(0)">None</v-btn>
        </div>

        <v-alert
          v-if="(form.permissions & ADMINISTRATOR) !== 0"
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-2"
        >
          Administrator bypasses every other permission check — holders can do
          everything, including managing roles and remote-controlling devices.
        </v-alert>

        <v-row dense>
          <v-col v-for="group in PERMISSION_GROUPS" :key="group" cols="12" sm="6" md="4">
            <div class="text-caption font-weight-medium mb-1">{{ group }}</div>
            <v-checkbox
              v-for="flag in flagsFor(group)"
              :key="flag.key"
              :model-value="(form.permissions & flag.bit) !== 0"
              :label="flag.label"
              density="compact"
              hide-details
              @update:model-value="toggleFlag(flag.bit, $event as boolean)"
            >
              <template v-if="flag.description" #label>
                <v-tooltip location="bottom" max-width="360">
                  <template #activator="{ props: tip }">
                    <span v-bind="tip">{{ flag.label }}</span>
                  </template>
                  <span>{{ flag.description }}</span>
                </v-tooltip>
              </template>
            </v-checkbox>
          </v-col>
        </v-row>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" :disabled="saving" @click="editDialog = false">Cancel</v-btn>
        <v-btn color="primary" variant="flat" :loading="saving" @click="saveRole">
          {{ editingId ? 'Save' : 'Create' }}
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>

  <!-- Delete confirm -->
  <v-dialog v-model="deleteDialog" max-width="500">
    <v-card>
      <v-card-title>Delete role</v-card-title>
      <v-card-text>
        Delete <span class="font-weight-medium">{{ deleteTarget?.name }}</span>?
        Members holding it lose its permissions immediately.
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" :disabled="deleting" @click="deleteDialog = false">Cancel</v-btn>
        <v-btn color="error" variant="flat" :loading="deleting" @click="doDelete">Delete</v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { useRoleStore, type Role } from '@/stores/role'
import {
  ADMINISTRATOR,
  DEFAULT_ADMIN,
  DEFAULT_MEMBER,
  PERMISSION_FLAGS,
  PERMISSION_GROUPS,
  describePermissions,
} from '@/utils/permissions'

const props = defineProps<{ tenantId: string }>()

const roleStore = useRoleStore()

const sortedRoles = computed(() =>
  [...roleStore.roles].sort((a, b) => a.position - b.position || a.name.localeCompare(b.name)),
)

// A small predefined palette (stored as 0xRRGGBB u32; 0 = no color — the
// server round-trips it and colorHex(0) renders colorless).
const COLOR_SWATCHES: number[] = [
  0, 0x1976d2, 0x0d9488, 0x15803d, 0x65a30d, 0xb45309, 0xdc2626, 0xdb2777,
  0x7c3aed, 0x475569,
]

function hex(color: number): string {
  return `#${color.toString(16).padStart(6, '0')}`
}

function isAdministrator(mask: number): boolean {
  return (mask & ADMINISTRATOR) !== 0
}

function permissionSummary(mask: number): string {
  const n = describePermissions(mask).length
  return n === 0 ? 'No permissions' : `${n} permission${n === 1 ? '' : 's'}`
}

function flagsFor(group: string) {
  return PERMISSION_FLAGS.filter((f) => f.group === group)
}

// ── create / edit ──────────────────────────────────────────────────

interface RoleForm {
  name: string
  description: string
  color: number
  position: number
  permissions: number
}

function emptyForm(): RoleForm {
  return { name: '', description: '', color: 0, position: 100, permissions: DEFAULT_MEMBER }
}

const form = reactive<RoleForm>(emptyForm())
const editDialog = ref(false)
const editingId = ref<string | null>(null)
const saving = ref(false)
const formError = ref<string | null>(null)

function openCreateDialog() {
  Object.assign(form, emptyForm())
  editingId.value = null
  formError.value = null
  editDialog.value = true
}

function openEditDialog(role: Role) {
  Object.assign(form, {
    name: role.name,
    description: role.description ?? '',
    color: role.color ?? 0,
    position: role.position,
    permissions: role.permissions,
  })
  editingId.value = role.id
  formError.value = null
  editDialog.value = true
}

function toggleFlag(bit: number, on: boolean) {
  form.permissions = on ? form.permissions | bit : form.permissions & ~bit
}

function applyPreset(mask: number) {
  form.permissions = mask
}

function validateForm(): string | null {
  if (!form.name.trim()) return 'Name is required.'
  if (!Number.isInteger(form.position) || form.position < 0) return 'Position must be ≥ 0.'
  return null
}

function buildPayload() {
  // description/color are sent even when "empty" ('' / 0) — an OMITTED field
  // is left unchanged server-side, so omission would make clearing impossible.
  return {
    name: form.name.trim(),
    description: form.description.trim(),
    color: form.color,
    position: form.position,
    permissions: form.permissions,
  }
}

async function saveRole() {
  const invalid = validateForm()
  if (invalid) {
    formError.value = invalid
    return
  }
  saving.value = true
  formError.value = null
  try {
    if (editingId.value) {
      await roleStore.updateRole(props.tenantId, editingId.value, buildPayload())
    } else {
      await roleStore.createRole(props.tenantId, buildPayload())
    }
    editDialog.value = false
  } catch (e) {
    formError.value = (e as Error).message
  } finally {
    saving.value = false
  }
}

// ── delete ─────────────────────────────────────────────────────────

const deleteDialog = ref(false)
const deleteTarget = ref<Role | null>(null)
const deleting = ref(false)

function openDeleteDialog(role: Role) {
  deleteTarget.value = role
  deleteDialog.value = true
}

async function doDelete() {
  if (!deleteTarget.value) return
  deleting.value = true
  try {
    await roleStore.deleteRole(props.tenantId, deleteTarget.value.id)
    deleteDialog.value = false
  } catch (e) {
    roleStore.error = (e as Error).message
    deleteDialog.value = false
  } finally {
    deleting.value = false
  }
}

onMounted(() => roleStore.fetchRoles(props.tenantId))
</script>

<style scoped>
.role-dot {
  display: inline-block;
  width: 12px;
  height: 12px;
  border-radius: 50%;
  flex: 0 0 12px;
}
.swatch {
  width: 26px;
  height: 26px;
  min-width: 26px;
  border-radius: 50%;
}
.swatch-selected {
  outline: 2px solid rgb(var(--v-theme-primary));
  outline-offset: 1px;
}
</style>
