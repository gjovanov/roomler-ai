<template>
  <div class="d-flex flex-column ga-4">
    <v-card>
      <v-card-title>Workspace Settings</v-card-title>
      <v-card-text>
        <v-text-field
          :model-value="tenantStore.current?.name"
          label="Workspace Name"
          disabled
        />
        <v-text-field
          :model-value="tenantStore.current?.slug"
          label="Slug"
          disabled
        />
      </v-card-text>
    </v-card>

    <!-- Fleet RPC gate 1. Deliberately here rather than in the device list:
         this one switch governs every device in the org, so it belongs with
         org settings and needs MANAGE_TENANT, not fleet-admin rights. -->
    <v-card>
      <v-card-title class="d-flex align-center">
        <v-icon icon="mdi-console-network-outline" color="primary" class="mr-2" />
        Remote command execution
      </v-card-title>
      <v-card-text>
        <v-alert
          type="warning"
          variant="tonal"
          density="compact"
          class="mb-4"
          icon="mdi-shield-alert-outline"
        >
          When enabled, users holding the <strong>EXEC_DEVICE</strong>
          permission can run commands on devices that individually opt in.
          Commands run as <strong>SYSTEM</strong> (Windows) or
          <strong>root</strong> (Linux) and are all recorded in the audit log
          below.
        </v-alert>

        <v-switch
          :model-value="agentStore.orgExecEnabled === true"
          :loading="saving"
          :disabled="saving || agentStore.orgExecEnabled === null"
          color="primary"
          density="compact"
          hide-details
          label="Allow remote command execution in this organization"
          @update:model-value="onToggle"
        />
        <div class="text-caption text-medium-emphasis mt-1">
          Off by default. Turning it on does not enable any device on its own —
          each device must also be allowed individually, and the device's own
          <code>exec_enabled</code> setting must permit it.
        </div>

        <v-alert v-if="error" type="error" variant="tonal" density="compact" class="mt-3">
          {{ error }}
        </v-alert>
      </v-card-text>
    </v-card>

    <ExecAuditSection :tenant-id="tenantId" />
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useTenantStore } from '@/stores/tenant'
import { useAgentStore } from '@/stores/agents'
import ExecAuditSection from './ExecAuditSection.vue'

const props = defineProps<{ tenantId: string }>()
const tenantStore = useTenantStore()
const agentStore = useAgentStore()

const saving = ref(false)
const error = ref<string | null>(null)

async function onToggle(v: boolean | null) {
  saving.value = true
  error.value = null
  try {
    await agentStore.setOrgExecEnabled(props.tenantId, v === true)
  } catch (e) {
    error.value = (e as Error).message
  } finally {
    saving.value = false
  }
}

onMounted(() => {
  void agentStore.fetchOrgExecEnabled(props.tenantId)
})
</script>
