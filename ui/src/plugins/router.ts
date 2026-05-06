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
              { path: 'agents',    name: 'admin-agents',    props: true, component: () => import('@/components/admin/AgentsSection.vue') },
              { path: 'tasks',     name: 'admin-tasks',     props: true, component: () => import('@/components/admin/TasksSection.vue') },
              { path: 'audit-log', name: 'admin-audit-log', props: true, component: () => import('@/components/admin/AuditSection.vue') },
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
    next({ name: 'landing' })
  } else if (to.meta.guest && token) {
    // After login/register, check for pending invite
    const pendingInvite = sessionStorage.getItem('pending_invite_code')
    if (pendingInvite) {
      sessionStorage.removeItem('pending_invite_code')
      next({ name: 'invite', params: { code: pendingInvite } })
    } else {
      next({ name: 'dashboard' })
    }
  } else {
    next()
  }
})

export default router
