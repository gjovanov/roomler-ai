<template>
  <v-theme-provider theme="light">
    <div class="legal-page">
      <v-container class="legal-container py-12">
        <router-link to="/landing" class="text-body-2 text-primary text-decoration-none d-inline-flex align-center mb-6">
          <v-icon size="small" class="mr-1">mdi-arrow-left</v-icon>
          Back to home
        </router-link>

        <h1 class="text-h3 font-weight-bold mb-2">Terms of Service</h1>
        <p class="text-body-2 legal-muted mb-8">Last updated: February 2026</p>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">1. Introduction and Acceptance</h2>
          <p class="text-body-1 legal-muted">
            These Terms of Service ("Terms") govern your access to and use of Roomler AI ("Roomler," "we," "us,"
            or "our"), a real-time collaboration platform available at
            <a href="https://roomler.ai" class="text-primary">roomler.ai</a>. Roomler provides chat, video
            conferencing, AI-powered transcription, file sharing, and multi-tenant workspace management. By
            creating an account or using our services, you agree to be bound by these Terms. If you do not
            agree, please do not use the platform.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">2. Eligibility</h2>
          <p class="text-body-1 legal-muted">
            You must be at least 16 years of age to use Roomler. By using the platform, you represent and warrant
            that you meet this age requirement and have the legal capacity to enter into these Terms. If you are
            using Roomler on behalf of an organization, you represent that you have the authority to bind that
            organization to these Terms.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">3. Account Responsibilities</h2>
          <p class="text-body-1 legal-muted mb-2">
            When you create an account on Roomler, you agree to:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1">Provide accurate and complete registration information</li>
            <li class="mb-1">Maintain the security and confidentiality of your login credentials</li>
            <li class="mb-1">Promptly notify us of any unauthorized use of your account</li>
            <li class="mb-1">Accept responsibility for all activities that occur under your account</li>
          </ul>
          <p class="text-body-1 legal-muted">
            You may authenticate using email/password or through third-party OAuth providers (Google, Facebook,
            GitHub, LinkedIn). You are responsible for maintaining the security of your chosen authentication method.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">4. Acceptable Use</h2>
          <p class="text-body-1 legal-muted mb-2">
            You agree not to use Roomler to:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1">Harass, bully, threaten, or intimidate other users in chat rooms, direct messages, or video calls</li>
            <li class="mb-1">Share, upload, or transmit illegal, obscene, defamatory, or harmful content</li>
            <li class="mb-1">Distribute malware, spam, or phishing content through messages or file uploads</li>
            <li class="mb-1">Impersonate any person or entity, or misrepresent your identity</li>
            <li class="mb-1">Attempt to gain unauthorized access to other users' accounts, workspaces, or data</li>
            <li class="mb-1">Interfere with or disrupt the platform, servers, or networks connected to Roomler</li>
            <li class="mb-1">Use the platform to violate any applicable law or regulation</li>
            <li class="mb-1">Record, screenshot, or redistribute calls or content without the consent of all participants</li>
            <li class="mb-1">Use automated bots or scripts to access the platform without our prior written consent</li>
          </ul>
          <p class="text-body-1 legal-muted">
            We reserve the right to investigate and take appropriate action against any violations, including
            suspending or terminating your account.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">5. Content Ownership and User-Generated Content</h2>
          <p class="text-body-1 legal-muted mb-4">
            You retain ownership of all content you create, upload, or share on Roomler, including chat messages,
            files, documents, and any other materials ("User Content"). By uploading or sharing User Content, you
            grant Roomler a limited, non-exclusive, worldwide license to store, process, transmit, and display
            your content solely for the purpose of providing the platform services to you and your workspace members.
          </p>
          <p class="text-body-1 legal-muted">
            You are solely responsible for your User Content and represent that you have all necessary rights
            to share it. We do not claim ownership over your User Content, and this license terminates when you
            delete your content or your account.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">6. Intellectual Property</h2>
          <p class="text-body-1 legal-muted">
            The Roomler platform, including its software, design, logos, trademarks, and documentation, is owned
            by Roomler and is protected by intellectual property laws. These Terms do not grant you any right to
            use our trademarks, logos, or brand assets without our prior written permission. All rights not
            expressly granted to you are reserved.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">7. Recording and Transcription Consent</h2>
          <p class="text-body-1 legal-muted mb-4">
            Roomler offers call recording and AI-powered transcription features for video and audio conferences.
            By using these features, you acknowledge and agree to the following:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1">Recording can only be initiated by authorized participants with appropriate workspace permissions</li>
            <li class="mb-1">All participants in a call will be notified when recording begins</li>
            <li class="mb-1">By remaining in a call after recording notification, participants consent to being recorded</li>
            <li class="mb-1">AI-generated transcriptions are provided as-is and may contain inaccuracies</li>
            <li class="mb-1">Recordings and transcriptions are stored according to workspace settings and your subscription plan</li>
            <li class="mb-1">You are responsible for complying with all applicable laws regarding recording consent in your jurisdiction</li>
          </ul>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">8. Billing and Subscriptions</h2>
          <p class="text-body-1 legal-muted mb-2">
            Roomler offers free and paid subscription plans. For paid plans:
          </p>
          <ul class="text-body-1 legal-muted ml-6 mb-2">
            <li class="mb-1">All payments are processed securely by Stripe. We do not store your full payment card details.</li>
            <li class="mb-1">Subscriptions are billed on a recurring basis (monthly or annually) according to your chosen plan.</li>
            <li class="mb-1">You may upgrade, downgrade, or cancel your subscription at any time through your workspace billing settings.</li>
            <li class="mb-1">Cancellation takes effect at the end of the current billing period. No prorated refunds are provided for partial periods.</li>
            <li class="mb-1">We reserve the right to modify pricing with at least 30 days' advance notice. Price changes will apply at your next renewal.</li>
            <li class="mb-1">If payment fails, we may suspend access to paid features after a reasonable grace period.</li>
          </ul>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">9. Termination</h2>
          <p class="text-body-1 legal-muted mb-4">
            You may terminate your account at any time by contacting us or through your account settings.
            We may suspend or terminate your account if you violate these Terms, engage in prohibited conduct,
            or if required by law.
          </p>
          <p class="text-body-1 legal-muted">
            Upon termination, your right to use the platform ceases immediately. We may retain certain data
            as required by law or for legitimate business purposes. Content shared in workspaces may persist
            if other members of the workspace retain access to it.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">10. Disclaimers</h2>
          <p class="text-body-1 legal-muted">
            Roomler is provided on an "AS IS" and "AS AVAILABLE" basis without warranties of any kind, whether
            express or implied, including but not limited to warranties of merchantability, fitness for a
            particular purpose, and non-infringement. We do not warrant that the platform will be uninterrupted,
            error-free, or secure. AI-generated transcriptions are provided for convenience and may contain
            errors. WebRTC call quality may vary depending on network conditions beyond our control.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">11. Limitation of Liability</h2>
          <p class="text-body-1 legal-muted">
            To the maximum extent permitted by applicable law, Roomler and its affiliates, officers, directors,
            employees, and agents shall not be liable for any indirect, incidental, special, consequential, or
            punitive damages, including but not limited to loss of data, loss of profits, or business
            interruption, arising out of or in connection with your use of the platform. Our total aggregate
            liability for any claims arising under these Terms shall not exceed the amount you paid to us in
            the twelve (12) months preceding the claim.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">12. Indemnification</h2>
          <p class="text-body-1 legal-muted">
            You agree to indemnify, defend, and hold harmless Roomler and its affiliates from and against any
            claims, liabilities, damages, losses, and expenses (including reasonable legal fees) arising from
            your use of the platform, your User Content, your violation of these Terms, or your violation of
            any rights of a third party.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">13. Governing Law and Disputes</h2>
          <p class="text-body-1 legal-muted">
            These Terms shall be governed by and construed in accordance with the laws of the jurisdiction in
            which Roomler operates, without regard to its conflict of law principles. Any disputes arising out
            of or relating to these Terms shall first be attempted to be resolved through good-faith negotiation.
            If negotiation fails, disputes shall be resolved through binding arbitration or in the courts of
            competent jurisdiction.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">14. Changes to These Terms</h2>
          <p class="text-body-1 legal-muted">
            We may update these Terms from time to time. We will notify you of material changes by posting
            the updated Terms on this page and updating the "Last updated" date. For significant changes, we
            may also notify you via email or an in-app notification. Your continued use of Roomler after the
            effective date of any changes constitutes acceptance of the updated Terms.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">15. Severability</h2>
          <p class="text-body-1 legal-muted">
            If any provision of these Terms is found to be unenforceable or invalid, that provision shall be
            limited or eliminated to the minimum extent necessary, and the remaining provisions shall continue
            in full force and effect.
          </p>
        </section>

        <section class="mb-8">
          <h2 class="text-h5 font-weight-bold mb-3">16. Contact Us</h2>
          <p class="text-body-1 legal-muted">
            If you have any questions or concerns about these Terms of Service, please contact us at:
          </p>
          <p class="text-body-1 legal-muted mt-2">
            <strong>Email:</strong> <a href="mailto:legal@roomler.ai" class="text-primary">legal@roomler.ai</a>
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
