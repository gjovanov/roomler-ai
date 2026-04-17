<template>
  <v-container>
    <v-row>
      <v-col cols="12">
        <h1 class="text-h4 mb-4">{{ $t('nav.admin') }}</h1>
      </v-col>
    </v-row>

    <v-row>
      <v-col cols="12" md="3">
        <v-list density="compact" nav>
          <v-list-item
            v-for="item in adminSections"
            :key="item.id"
            :prepend-icon="item.icon"
            :title="item.title"
            :active="activeSection === item.id"
            @click="activeSection = item.id"
          />
        </v-list>
      </v-col>

      <v-col cols="12" md="9">
        <!-- Workspace settings -->
        <v-card v-if="activeSection === 'settings'">
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

        <!-- Members -->
        <v-card v-if="activeSection === 'members'">
          <v-card-title>Members</v-card-title>
          <v-card-text>
            <v-list>
              <v-list-item
                prepend-icon="mdi-account"
                title="Member management coming soon"
                subtitle="Invite, remove, and manage member roles"
              />
            </v-list>
          </v-card-text>
        </v-card>

        <!-- Roles -->
        <v-card v-if="activeSection === 'roles'">
          <v-card-title>Roles & Permissions</v-card-title>
          <v-card-text>
            <v-list>
              <v-list-item
                prepend-icon="mdi-shield-account"
                title="Role management coming soon"
                subtitle="Create, edit, and assign roles with permission bitfields"
              />
            </v-list>
          </v-card-text>
        </v-card>

        <!-- Background tasks -->
        <v-card v-if="activeSection === 'tasks'">
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
                      :href="taskStore.downloadUrl(tenantId, t.id)"
                    />
                  </td>
                </tr>
              </tbody>
            </v-table>
            <div v-else class="text-center pa-4 text-medium-emphasis">
              No background tasks
            </div>
          </v-card-text>
        </v-card>

        <!-- Remote-control agents -->
        <AgentsSection v-if="activeSection === 'agents'" :tenant-id="tenantId" />

        <!-- Audit log -->
        <v-card v-if="activeSection === 'audit'">
          <v-card-title>Audit Log</v-card-title>
          <v-card-text>
            <v-list>
              <v-list-item
                prepend-icon="mdi-clipboard-text-clock"
                title="Audit log coming soon"
                subtitle="Track all administrative actions"
              />
            </v-list>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { useTaskStore } from '@/stores/tasks'
import AgentsSection from '@/components/admin/AgentsSection.vue'

const route = useRoute()
const tenantStore = useTenantStore()
const taskStore = useTaskStore()

const tenantId = computed(() => route.params.tenantId as string)
const activeSection = ref('settings')

const adminSections = [
  { id: 'settings', icon: 'mdi-cog', title: 'Settings' },
  { id: 'members', icon: 'mdi-account-group', title: 'Members' },
  { id: 'roles', icon: 'mdi-shield-account', title: 'Roles' },
  { id: 'agents', icon: 'mdi-desktop-classic', title: 'Agents' },
  { id: 'tasks', icon: 'mdi-progress-clock', title: 'Tasks' },
  { id: 'audit', icon: 'mdi-clipboard-text-clock', title: 'Audit Log' },
]

function taskColor(status: string): string {
  switch (status) {
    case 'Completed': return 'success'
    case 'Processing': return 'primary'
    case 'Failed': return 'error'
    default: return 'warning'
  }
}

onMounted(() => {
  taskStore.fetchTasks(tenantId.value)
})
</script>
