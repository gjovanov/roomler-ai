import { createRouter, createWebHistory } from 'vue-router'
import type { RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
  {
    path: '/landing',
    name: 'landing',
    component: () => import('@/views/LandingView.vue'),
    meta: { guest: true },
  },
  {
    path: '/pricing',
    name: 'pricing',
    component: () => import('@/views/LandingView.vue'),
    meta: { guest: true },
  },
  {
    path: '/login',
    name: 'login',
    component: () => import('@/views/auth/LoginView.vue'),
    meta: { guest: true },
  },
  {
    path: '/register',
    name: 'register',
    component: () => import('@/views/auth/RegisterView.vue'),
    meta: { guest: true },
  },
  {
    path: '/privacy',
    name: 'privacy',
    component: () => import('@/views/legal/PrivacyPolicyView.vue'),
  },
  {
    path: '/terms',
    name: 'terms',
    component: () => import('@/views/legal/TermsView.vue'),
  },
  {
    path: '/oauth/callback',
    name: 'oauth-callback',
    component: () => import('@/views/auth/OAuthCallbackView.vue'),
    meta: { guest: true },
  },
  {
    path: '/invite/:code',
    name: 'invite',
    component: () => import('@/views/invite/InviteLandingView.vue'),
  },
  {
    // Public owner-consent landing (Phase 4). No `meta.auth` — the token in the
    // path is the capability, so a logged-out owner can approve from the email
    // link / push tap.
    path: '/consent/:token',
    name: 'consent',
    component: () => import('@/views/remote/ConsentView.vue'),
  },
  {
    path: '/',
    component: () => import('@/components/layout/AppLayout.vue'),
    meta: { auth: true },
    children: [
      {
        path: '',
        name: 'dashboard',
        component: () => import('@/views/dashboard/DashboardView.vue'),
      },
      {
        path: 'profile/edit',
        name: 'profile-edit',
        component: () => import('@/views/profile/ProfileEditView.vue'),
      },
      {
        // Stats PR-4 — platform-operator observability (relay fleet,
        // orgs, calls). Client-gated by user.is_platform_admin; the
        // server 404s everyone else.
        path: 'observability',
        name: 'observability',
        component: () => import('@/views/observability/ObservabilityView.vue'),
      },
      {
        path: 'profile/:userId',
        name: 'profile',
        component: () => import('@/views/profile/ProfileView.vue'),
      },
      {
        path: 'tenant/:tenantId',
        children: [
          {
            path: '',
            name: 'tenant-dashboard',
            component: () => import('@/views/dashboard/TenantDashboard.vue'),
          },
          {
            path: 'room/:roomId',
            name: 'room-chat',
            component: () => import('@/views/chat/ChatView.vue'),
          },
          {
            path: 'room/:roomId/call',
            name: 'room-call',
            component: () => import('@/views/conference/ConferenceView.vue'),
          },
          {
            path: 'rooms',
            name: 'rooms',
            component: () => import('@/views/rooms/RoomList.vue'),
          },
          {
            path: 'explore',
            name: 'explore',
            component: () => import('@/views/rooms/ExploreView.vue'),
          },
          {
            path: 'files',
            name: 'files',
            component: () => import('@/views/files/FilesBrowser.vue'),
          },
          {
            path: 'invites',
            name: 'invites',
            component: () => import('@/views/invite/InviteManageView.vue'),
          },
          {
            // Stats PR-4 — org analytics (machines/calls/tunnels over
            // time). The component fail-closes on membership permissions
            // before firing any query.
            path: 'analytics',
            name: 'analytics',
            props: true,
            component: () => import('@/views/analytics/AnalyticsView.vue'),
          },
          {
            // S4 pivot — the fleet page, promoted out of Admin. The old
            // `admin-agents` path redirects here (below).
            path: 'devices',
            name: 'devices',
            component: () => import('@/views/devices/DevicesView.vue'),
          },
          {
            // S4 pivot — the overlay/tunnel network group, promoted out
            // of Admin (Tailscale-style IA). Sections stay child routes
            // (bookmarkable, back/forward-friendly), same pattern as
            // Admin.
            path: 'network',
            // 2026-08-04 — machines + tunnel-clients folded into the unified
            // Devices page (overlay address/last-seen live on the device
            // rows now); the parent lands on ACL, the first remaining child.
            redirect: { name: 'network-acl' },
            component: () => import('@/views/network/NetworkPanel.vue'),
            children: [
              { path: 'machines',       redirect: { name: 'devices' } },
              { path: 'tunnel-clients', redirect: { name: 'devices' } },
              // 2026-08-04 — ONE ACL page with Overlay + Tunnel tabs
              // (separate backends, single place); the standalone
              // overlay-acl path lands on its tab.
              { path: 'acl',            name: 'network-acl',            props: true, component: () => import('@/components/admin/AclSection.vue') },
              { path: 'subnet-routes',  name: 'network-subnet-routes',  props: true, component: () => import('@/components/admin/OverlaySubnetRoutesSection.vue') },
              { path: 'overlay-acl',    redirect: (to) => ({ name: 'network-acl', params: to.params, query: { tab: 'overlay' } }) },
              { path: 'dns',            name: 'network-dns',            props: true, component: () => import('@/components/admin/MagicDnsSection.vue') },
            ],
          },
          {
            path: 'admin',
            // Parent-level redirect: hitting `/tenant/{id}/admin` goes
            // straight to the Settings child without leaving an
            // intermediate history entry. Avoids the back-button loop
            // that an empty-path child redirect would create.
            redirect: { name: 'admin-settings' },
            component: () => import('@/views/admin/AdminPanel.vue'),
            // Each section is a child route — URL reflects the active
            // tab, browser back/forward works, deep links bookmarkable.
            // `props: true` auto-passes route params (tenantId) as
            // component props so each section receives `tenantId`
            // consistently with the existing AgentsSection contract.
            children: [
              { path: 'settings',  name: 'admin-settings',  props: true, component: () => import('@/components/admin/SettingsSection.vue') },
              { path: 'members',   name: 'admin-members',   props: true, component: () => import('@/components/admin/MembersSection.vue') },
              { path: 'roles',     name: 'admin-roles',     props: true, component: () => import('@/components/admin/RolesSection.vue') },
              // S4 — the fleet/network sections moved out of Admin; the
              // old paths 301 to their new homes (named redirects keep
              // the :tenantId param). Old bookmarks + docs keep working.
              { path: 'agents',          redirect: { name: 'devices' } },
              { path: 'tunnel-clients',  redirect: { name: 'devices' } },
              { path: 'tunnel-policies', redirect: { name: 'network-acl' } },
              { path: 'subnet-routes',   redirect: { name: 'network-subnet-routes' } },
              { path: 'magic-dns',       redirect: { name: 'network-dns' } },
            ],
          },
          {
            path: 'billing',
            name: 'billing',
            component: () => import('@/views/billing/BillingView.vue'),
          },
          {
            path: 'agent/:agentId/remote',
            name: 'agent-remote',
            component: () => import('@/views/remote/RemoteControl.vue'),
          },
        ],
      },
    ],
  },
  // 404 catch-all
  {
    path: '/:pathMatch(.*)*',
    name: 'not-found',
    component: () => import('@/views/NotFoundView.vue'),
  },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

router.beforeEach((to, _from, next) => {
  const token = localStorage.getItem('access_token')
  if (to.meta.auth && !token) {
    if (to.fullPath === '/' || to.fullPath === '') {
      // Bare root: first-visit marketing page, nothing to return to.
      next({ name: 'landing' })
    } else {
      // S2: remember the protected deep-link (e.g. the desktop app's
      // "View screen" → /tenant/{tid}/agent/{aid}/remote) and go
      // straight to login — the landing page would strand the link.
      // Consumed by the guest guard below and the login/OAuth handlers.
      sessionStorage.setItem('pending_redirect', to.fullPath)
      next({ name: 'login' })
    }
  } else if (to.meta.guest && token) {
    // After login/register, check for pending invite
    const pendingInvite = sessionStorage.getItem('pending_invite_code')
    const pendingRedirect = sessionStorage.getItem('pending_redirect')
    if (pendingInvite) {
      sessionStorage.removeItem('pending_invite_code')
      next({ name: 'invite', params: { code: pendingInvite } })
    } else if (pendingRedirect && pendingRedirect.startsWith('/')) {
      sessionStorage.removeItem('pending_redirect')
      next(pendingRedirect)
    } else {
      next({ name: 'dashboard' })
    }
  } else {
    next()
  }
})

export default router
