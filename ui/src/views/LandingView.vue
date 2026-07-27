<template>
  <v-theme-provider theme="light">
  <div class="landing-page">
    <!-- Navbar -->
    <v-app-bar flat color="transparent" class="landing-nav">
      <v-toolbar-title class="font-weight-bold text-h5">
        <span class="text-primary">Roomler</span>
      </v-toolbar-title>
      <v-spacer />
      <v-btn variant="text" href="#features" class="d-none d-sm-inline-flex">Features</v-btn>
      <v-btn variant="text" href="#download" class="d-none d-sm-inline-flex">Download</v-btn>
      <v-btn variant="text" href="#pricing" class="d-none d-sm-inline-flex">Pricing</v-btn>
      <v-btn variant="outlined" color="primary" :to="{ name: 'login' }" class="mx-2">Log In</v-btn>
      <v-btn color="primary" :to="{ name: 'register' }" class="d-none d-sm-inline-flex">Get Started Free</v-btn>
      <v-btn color="primary" :to="{ name: 'register' }" class="d-sm-none" size="small">Sign Up</v-btn>
    </v-app-bar>

    <!-- Hero -->
    <section class="hero-section">
      <v-container>
        <v-row align="center" justify="center">
          <v-col cols="12" md="9" class="text-center">
            <h1 class="text-h4 text-md-h2 font-weight-bold mb-4">Every device you own,<br/><span class="text-primary">one secure network</span></h1>
            <p class="text-body-1 text-md-h6 landing-muted mb-6 mb-md-8">Remote desktop from any browser, a private WireGuard-style mesh between your machines, and tunnels into the networks you care about — with team chat and video conferencing included.</p>
            <div class="d-flex flex-wrap justify-center ga-3">
              <v-btn color="primary" size="large" :to="{ name: 'register' }" class="px-6">Start Free</v-btn>
              <v-btn variant="outlined" size="large" href="#download" class="px-6">Download</v-btn>
            </div>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Capability strip (honest, no fabricated stats) -->
    <section class="trust-section py-4 py-md-6">
      <v-container>
        <div class="d-flex flex-wrap justify-center ga-2">
          <v-chip v-for="c in capabilities" :key="c" variant="tonal" color="primary" size="small">{{ c }}</v-chip>
        </div>
      </v-container>
    </section>

    <!-- Pillars -->
    <section id="features" class="features-section py-8 py-md-16">
      <v-container>
        <div v-for="(pillar, pi) in pillars" :key="pillar.title" :class="pi > 0 ? 'mt-10 mt-md-16' : ''">
          <h2 class="text-h4 text-md-h3 text-center font-weight-bold mb-2">{{ pillar.title }}</h2>
          <p class="text-center text-body-1 landing-muted mb-6 mb-md-10">{{ pillar.subtitle }}</p>
          <v-row>
            <v-col v-for="feature in pillar.features" :key="feature.title" cols="12" sm="6" md="4">
              <v-card variant="outlined" class="feature-card pa-4 pa-md-6 h-100" rounded="lg">
                <v-icon :color="feature.color" size="48" class="mb-4">{{ feature.icon }}</v-icon>
                <h3 class="text-h6 font-weight-bold mb-2">{{ feature.title }}</h3>
                <p class="text-body-2 landing-muted">{{ feature.description }}</p>
              </v-card>
            </v-col>
          </v-row>
        </div>
      </v-container>
    </section>

    <!-- Download / install -->
    <section id="download" class="download-section py-8 py-md-16">
      <v-container>
        <h2 class="text-h4 text-md-h3 text-center font-weight-bold mb-2">Set up a device in minutes</h2>
        <p class="text-center text-body-1 landing-muted mb-6 mb-md-12">Run the graphical installer or paste one command. Enrollment tokens come from your workspace (Devices → Enroll device).</p>
        <v-row justify="center">
          <v-col v-for="os in downloads" :key="os.os" cols="12" md="4">
            <v-card variant="outlined" class="pa-4 pa-md-6 h-100 d-flex flex-column" rounded="lg">
              <div class="d-flex align-center mb-3">
                <v-icon size="32" class="mr-2" color="primary">{{ os.icon }}</v-icon>
                <h3 class="text-h6 font-weight-bold">{{ os.title }}</h3>
              </div>
              <v-btn
                :href="os.wizardUrl"
                color="primary"
                variant="tonal"
                prepend-icon="mdi-download"
                class="mb-4 align-self-start"
              >
                Roomler Setup
              </v-btn>
              <p class="text-body-2 landing-muted mb-1">Or from a terminal:</p>
              <pre class="install-cmd text-caption pa-2 rounded flex-grow-1">{{ os.command }}</pre>
            </v-card>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Pricing -->
    <section id="pricing" class="pricing-section py-8 py-md-16">
      <v-container>
        <h2 class="text-h4 text-md-h3 text-center font-weight-bold mb-2">Simple, per-user pricing</h2>
        <p class="text-center text-body-1 landing-muted mb-6 mb-md-12">Every plan includes the private network and remote desktop. Start free, upgrade for more devices.</p>
        <v-row justify="center">
          <v-col v-for="plan in plans" :key="plan.id" cols="12" sm="6" md="4">
            <v-card
              :variant="plan.id === 'pro' ? 'elevated' : 'outlined'"
              :elevation="plan.id === 'pro' ? 8 : 0"
              class="pa-4 pa-md-6 h-100 d-flex flex-column"
              rounded="lg"
              :class="{ 'border-primary': plan.id === 'pro' }"
            >
              <v-chip v-if="plan.id === 'pro'" color="primary" size="small" class="mb-4 align-self-start">Most Popular</v-chip>
              <h3 class="text-h5 font-weight-bold">{{ plan.name }}</h3>
              <div class="my-4">
                <span class="text-h3 font-weight-bold">${{ plan.price_cents / 100 }}</span>
                <span v-if="plan.price_cents > 0" class="text-body-2 landing-muted">/user/mo</span>
                <span v-else class="text-body-2 landing-muted">forever</span>
              </div>
              <v-divider class="mb-4" />
              <v-list density="compact" class="flex-grow-1 bg-transparent">
                <v-list-item v-for="f in plan.features" :key="f" :title="f" prepend-icon="mdi-check" />
              </v-list>
              <v-btn
                :color="plan.id === 'pro' ? 'primary' : undefined"
                :variant="plan.id === 'pro' ? 'flat' : 'outlined'"
                block
                size="large"
                :to="{ name: 'register' }"
                class="mt-4"
              >
                {{ plan.price_cents === 0 ? 'Get Started Free' : 'Start Now' }}
              </v-btn>
            </v-card>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Final CTA -->
    <section class="cta-section py-8 py-md-16">
      <v-container>
        <v-row justify="center">
          <v-col cols="12" md="8" class="text-center">
            <h2 class="text-h4 text-md-h3 font-weight-bold mb-4 text-white">Take your devices with you</h2>
            <p class="text-body-1 cta-subtitle mb-6 mb-md-8">Free for up to 3 devices — remote desktop, private mesh, tunnels, chat and calls.</p>
            <v-btn color="white" size="large" :to="{ name: 'register' }" class="px-6 text-primary">Create Your Workspace — Free</v-btn>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Footer -->
    <v-footer class="landing-footer py-4 py-md-8">
      <v-container>
        <v-row>
          <v-col cols="12" sm="4">
            <div class="text-h6 font-weight-bold mb-2">Roomler</div>
            <p class="text-body-2 landing-muted">Remote access, private networking, and collaboration for your devices and your team.</p>
          </v-col>
          <v-col cols="6" sm="2">
            <div class="text-subtitle-2 font-weight-bold mb-2">Product</div>
            <a href="#features" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Features</a>
            <a href="#download" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Download</a>
            <a href="#pricing" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Pricing</a>
          </v-col>
          <v-col cols="6" sm="2">
            <div class="text-subtitle-2 font-weight-bold mb-2">Install</div>
            <a href="/api/setup/windows" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Windows</a>
            <a href="/api/setup/macos" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">macOS</a>
            <a href="/api/setup/linux" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Linux</a>
          </v-col>
          <v-col cols="12" sm="4">
            <div class="text-subtitle-2 font-weight-bold mb-2">Legal</div>
            <router-link to="/privacy" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Privacy Policy</router-link>
            <router-link to="/terms" class="text-body-2 landing-muted mb-1 d-block text-decoration-none">Terms of Service</router-link>
          </v-col>
        </v-row>
        <v-divider class="my-4" />
        <div class="text-body-2 landing-muted text-center">&copy; {{ new Date().getFullYear() }} Roomler. All rights reserved.</div>
      </v-container>
    </v-footer>
  </div>
  </v-theme-provider>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { enrollCommands } from '@/utils/enrollCommands'

const route = useRoute()

const capabilities = [
  'Remote desktop',
  'Private mesh network',
  'Tunnels & SOCKS5',
  'Exit nodes',
  'MagicDNS',
  'Chat & video included',
]

// Pivot order: remote access first, the private network second,
// collaboration as the included bonus.
const pillars = [
  {
    title: 'Reach any of your devices',
    subtitle: 'A TeamViewer-style remote desktop that lives in your browser',
    features: [
      {
        icon: 'mdi-monitor-eye',
        title: 'Remote desktop in the browser',
        description: 'Hardware-encoded H.264/HEVC/VP9 with sub-100 ms input latency. No viewer install — any Chromium browser is the controller.',
        color: '#009688',
      },
      {
        icon: 'mdi-shield-lock-outline',
        title: 'Works behind strict networks',
        description: 'Direct peer-to-peer when possible; TURN relays and WebSocket fallbacks punch through corporate firewalls and full-tunnel VPNs.',
        color: '#ef5350',
      },
      {
        icon: 'mdi-monitor-multiple',
        title: 'Fleet management built in',
        description: 'Enroll unattended machines, push updates from the web, transfer files and clipboard, and audit every session.',
        color: '#009688',
      },
    ],
  },
  {
    title: 'Your own private network',
    subtitle: 'A Tailscale-style overlay mesh between everything you enroll',
    features: [
      {
        icon: 'mdi-lan',
        title: 'WireGuard-style mesh',
        description: 'Every device gets a stable private address. Traffic flows directly between machines with NAT hole-punching and encrypted end to end.',
        color: '#ef5350',
      },
      {
        icon: 'mdi-router-network',
        title: 'Subnet routers & exit nodes',
        description: 'Expose a whole LAN through one machine, or route all your traffic through a trusted exit node when you travel.',
        color: '#009688',
      },
      {
        icon: 'mdi-dns-outline',
        title: 'MagicDNS & tunnels',
        description: 'Reach machines by name, forward ports, and run SOCKS5 tunnels into networks only one of your devices can see.',
        color: '#ef5350',
      },
    ],
  },
  {
    title: 'Collaboration included',
    subtitle: 'The team layer is part of every plan — not an add-on',
    features: [
      {
        icon: 'mdi-pound',
        title: 'Rooms, chat & threads',
        description: 'Organized rooms with threaded messaging, reactions, mentions and file attachments.',
        color: '#009688',
      },
      {
        icon: 'mdi-video-outline',
        title: 'HD video conferencing',
        description: 'Built-in SFU for crystal-clear meetings with screen sharing and recordings.',
        color: '#ef5350',
      },
      {
        icon: 'mdi-file-document-outline',
        title: 'Files, cloud & AI',
        description: 'File sharing with cloud-storage sync and AI-powered document recognition.',
        color: '#009688',
      },
    ],
  },
]

// One-line installs — same vitest-locked template source the in-app
// enrollment dialog uses (token placeholder until they have one).
const osMeta: Record<string, { icon: string; wizard: string }> = {
  windows: { icon: 'mdi-microsoft-windows', wizard: '/api/setup/windows' },
  linux: { icon: 'mdi-linux', wizard: '/api/setup/linux' },
  macos: { icon: 'mdi-apple', wizard: '/api/setup/macos' },
}
const downloads = enrollCommands('agent', window.location.origin, null).map((os) => ({
  os: os.os,
  title: os.title,
  icon: osMeta[os.os]!.icon,
  wizardUrl: osMeta[os.os]!.wizard,
  command: os.blocks[0]!.command,
}))

interface LandingPlan {
  id: string
  name: string
  price_cents: number
  features: string[]
}

// Static fallback mirrors the server matrix so the page renders even if
// the API is briefly unreachable; replaced by /api/stripe/plans (the
// single source of truth) as soon as the fetch lands.
const plans = ref<LandingPlan[]>([
  { id: 'free', name: 'Free', price_cents: 0, features: ['3 devices', 'Private network (overlay mesh)', 'Chat: 10 members'] },
  { id: 'pro', name: 'Pro', price_cents: 800, features: ['30 devices', 'Exit nodes + MagicDNS', 'Unlimited members'] },
  { id: 'business', name: 'Business', price_cents: 1600, features: ['300 devices', 'Everything in Pro', 'Priority support'] },
])

onMounted(async () => {
  try {
    const resp = await fetch('/api/stripe/plans')
    if (resp.ok) {
      const live = (await resp.json()) as LandingPlan[]
      if (Array.isArray(live) && live.length > 0) plans.value = live
    }
  } catch {
    // fallback copy stays
  }
  // The /pricing route renders this same view — land on the plans.
  if (route.name === 'pricing') {
    document.getElementById('pricing')?.scrollIntoView({ behavior: 'auto' })
  }
})
</script>

<style scoped>
.landing-page {
  background: linear-gradient(180deg, #f5faf9 0%, #ffffff 40%);
  color: #1a1a2e;
}

.landing-nav {
  position: fixed !important;
  z-index: 100;
  backdrop-filter: blur(12px);
  background: rgba(255, 255, 255, 0.92) !important;
  border-bottom: 1px solid rgba(0, 150, 136, 0.1);
  color: #1a1a2e !important;
}

.hero-section {
  padding-top: 96px;
  padding-bottom: 48px;
  background: linear-gradient(135deg, #e0f2f1 0%, #e8f5e9 50%, #e0f7fa 100%);
  position: relative;
  overflow: hidden;
}
@media (min-width: 960px) {
  .hero-section {
    padding-top: 140px;
    padding-bottom: 80px;
  }
}

.hero-section::before {
  content: '';
  position: absolute;
  top: -50%;
  left: -50%;
  width: 200%;
  height: 200%;
  background: radial-gradient(circle at 30% 70%, rgba(0, 150, 136, 0.06) 0%, transparent 50%),
              radial-gradient(circle at 70% 30%, rgba(239, 83, 80, 0.06) 0%, transparent 50%);
  animation: float 20s ease-in-out infinite;
}

@keyframes float {
  0%, 100% { transform: translate(0, 0); }
  50% { transform: translate(-2%, 2%); }
}

.trust-section {
  background: #fafafa;
  border-top: 1px solid rgba(0, 0, 0, 0.06);
  border-bottom: 1px solid rgba(0, 0, 0, 0.06);
}

.features-section {
  background: #ffffff;
}

.download-section {
  background: linear-gradient(180deg, #ffffff 0%, #f5faf9 100%);
}

.install-cmd {
  background: #1a1a2e;
  color: #e0f2f1;
  font-family: monospace;
  white-space: pre-wrap;
  word-break: break-all;
  margin: 0;
}

.feature-card {
  transition: transform 0.2s ease, box-shadow 0.2s ease;
  border-color: rgba(0, 0, 0, 0.08) !important;
}

.feature-card:hover {
  transform: translateY(-4px);
  box-shadow: 0 12px 40px rgba(0, 150, 136, 0.12) !important;
}

.pricing-section {
  background: linear-gradient(180deg, #f5f5f5 0%, #fafafa 100%);
}

.border-primary {
  border: 2px solid #009688 !important;
}

.cta-section {
  background: linear-gradient(135deg, #009688 0%, #00796B 100%);
  position: relative;
  overflow: hidden;
}

.cta-section::before {
  content: '';
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  bottom: 0;
  background: radial-gradient(circle at 20% 80%, rgba(255, 255, 255, 0.1) 0%, transparent 50%),
              radial-gradient(circle at 80% 20%, rgba(255, 255, 255, 0.08) 0%, transparent 50%);
}

.cta-subtitle {
  color: rgba(255, 255, 255, 0.85);
}

.landing-footer {
  background: #fafafa !important;
  border-top: 1px solid rgba(0, 0, 0, 0.06);
}

.landing-muted {
  color: rgba(26, 26, 46, 0.7) !important;
}

.landing-page :deep(.v-card) {
  background-color: #ffffff !important;
  color: #1a1a2e !important;
}

.landing-page :deep(.v-list) {
  color: #1a1a2e !important;
}

.landing-page :deep(.v-footer) {
  color: #1a1a2e !important;
}
</style>
