<template>
  <v-theme-provider theme="light">
    <div class="legal-page">
      <v-container class="legal-container py-12">
        <router-link to="/landing" class="text-body-2 text-primary text-decoration-none d-inline-flex align-center mb-6">
          <v-icon size="small" class="mr-1">mdi-arrow-left</v-icon>
          Back to home
        </router-link>

        <h1 class="text-h3 font-weight-bold mb-2">Privacy Policy</h1>
        <p class="text-body-2 legal-muted mb-8">Last updated: February 2026</p>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">1. Introduction</h2>
          <p class="text-body-1 legal-muted">
            Welcome to Roomler AI ("Roomler," "we," "us," or "our"). Roomler is a real-time collaboration platform
            that provides chat, video conferencing powered by WebRTC and mediasoup, AI-powered transcription,
            file sharing, and multi-tenant workspace management. This Privacy Policy explains how we collect, use,
            disclose, and safeguard your information when you use our platform at
            <a href="https://roomler.ai" class="text-primary">roomler.ai</a>.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">2. Information We Collect</h2>

          <h3 class="text-h6 font-weight-bold mb-2">2.1 Account Information</h3>
          <p class="text-body-1 legal-muted mb-4">
            When you create an account, we collect your email address, display name, username, and optionally a
            profile avatar. This information is necessary to provide you with access to the platform and to
            identify you within your workspaces.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.2 Tenant and Workspace Data</h3>
          <p class="text-body-1 legal-muted mb-4">
            We collect data related to the tenants (workspaces) you create or join, including workspace names,
            settings, room configurations, membership roles, and invite links. This data is used to manage your
            collaborative environment.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.3 Chat Messages and Content</h3>
          <p class="text-body-1 legal-muted mb-4">
            Messages you send in rooms, direct messages, threads, reactions, mentions, and pinned items are stored
            to provide the chat functionality. Message history is retained according to your workspace plan limits.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.4 Files and Uploads</h3>
          <p class="text-body-1 legal-muted mb-4">
            Files you upload to Roomler are stored in our S3-compatible object storage (MinIO). This includes
            documents, images, and any other files shared within rooms or direct messages. Files are associated
            with your account and the workspace where they were shared.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.5 Call Recordings and Transcriptions</h3>
          <p class="text-body-1 legal-muted mb-4">
            If recording is enabled for a video or audio call, the recording and any AI-generated transcriptions
            are stored on our servers. Recordings are only created when explicitly initiated by an authorized
            participant, and all participants are notified when recording begins.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.6 OAuth and Third-Party Sign-In Data</h3>
          <p class="text-body-1 legal-muted mb-4">
            When you sign in using Google, Facebook, GitHub, or LinkedIn, we receive your email address, display
            name, and profile picture from the respective provider. We do not receive or store your passwords
            from these services. We use this information solely to create and authenticate your Roomler account.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.7 WebRTC and Conferencing Data</h3>
          <p class="text-body-1 legal-muted mb-4">
            During video and audio calls, your media streams (video and audio) are routed through our mediasoup
            Selective Forwarding Unit (SFU) server. Media streams are processed in real time for delivery to
            other participants and are <strong>not stored</strong> unless recording has been explicitly enabled
            for the call. Connection metadata (IP addresses, codec information, bandwidth statistics) may be
            temporarily logged for quality monitoring and troubleshooting purposes.
          </p>

          <h3 class="text-h6 font-weight-bold mb-2">2.8 Billing Information</h3>
          <p class="text-body-1 legal-muted mb-4">
            Payment processing is handled by Stripe. We do not store your credit card numbers, CVV, or full
            payment card details on our servers. We may store your Stripe customer ID, subscription plan details,
            billing email, and transaction history for account management and invoicing purposes.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">3. Cookies and Local Storage</h2>
          <p class="text-body-1 legal-muted">
            Roomler uses a JWT (JSON Web Token) for authentication, which is stored in your browser's
            localStorage. We also maintain WebSocket connections for real-time messaging and presence
            functionality. We do not use third-party tracking cookies. Essential technical data may be
            stored in your browser to maintain your session and preferences.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">4. How We Use Your Information</h2>
          <p class="text-body-1 legal-muted mb-2">We use the information we collect to:</p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1">Provide, maintain, and improve the Roomler platform</li>
            <li class="mb-1">Authenticate your identity and manage your account</li>
            <li class="mb-1">Deliver chat messages, files, and video/audio streams to the intended recipients</li>
            <li class="mb-1">Generate AI-powered transcriptions of calls when recording is enabled</li>
            <li class="mb-1">Process payments and manage subscription billing</li>
            <li class="mb-1">Send service-related communications (e.g., account verification, security alerts)</li>
            <li class="mb-1">Monitor and improve platform performance and reliability</li>
            <li class="mb-1">Comply with legal obligations</li>
          </ul>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">5. Data Sharing and Disclosure</h2>
          <p class="text-body-1 legal-muted mb-2">
            We do not sell your personal data. We may share your information only in the following circumstances:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1"><strong>With workspace members:</strong> Your profile information, messages, and shared files are visible to members of the same workspace according to room permissions.</li>
            <li class="mb-1"><strong>Service providers:</strong> We use Stripe for payment processing and may use other service providers for infrastructure and operations, all bound by data processing agreements.</li>
            <li class="mb-1"><strong>Legal requirements:</strong> We may disclose information if required by law, regulation, legal process, or governmental request.</li>
            <li class="mb-1"><strong>Business transfers:</strong> In the event of a merger, acquisition, or sale of assets, your data may be transferred as part of that transaction.</li>
          </ul>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">6. Data Retention</h2>
          <p class="text-body-1 legal-muted">
            We retain your account data for as long as your account is active. Chat message history is retained
            according to your workspace plan limits. Files are retained until deleted by you or a workspace
            administrator. Call recordings and transcriptions are retained according to your workspace settings.
            When you delete your account, we will remove your personal data within 30 days, except where retention
            is required by law or for legitimate business purposes (such as billing records).
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">7. Data Security</h2>
          <p class="text-body-1 legal-muted">
            We implement industry-standard security measures to protect your data, including encrypted connections
            (TLS/HTTPS), secure WebSocket connections (WSS), encrypted media streams (DTLS-SRTP for WebRTC),
            and access controls. While we strive to protect your information, no method of electronic transmission
            or storage is 100% secure, and we cannot guarantee absolute security.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">8. Your Rights (GDPR and Global Privacy)</h2>
          <p class="text-body-1 legal-muted mb-2">
            Depending on your jurisdiction, you may have the following rights regarding your personal data:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1"><strong>Access:</strong> Request a copy of the personal data we hold about you.</li>
            <li class="mb-1"><strong>Rectification:</strong> Request correction of inaccurate or incomplete data.</li>
            <li class="mb-1"><strong>Erasure:</strong> Request deletion of your personal data ("right to be forgotten").</li>
            <li class="mb-1"><strong>Portability:</strong> Request a machine-readable copy of your data.</li>
            <li class="mb-1"><strong>Restriction:</strong> Request that we limit how we process your data.</li>
            <li class="mb-1"><strong>Objection:</strong> Object to processing of your data for certain purposes.</li>
            <li class="mb-1"><strong>Withdraw consent:</strong> Where processing is based on consent, you may withdraw it at any time.</li>
          </ul>
          <p class="text-body-1 legal-muted">
            To exercise any of these rights, please contact us at
            <a href="mailto:privacy@roomler.ai" class="text-primary">privacy@roomler.ai</a>.
            We will respond to your request within 30 days.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">9. Children's Privacy</h2>
          <p class="text-body-1 legal-muted">
            Roomler is not intended for use by individuals under the age of 16. We do not knowingly collect
            personal data from children. If we become aware that we have collected data from a child under 16,
            we will take steps to delete that information promptly.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">10. Changes to This Policy</h2>
          <p class="text-body-1 legal-muted">
            We may update this Privacy Policy from time to time. We will notify you of material changes by
            posting the updated policy on this page and updating the "Last updated" date. Your continued use
            of Roomler after any changes constitutes acceptance of the updated policy.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">11. Contact Us</h2>
          <p class="text-body-1 legal-muted">
            If you have any questions or concerns about this Privacy Policy or our data practices, please
            contact us at:
          </p>
          <p class="text-body-1 legal-muted mt-2">
            <strong>Email:</strong> <a href="mailto:privacy@roomler.ai" class="text-primary">privacy@roomler.ai</a>
          </p>
        </section>
      </v-container>
    </div>
  </v-theme-provider>
</template>

<script setup>
</script>

<style scoped>
.legal-page {
  background: #ffffff;
  color: #1a1a2e;
  min-height: 100vh;
}

.legal-container {
  max-width: 800px;
}

.legal-muted {
  color: rgba(26, 26, 46, 0.75) !important;
  line-height: 1.7;
}
</style>
