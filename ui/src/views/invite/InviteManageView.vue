<template>
  <v-container>
    <v-row>
      <v-col>
        <div class="d-flex align-center mb-4">
          <h2 class="text-h5">Invite Links</h2>
          <v-spacer />
          <v-btn color="primary" @click="showCreateDialog = true">
            <v-icon start>mdi-plus</v-icon>
            Create Invite
          </v-btn>
        </div>

        <v-alert v-if="inviteStore.error" type="error" density="compact" class="mb-4">
          {{ inviteStore.error }}
        </v-alert>

        <v-card>
          <v-data-table-virtual
            :headers="headers"
            :items="inviteStore.invites"
            :loading="inviteStore.loading"
          >
            <template #item.code="{ item }">
              <div class="d-flex align-center">
                <code class="text-body-2">{{ item.code }}</code>
                <v-btn
                  icon
                  size="x-small"
                  variant="text"
                  @click="copyLink(item.code)"
                >
                  <v-icon size="16">mdi-content-copy</v-icon>
                </v-btn>
              </div>
            </template>
            <template #item.status="{ item }">
              <v-chip
                :color="statusColor(item.status)"
                size="small"
                variant="tonal"
              >
                {{ item.status }}
              </v-chip>
            </template>
            <template #item.usage="{ item }">
              {{ item.use_count }} / {{ item.max_uses ?? 'unlimited' }}
            </template>
            <template #item.target_email="{ item }">
              {{ item.target_email || 'Anyone' }}
            </template>
            <template #item.actions="{ item }">
              <v-btn
                v-if="item.status === 'active'"
                icon
                size="small"
                variant="text"
                color="error"
                @click="handleRevoke(item.id)"
              >
                <v-icon>mdi-close-circle</v-icon>
              </v-btn>
            </template>
          </v-data-table-virtual>
        </v-card>

        <!-- Create invite dialog -->
        <v-dialog v-model="showCreateDialog" max-width="500">
          <v-card>
            <v-card-title>Create Invite</v-card-title>
            <v-card-text>
              <v-radio-group v-model="inviteType" inline class="mb-4">
                <v-radio label="Shareable Link" value="link" />
                <v-radio label="Email Invite" value="email" />
              </v-radio-group>

              <v-text-field
                v-if="inviteType === 'email'"
                v-model="targetEmail"
                label="Email address"
                type="email"
              />

              <v-text-field
                v-if="inviteType === 'link'"
                v-model.number="maxUses"
                label="Max uses (leave empty for unlimited)"
                type="number"
                min="1"
              />

              <v-text-field
                v-model.number="expiresInHours"
                label="Expires in (hours)"
                type="number"
                min="1"
                :placeholder="'168 (7 days)'"
              />
            </v-card-text>
            <v-card-actions>
              <v-spacer />
              <v-btn @click="showCreateDialog = false">Cancel</v-btn>
              <v-btn color="primary" :loading="creating" @click="handleCreate">
                Create
              </v-btn>
            </v-card-actions>
          </v-card>
        </v-dialog>

        <!-- Copy success snackbar -->
        <v-snackbar v-model="showCopied" :timeout="2000" color="success">
          Invite link copied to clipboard
        </v-snackbar>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useRoute } from 'vue-router'
import { useInviteStore } from '@/stores/invite'

const route = useRoute()
const inviteStore = useInviteStore()

const tenantId = ref(route.params.tenantId as string)
const showCreateDialog = ref(false)
const showCopied = ref(false)
const creating = ref(false)

const inviteType = ref<'link' | 'email'>('link')
const targetEmail = ref('')
const maxUses = ref<number | undefined>(undefined)
const expiresInHours = ref<number | undefined>(undefined)

const headers = [
  { title: 'Code', key: 'code', sortable: false },
  { title: 'Status', key: 'status' },
  { title: 'Target', key: 'target_email', sortable: false },
  { title: 'Usage', key: 'usage', sortable: false },
  { title: 'Created', key: 'created_at' },
  { title: '', key: 'actions', sortable: false, width: 60 },
]

function statusColor(status: string) {
  switch (status) {
    case 'active':
      return 'success'
    case 'revoked':
      return 'error'
    case 'exhausted':
      return 'warning'
    case 'expired':
      return 'grey'
    default:
      return 'grey'
  }
}

function copyLink(code: string) {
  const url = `${window.location.origin}/invite/${code}`
  navigator.clipboard.writeText(url)
  showCopied.value = true
}

async function handleCreate() {
  creating.value = true
  try {
    await inviteStore.createInvite(tenantId.value, {
      target_email: inviteType.value === 'email' ? targetEmail.value : undefined,
      max_uses: inviteType.value === 'link' ? maxUses.value : undefined,
      expires_in_hours: expiresInHours.value,
    })
    showCreateDialog.value = false
    targetEmail.value = ''
    maxUses.value = undefined
    expiresInHours.value = undefined
  } finally {
    creating.value = false
  }
}

async function handleRevoke(inviteId: string) {
  await inviteStore.revokeInvite(tenantId.value, inviteId)
}

onMounted(() => {
  inviteStore.listInvites(tenantId.value)
})
</script>
