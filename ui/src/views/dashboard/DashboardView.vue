<template>
  <v-container fluid class="pa-2 pa-md-4 pa-xl-6">
    <h1 class="text-h5 text-md-h4 mb-2 mb-md-4">{{ $t('nav.dashboard') }}</h1>

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
      <!-- Creating a SECOND org used to be impossible: the create form
           only rendered while you had zero tenants. -->
      <v-col cols="12" sm="6" md="4">
        <v-card
          variant="outlined"
          hover
          class="d-flex align-center justify-center"
          style="min-height: 96px; cursor: pointer"
          @click="showCreate = true"
        >
          <div class="text-center text-medium-emphasis">
            <v-icon size="28">mdi-plus</v-icon>
            <div>New organization</div>
          </div>
        </v-card>
      </v-col>
    </v-row>

    <v-dialog v-model="showCreate" max-width="420">
      <v-card>
        <v-card-title>New organization</v-card-title>
        <v-card-text>
          <v-form ref="formRef" @submit.prevent="handleCreate">
            <v-text-field v-model="name" label="Name" :rules="[rules.required]" @update:model-value="autoSlug" />
            <v-text-field v-model="slug" label="Slug" hint="URL-friendly identifier, globally unique" :rules="[rules.required, rules.slug]" @update:model-value="slugTouched = true" />
          </v-form>
        </v-card-text>
        <v-card-actions>
          <v-spacer />
          <v-btn variant="text" @click="showCreate = false">Cancel</v-btn>
          <v-btn color="primary" @click="handleCreate">Create</v-btn>
        </v-card-actions>
      </v-card>
    </v-dialog>
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
const showCreate = ref(false)
const slugTouched = ref(false)

function autoSlug() {
  if (!slugTouched.value) {
    slug.value = name.value
      .toLowerCase()
      .trim()
      .replace(/[^a-z0-9]+/g, '-')
      .replace(/^-+|-+$/g, '')
  }
}

async function handleCreate() {
  const { valid } = await formRef.value.validate()
  if (!valid) return
  try {
    const tenant = await tenantStore.createTenant(name.value, slug.value)
    showSuccess('Workspace created')
    router.push(`/tenant/${tenant.id}`)
  } catch (e) {
    const msg = e instanceof Error ? e.message : 'Failed to create workspace'
    // tenants.slug is globally unique — put the dup-key case in words.
    showError(
      msg.includes('duplicate') || msg.includes('E11000')
        ? 'That slug is already taken — pick another'
        : msg,
    )
  }
}

onMounted(() => {
  tenantStore.fetchTenants()
})
</script>
