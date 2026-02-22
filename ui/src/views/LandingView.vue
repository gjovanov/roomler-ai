<template>
  <v-theme-provider theme="light">
  <div class="landing-page">
    <!-- Navbar -->
    <v-app-bar flat color="transparent" class="landing-nav">
      <v-toolbar-title class="font-weight-bold text-h5">
        <span class="text-primary">Roomler</span>
      </v-toolbar-title>
      <v-spacer />
      <v-btn variant="text" href="#features">Features</v-btn>
      <v-btn variant="text" href="#pricing">Pricing</v-btn>
      <v-btn variant="outlined" color="primary" :to="{ name: 'login' }" class="mx-2">Log In</v-btn>
      <v-btn color="primary" :to="{ name: 'register' }">Get Started Free</v-btn>
    </v-app-bar>

    <!-- Hero -->
    <section class="hero-section">
      <v-container>
        <v-row align="center" justify="center">
          <v-col cols="12" md="8" class="text-center">
            <h1 class="text-h2 font-weight-bold mb-4">Where teams connect, collaborate,<br/>and create — <span class="text-primary">in real time</span></h1>
            <p class="text-h6 landing-muted mb-8">Rooms for chat and calls, threaded messaging, HD video conferencing, file sharing, and AI-powered document recognition — all in one platform.</p>
            <v-btn color="primary" size="x-large" :to="{ name: 'register' }" class="mr-4 px-8">Start Free</v-btn>
            <v-btn variant="outlined" size="x-large" href="#pricing" class="px-8">View Plans</v-btn>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Trust bar -->
    <section class="trust-section py-8">
      <v-container>
        <p class="text-center text-body-2 landing-muted mb-4">Trusted by teams who value real-time collaboration</p>
        <v-row justify="center" align="center">
          <v-col v-for="stat in stats" :key="stat.label" cols="6" sm="3" class="text-center">
            <div class="text-h4 font-weight-bold text-primary">{{ stat.value }}</div>
            <div class="text-body-2 landing-muted">{{ stat.label }}</div>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Features -->
    <section id="features" class="features-section py-16">
      <v-container>
        <h2 class="text-h3 text-center font-weight-bold mb-2">Everything your team needs</h2>
        <p class="text-center text-body-1 landing-muted mb-12">From quick messages to deep collaboration</p>
        <v-row>
          <v-col v-for="feature in features" :key="feature.title" cols="12" sm="6" md="4">
            <v-card variant="outlined" class="feature-card pa-6 h-100" rounded="lg">
              <v-icon :color="feature.color" size="48" class="mb-4">{{ feature.icon }}</v-icon>
              <h3 class="text-h6 font-weight-bold mb-2">{{ feature.title }}</h3>
              <p class="text-body-2 landing-muted">{{ feature.description }}</p>
            </v-card>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Pricing -->
    <section id="pricing" class="pricing-section py-16">
      <v-container>
        <h2 class="text-h3 text-center font-weight-bold mb-2">Simple, transparent pricing</h2>
        <p class="text-center text-body-1 landing-muted mb-12">Start free, upgrade when you need more</p>
        <v-row justify="center">
          <v-col v-for="plan in plans" :key="plan.name" cols="12" sm="6" md="4">
            <v-card
              :variant="plan.popular ? 'elevated' : 'outlined'"
              :elevation="plan.popular ? 8 : 0"
              class="pa-6 h-100 d-flex flex-column"
              rounded="lg"
              :class="{ 'border-primary': plan.popular }"
            >
              <v-chip v-if="plan.popular" color="primary" size="small" class="mb-4 align-self-start">Most Popular</v-chip>
              <h3 class="text-h5 font-weight-bold">{{ plan.name }}</h3>
              <div class="my-4">
                <span class="text-h3 font-weight-bold">${{ plan.price }}</span>
                <span v-if="plan.price > 0" class="text-body-2 landing-muted">/user/mo</span>
                <span v-else class="text-body-2 landing-muted">forever</span>
              </div>
              <v-divider class="mb-4" />
              <v-list density="compact" class="flex-grow-1 bg-transparent">
                <v-list-item v-for="f in plan.features" :key="f" :title="f" prepend-icon="mdi-check" />
              </v-list>
              <v-btn
                :color="plan.popular ? 'primary' : undefined"
                :variant="plan.popular ? 'flat' : 'outlined'"
                block
                size="large"
                :to="{ name: 'register' }"
                class="mt-4"
              >
                {{ plan.price === 0 ? 'Get Started Free' : 'Start Free Trial' }}
              </v-btn>
            </v-card>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Final CTA -->
    <section class="cta-section py-16">
      <v-container>
        <v-row justify="center">
          <v-col cols="12" md="8" class="text-center">
            <h2 class="text-h3 font-weight-bold mb-4 text-white">Ready to bring your team together?</h2>
            <p class="text-body-1 cta-subtitle mb-8">Join thousands of teams already collaborating on Roomler.</p>
            <v-btn color="white" size="x-large" :to="{ name: 'register' }" class="px-8 text-primary">Create Your Workspace — Free</v-btn>
          </v-col>
        </v-row>
      </v-container>
    </section>

    <!-- Footer -->
    <v-footer class="landing-footer py-8">
      <v-container>
        <v-row>
          <v-col cols="12" sm="4">
            <div class="text-h6 font-weight-bold mb-2">Roomler</div>
            <p class="text-body-2 landing-muted">Real-time collaboration for modern teams.</p>
          </v-col>
          <v-col cols="6" sm="2">
            <div class="text-subtitle-2 font-weight-bold mb-2">Product</div>
            <div class="text-body-2 landing-muted mb-1">Features</div>
            <div class="text-body-2 landing-muted mb-1">Pricing</div>
            <div class="text-body-2 landing-muted mb-1">Security</div>
          </v-col>
          <v-col cols="6" sm="2">
            <div class="text-subtitle-2 font-weight-bold mb-2">Company</div>
            <div class="text-body-2 landing-muted mb-1">About</div>
            <div class="text-body-2 landing-muted mb-1">Blog</div>
            <div class="text-body-2 landing-muted mb-1">Careers</div>
          </v-col>
          <v-col cols="12" sm="4">
            <div class="text-subtitle-2 font-weight-bold mb-2">Legal</div>
            <div class="text-body-2 landing-muted mb-1">Privacy Policy</div>
            <div class="text-body-2 landing-muted mb-1">Terms of Service</div>
          </v-col>
        </v-row>
        <v-divider class="my-4" />
        <div class="text-body-2 landing-muted text-center">&copy; {{ new Date().getFullYear() }} Roomler. All rights reserved.</div>
      </v-container>
    </v-footer>
  </div>
  </v-theme-provider>
</template>

<script setup>
const stats = [
  { value: '10K+', label: 'Active Teams' },
  { value: '1M+', label: 'Messages Daily' },
  { value: '99.9%', label: 'Uptime' },
  { value: '4.8/5', label: 'User Rating' },
]

const features = [
  {
    icon: 'mdi-pound',
    title: 'Organized Rooms',
    description: 'Create rooms for every team, project, or topic. Chat and call in the same space with flexible room settings.',
    color: '#009688',
  },
  {
    icon: 'mdi-message-text-outline',
    title: 'Threaded Chat',
    description: 'Rich messaging with threads, reactions, mentions, pins, and file attachments — nothing gets lost.',
    color: '#ef5350',
  },
  {
    icon: 'mdi-video-outline',
    title: 'HD Video Conferencing',
    description: 'Built-in WebRTC SFU for crystal-clear video meetings with screen sharing and recording.',
    color: '#009688',
  },
  {
    icon: 'mdi-file-document-outline',
    title: 'File Sharing & AI',
    description: 'Upload files, sync with cloud storage, and let AI recognize and categorize your documents.',
    color: '#ef5350',
  },
  {
    icon: 'mdi-account-group-outline',
    title: 'Multi-Tenant Workspaces',
    description: 'Isolated workspaces with roles, permissions, and invite links for secure collaboration.',
    color: '#009688',
  },
  {
    icon: 'mdi-cloud-outline',
    title: 'Cloud Integrations',
    description: 'Connect Google Drive, OneDrive, and Dropbox. Export conversations to Excel and PDF.',
    color: '#ef5350',
  },
]

const plans = [
  {
    name: 'Free',
    price: 0,
    popular: false,
    features: [
      '10 members',
      '5 rooms',
      '5K message history',
      '100 MB storage',
      'Basic chat & DMs',
    ],
  },
  {
    name: 'Pro',
    price: 8,
    popular: true,
    features: [
      'Unlimited members',
      'Unlimited rooms',
      'Full message history',
      '10 GB storage',
      'Video conferencing (10)',
      'Cloud integrations',
    ],
  },
  {
    name: 'Business',
    price: 16,
    popular: false,
    features: [
      'Everything in Pro',
      '100 GB storage',
      'Video (100 participants)',
      'AI doc recognition',
      'Recordings & transcription',
      'Priority support',
    ],
  },
]
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
  padding-top: 140px;
  padding-bottom: 80px;
  background: linear-gradient(135deg, #e0f2f1 0%, #e8f5e9 50%, #e0f7fa 100%);
  position: relative;
  overflow: hidden;
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
