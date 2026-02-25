<template>
  <v-dialog v-model="show" max-width="450" persistent>
    <v-card>
      <v-card-title>Assign Role</v-card-title>
      <v-card-text>
        <p class="text-body-2 mb-3">
          Assign a role to <strong>{{ memberName }}</strong>
        </p>
        <v-select
          v-model="selectedRoleId"
          :items="availableRoles"
          item-title="name"
          item-value="id"
          label="Role"
          variant="outlined"
          density="comfortable"
        >
          <template #item="{ item, props: itemProps }">
            <v-list-item v-bind="itemProps">
              <template #prepend>
                <v-icon
                  :color="roleStore.colorHex(item.raw.color)"
                  size="small"
                >
                  mdi-shield-account
                </v-icon>
              </template>
            </v-list-item>
          </template>
        </v-select>
      </v-card-text>
      <v-card-actions>
        <v-spacer />
        <v-btn variant="text" @click="cancel">Cancel</v-btn>
        <v-btn
          color="primary"
          variant="flat"
          :disabled="!selectedRoleId"
          :loading="saving"
          @click="assign"
        >
          Assign
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, computed, watch } from 'vue'
import { useRoleStore } from '@/stores/role'

const props = defineProps<{
  modelValue: boolean
  tenantId: string
  userId: string
  memberName: string
  currentRoleIds?: string[]
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
  assigned: [roleId: string]
}>()

const roleStore = useRoleStore()
const selectedRoleId = ref('')
const saving = ref(false)

const show = computed({
  get: () => props.modelValue,
  set: (v) => emit('update:modelValue', v),
})

const availableRoles = computed(() =>
  roleStore.roles.filter((r) => !(props.currentRoleIds || []).includes(r.id)),
)

watch(show, (v) => {
  if (v) {
    selectedRoleId.value = ''
    if (roleStore.roles.length === 0) {
      roleStore.fetchRoles(props.tenantId)
    }
  }
})

function cancel() {
  show.value = false
}

async function assign() {
  if (!selectedRoleId.value) return
  saving.value = true
  try {
    await roleStore.assignRole(props.tenantId, selectedRoleId.value, props.userId)
    emit('assigned', selectedRoleId.value)
    show.value = false
  } finally {
    saving.value = false
  }
}
</script>
