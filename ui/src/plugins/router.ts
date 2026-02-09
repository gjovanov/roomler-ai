import { createRouter, createWebHistory } from 'vue-router'
import type { RouteRecordRaw } from 'vue-router'

const routes: RouteRecordRaw[] = [
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
    path: '/oauth/callback',
    name: 'oauth-callback',
    component: () => import('@/views/auth/OAuthCallbackView.vue'),
    meta: { guest: true },
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
        path: 'tenant/:tenantId',
        children: [
          {
            path: '',
            name: 'tenant-dashboard',
            component: () => import('@/views/dashboard/TenantDashboard.vue'),
          },
          {
            path: 'channel/:channelId',
            name: 'channel-chat',
            component: () => import('@/views/chat/ChatView.vue'),
          },
          {
            path: 'channels',
            name: 'channels',
            component: () => import('@/views/channels/ChannelList.vue'),
          },
          {
            path: 'explore',
            name: 'explore',
            component: () => import('@/views/channels/ExploreView.vue'),
          },
          {
            path: 'conferences',
            name: 'conferences',
            component: () => import('@/views/conference/ConferenceList.vue'),
          },
          {
            path: 'conference/:conferenceId',
            name: 'conference',
            component: () => import('@/views/conference/ConferenceView.vue'),
          },
          {
            path: 'files',
            name: 'files',
            component: () => import('@/views/files/FilesBrowser.vue'),
          },
          {
            path: 'admin',
            name: 'admin',
            component: () => import('@/views/admin/AdminPanel.vue'),
          },
        ],
      },
    ],
  },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

router.beforeEach((to, _from, next) => {
  const token = localStorage.getItem('access_token')
  if (to.meta.auth && !token) {
    next({ name: 'login' })
  } else if (to.meta.guest && token) {
    next({ name: 'dashboard' })
  } else {
    next()
  }
})

export default router
