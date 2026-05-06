<template>
  <v-card>
    <v-card-title>Background Tasks</v-card-title>
    <v-card-text>
      <v-table v-if="taskStore.tasks.length > 0">
        <thead>
          <tr>
            <th>Type</th>
            <th>Status</th>
            <th>Progress</th>
            <th>Created</th>
            <th>Actions</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="t in taskStore.tasks" :key="t.id">
            <td>{{ t.task_type }}</td>
            <td>
              <v-chip size="small" :color="taskColor(t.status)">{{ t.status }}</v-chip>
            </td>
            <td>
              <v-progress-linear :model-value="t.progress" :color="taskColor(t.status)" />
            </td>
            <td>{{ new Date(t.created_at).toLocaleString() }}</td>
            <td>
              <v-btn
                v-if="t.status === 'Completed' && t.file_name"
                icon="mdi-download"
                size="small"
                variant="text"
                :href="taskStore.downloadUrl(props.tenantId, t.id)"
              />
            </td>
          </tr>
        </tbody>
      </v-table>
      <div v-else class="text-center pa-4 pa-md-6 text-medium-emphasis">
        No background tasks
      </div>
    </v-card-text>
  </v-card>
</template>

<script setup lang="ts">
import { onMounted, watch } from 'vue'
import { useTaskStore } from '@/stores/tasks'

const props = defineProps<{ tenantId: string }>()
const taskStore = useTaskStore()

function taskColor(status: string): string {
  switch (status) {
    case 'Completed': return 'success'
    case 'Processing': return 'primary'
    case 'Failed': return 'error'
    default: return 'warning'
  }
}

onMounted(() => {
  taskStore.fetchTasks(props.tenantId)
})

watch(() => props.tenantId, (tid) => {
  if (tid) taskStore.fetchTasks(tid)
})
</script>
