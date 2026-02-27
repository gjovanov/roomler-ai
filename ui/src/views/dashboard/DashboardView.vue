<template>
  <v-container>
    <v-row>
      <v-col cols="12">
        <h1 class="text-h4 mb-4">{{ $t('nav.dashboard') }}</h1>
      </v-col>
    </v-row>

    <v-row v-if="tenantStore.tenants.length === 0">
      <v-col cols="12" md="6">
        <v-card>
          <v-card-title>Create Your First Workspace</v-card-title>
          <v-card-text>
            <v-form ref="formRef" @submit.prevent="handleCreate">
              <v-text-field v-model="name" label="Workspace Name" :rules="[rules.required]" />
              <v-text-field v-model="slug" label="Slug" hint="URL-friendly identifier" :rules="[rules.required, rules.slug]" />
              <v-btn type="submit" color="primary" class="mt-2">Create</v-btn>
            </v-form>
          </v-card-text>
        </v-card>
      </v-col>
    </v-row>

    <v-row v-else>
      <v-col v-for="t in tenantStore.tenants" :key="t.id" cols="12" sm="6" md="4">
        <v-card :to="`/tenant/${t.id}`" hover>
          <v-card-title>
            <v-icon class="mr-2">mdi-domain</v-icon>
            {{ t.name }}
          </v-card-title>
          <v-card-subtitle>{{ t.slug }}</v-card-subtitle>
          <v-card-text v-if="t.description">{{ t.description }}</v-card-text>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { useRouter } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { useSnackbar } from '@/composables/useSnackbar'
import { useValidation } from '@/composables/useValidation'

const tenantStore = useTenantStore()
const router = useRouter()
const { showSuccess, showError } = useSnackbar()
const { rules } = useValidation()

const formRef = ref()
const name = ref('')
const slug = ref('')

async function handleCreate() {
  const { valid } = await formRef.value.validate()
  if (!valid) return
  try {
    const tenant = await tenantStore.createTenant(name.value, slug.value)
    showSuccess('Workspace created')
    router.push(`/tenant/${tenant.id}`)
  } catch (e) {
    showError(e instanceof Error ? e.message : 'Failed to create workspace')
  }
}

onMounted(() => {
  tenantStore.fetchTenants()
})
</script>
