<template>
  <v-container fluid class="py-8">
    <h1 class="text-h4 font-weight-bold mb-2">Billing & Subscription</h1>
    <p class="text-body-1 text-medium-emphasis mb-8">Manage your workspace's plan and billing</p>

    <v-alert v-if="$route.query.success" type="success" closable class="mb-6">
      Subscription updated successfully! Changes may take a moment to reflect.
    </v-alert>
    <v-alert v-if="$route.query.canceled" type="info" closable class="mb-6">
      Checkout was canceled. No changes were made.
    </v-alert>

    <!-- Current Plan -->
    <v-card variant="outlined" class="mb-8 pa-6">
      <div class="d-flex align-center justify-space-between flex-wrap ga-4">
        <div>
          <div class="text-overline text-medium-emphasis">Current Plan</div>
          <div class="text-h5 font-weight-bold text-capitalize">{{ currentPlan }}</div>
          <div v-if="billing?.status" class="mt-1">
            <v-chip
              :color="billing.status === 'active' ? 'success' : 'warning'"
              size="small"
              variant="tonal"
            >
              {{ billing.status }}
            </v-chip>
            <span v-if="billing.cancel_at_period_end" class="text-body-2 text-warning ml-2">
              Cancels at period end
            </span>
          </div>
          <div v-if="billing?.current_period_end" class="text-body-2 text-medium-emphasis mt-1">
            Current period ends: {{ new Date(billing.current_period_end).toLocaleDateString() }}
          </div>
        </div>
        <v-btn
          v-if="billing?.customer_id"
          variant="outlined"
          @click="openPortal"
          :loading="portalLoading"
        >
          Manage Subscription
        </v-btn>
      </div>
    </v-card>

    <!-- Plans -->
    <h2 class="text-h5 font-weight-bold mb-4">Available Plans</h2>
    <v-row>
      <v-col v-for="plan in plans" :key="plan.id" cols="12" sm="6" md="4">
        <v-card
          :variant="plan.id === currentPlan ? 'elevated' : 'outlined'"
          :elevation="plan.id === currentPlan ? 4 : 0"
          class="pa-6 h-100 d-flex flex-column"
          rounded="lg"
          :class="{ 'border-primary': plan.id === currentPlan }"
        >
          <div class="text-h6 font-weight-bold">{{ plan.name }}</div>
          <div class="my-4">
            <span class="text-h4 font-weight-bold">${{ (plan.price_cents / 100).toFixed(0) }}</span>
            <span v-if="plan.price_cents > 0" class="text-body-2 text-medium-emphasis">/mo</span>
            <span v-else class="text-body-2 text-medium-emphasis">forever</span>
          </div>
          <v-divider class="mb-4" />
          <v-list density="compact" class="flex-grow-1 bg-transparent">
            <v-list-item title="Members" :subtitle="plan.limits.max_members >= 4294967295 ? 'Unlimited' : String(plan.limits.max_members)" prepend-icon="mdi-account-group" />
            <v-list-item title="Channels" :subtitle="plan.limits.max_channels >= 4294967295 ? 'Unlimited' : String(plan.limits.max_channels)" prepend-icon="mdi-pound" />
            <v-list-item title="Video Participants" :subtitle="plan.limits.video_max_participants === 0 ? 'None' : String(plan.limits.video_max_participants)" prepend-icon="mdi-video" />
            <v-list-item title="Cloud Integrations" :subtitle="plan.limits.cloud_integrations ? 'Yes' : 'No'" :prepend-icon="plan.limits.cloud_integrations ? 'mdi-check-circle' : 'mdi-close-circle'" />
            <v-list-item title="AI Recognition" :subtitle="plan.limits.ai_recognition ? 'Yes' : 'No'" :prepend-icon="plan.limits.ai_recognition ? 'mdi-check-circle' : 'mdi-close-circle'" />
            <v-list-item title="Recordings" :subtitle="plan.limits.recordings ? 'Yes' : 'No'" :prepend-icon="plan.limits.recordings ? 'mdi-check-circle' : 'mdi-close-circle'" />
          </v-list>
          <v-btn
            v-if="plan.id !== 'free' && plan.id !== currentPlan"
            color="primary"
            block
            size="large"
            class="mt-4"
            @click="checkout(plan.id)"
            :loading="checkoutLoading === plan.id"
          >
            {{ currentPlan === 'free' ? 'Upgrade' : 'Switch' }} to {{ plan.name }}
          </v-btn>
          <v-btn
            v-else-if="plan.id === currentPlan"
            color="primary"
            variant="tonal"
            block
            size="large"
            class="mt-4"
            disabled
          >
            Current Plan
          </v-btn>
        </v-card>
      </v-col>
    </v-row>
  </v-container>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useRoute } from 'vue-router'
import { useTenantStore } from '@/stores/tenant'
import { api } from '@/api/client'

const route = useRoute()
const tenantStore = useTenantStore()

interface PlanInfo {
  id: string
  name: string
  price_cents: number
  limits: {
    max_members: number
    max_channels: number
    max_message_history: number
    storage_bytes: number
    video_max_participants: number
    cloud_integrations: boolean
    ai_recognition: boolean
    recordings: boolean
  }
}

const plans = ref<PlanInfo[]>([])
const portalLoading = ref(false)
const checkoutLoading = ref<string | null>(null)

const currentTenant = computed(() => tenantStore.current)
const currentPlan = computed(() => currentTenant.value?.plan || 'free')
const billing = computed(() => currentTenant.value?.billing || {})

async function fetchPlans() {
  try {
    plans.value = await api.get<PlanInfo[]>('/stripe/plans')
  } catch (e) {
    console.error('Failed to fetch plans:', e)
  }
}

async function checkout(planId: string) {
  if (!currentTenant.value?.id) return
  checkoutLoading.value = planId
  try {
    const result = await api.post<{ url: string }>('/stripe/checkout', {
      tenant_id: currentTenant.value.id,
      plan: planId,
      success_url: `${window.location.origin}/tenant/${currentTenant.value.id}/billing?success=true`,
      cancel_url: `${window.location.origin}/tenant/${currentTenant.value.id}/billing?canceled=true`,
    })
    if (result.url) window.location.href = result.url
  } catch (e) {
    console.error('Checkout failed:', e)
  } finally {
    checkoutLoading.value = null
  }
}

async function openPortal() {
  if (!currentTenant.value?.id) return
  portalLoading.value = true
  try {
    const result = await api.post<{ url: string }>('/stripe/portal', {
      tenant_id: currentTenant.value.id,
      return_url: `${window.location.origin}/tenant/${currentTenant.value.id}/billing`,
    })
    if (result.url) window.location.href = result.url
  } catch (e) {
    console.error('Portal failed:', e)
  } finally {
    portalLoading.value = false
  }
}

onMounted(fetchPlans)
</script>
