<template>
  <v-dialog :model-value="modelValue" max-width="700" @update:model-value="$emit('update:modelValue', $event)">
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon class="mr-2">mdi-email-multiple</v-icon>
        Batch Invite
      </v-card-title>

      <v-card-text>
        <p class="text-body-2 text-medium-emphasis mb-4">
          Add multiple email invites at once. Each invite can have a different role assigned.
        </p>

        <!-- Invite rows -->
        <div v-for="(row, idx) in rows" :key="idx" class="d-flex align-center ga-3 mb-3">
          <v-text-field
            v-model="row.email"
            label="Email"
            type="email"
            density="compact"
            hide-details
            :rules="[emailRule]"
            class="flex-grow-1"
          />
          <v-select
            v-model="row.role_id"
            :items="assignableRoles"
            item-title="name"
            item-value="id"
            label="Role"
            density="compact"
            hide-details
            style="max-width: 180px"
          />
          <v-btn
            icon
            size="small"
            variant="text"
            color="error"
            :disabled="rows.length <= 1"
            @click="removeRow(idx)"
          >
            <v-icon>mdi-close</v-icon>
          </v-btn>
        </div>

        <v-btn
          variant="text"
          size="small"
          prepend-icon="mdi-plus"
          @click="addRow"
        >
          Add another
        </v-btn>

        <!-- Shared settings -->
        <v-divider class="my-4" />
        <v-text-field
          v-model.number="expiresInHours"
          label="Expires in (hours)"
          type="number"
          min="1"
          density="compact"
          hint="Default: 168 (7 days)"
          persistent-hint
        />

        <!-- Results -->
        <v-alert v-if="resultMessage" :type="resultType" variant="tonal" class="mt-4">
          {{ resultMessage }}
        </v-alert>

        <div v-if="results.length > 0" class="mt-3">
          <div v-for="(r, i) in results" :key="i" class="d-flex align-center text-body-2 mb-1">
            <v-icon
              :color="r.error ? 'error' : 'success'"
              size="small"
              class="mr-2"
            >
              {{ r.error ? 'mdi-close-circle' : 'mdi-check-circle' }}
            </v-icon>
            <span>{{ r.target_email || `Invite #${i + 1}` }}</span>
            <span v-if="r.error" class="text-error ml-2">— {{ r.error }}</span>
          </div>
        </div>
      </v-card-text>

      <v-card-actions>
        <v-spacer />
        <v-btn @click="close">
          {{ results.length > 0 ? 'Done' : 'Cancel' }}
        </v-btn>
        <v-btn
          v-if="results.length === 0"
          color="primary"
          :loading="sending"
          :disabled="!isValid"
          @click="send"
        >
          Send All ({{ validRows.length }})
        </v-btn>
      </v-card-actions>
    </v-card>
  </v-dialog>
</template>

<script setup lang="ts">
import { ref, computed } from 'vue'
import { useInviteStore } from '@/stores/invite'
import { useSnackbar } from '@/composables/useSnackbar'
import type { Role } from '@/stores/role'

interface InviteRow {
  email: string
  role_id: string
}

interface ResultItem {
  target_email?: string
  error?: string
}

const props = defineProps<{
  modelValue: boolean
  tenantId: string
  roles: Role[]
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
}>()

const inviteStore = useInviteStore()
const { showSuccess, showError } = useSnackbar()

const rows = ref<InviteRow[]>([{ email: '', role_id: '' }])
const expiresInHours = ref<number | undefined>(undefined)
const sending = ref(false)
const results = ref<ResultItem[]>([])
const resultMessage = ref('')
const resultType = ref<'success' | 'error' | 'warning'>('success')

const assignableRoles = computed(() =>
  props.roles.filter((r) => !r.is_managed || r.name === 'member'),
)

const emailRule = (v: string) => {
  if (!v) return true // empty rows are filtered out
  return /.+@.+\..+/.test(v) || 'Invalid email'
}

const validRows = computed(() =>
  rows.value.filter((r) => r.email && /.+@.+\..+/.test(r.email)),
)

const isValid = computed(() => validRows.value.length > 0)

function addRow() {
  rows.value.push({ email: '', role_id: '' })
}

function removeRow(idx: number) {
  rows.value.splice(idx, 1)
}

async function send() {
  sending.value = true
  results.value = []
  resultMessage.value = ''
  try {
    const params = validRows.value.map((row) => ({
      target_email: row.email,
      expires_in_hours: expiresInHours.value,
      assign_role_ids: row.role_id ? [row.role_id] : [],
    }))

    const response = await inviteStore.batchCreateInvites(props.tenantId, params)
    results.value = response.results.map((r) => ({
      target_email: r.target_email,
      error: r.error,
    }))

    if (response.failed === 0) {
      resultType.value = 'success'
      resultMessage.value = `All ${response.created} invites created successfully.`
      showSuccess(`${response.created} invites sent`)
    } else if (response.created === 0) {
      resultType.value = 'error'
      resultMessage.value = `All ${response.failed} invites failed.`
      showError('All invites failed')
    } else {
      resultType.value = 'warning'
      resultMessage.value = `${response.created} created, ${response.failed} failed.`
    }
  } catch {
    resultType.value = 'error'
    resultMessage.value = 'Failed to send batch invites.'
    showError('Failed to send batch invites')
  } finally {
    sending.value = false
  }
}

function close() {
  results.value = []
  resultMessage.value = ''
  rows.value = [{ email: '', role_id: '' }]
  expiresInHours.value = undefined
  emit('update:modelValue', false)
}
</script>
