// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-87 (#1776) — the landing copy and the plan table, ONE source for the two
 * pages that show them: the SPA's `LandingView.vue` (in-app navigation) and
 * the static homepage the docs generator writes for `/` (what a guest and
 * every crawler get).
 *
 * Zero imports, like `enrollCommands.ts`, so Bun can load it at build time
 * with no Vue and no Vuetify. Icons stay with each renderer: the SPA draws
 * Material Design icons, the static page draws its own inline SVGs, so an
 * entry names its SPA icon and the generator maps it.
 */

export interface LandingFeature {
  /** Material Design icon for the SPA (`mdi-…`). */
  icon: string
  title: string
  description: string
  color: string
}

export interface LandingPillar {
  title: string
  subtitle: string
  features: LandingFeature[]
}

export interface Plan {
  id: string
  name: string
  price_cents: number
  features: string[]
}

export const HERO = {
  titleLead: 'Every device you own,',
  titleAccent: 'one secure network',
  lead: 'Remote desktop from any browser and a private WireGuard-style mesh between your machines — with team chat and video included.',
}

export const CAPABILITIES = [
  'Remote desktop',
  'Private mesh network',
  'Tunnels & SOCKS5',
  'Exit nodes',
  'MagicDNS',
  'Chat & video included',
]

// Pivot order: remote access first, the private network second,
// collaboration as the included bonus.
export const PILLARS: LandingPillar[] = [
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
        description: 'File sharing with versioned uploads, per-room libraries and search.',
        color: '#009688',
      },
    ],
  },
]

export const DOWNLOAD = {
  title: 'Set up a device in minutes',
  lead: 'Run the graphical installer or paste one command. Enrollment tokens come from your workspace (Devices → Enroll device).',
  /** The wizard download each OS's button points at (the API streams it). */
  wizard: { windows: '/api/setup/windows', linux: '/api/setup/linux', macos: '/api/setup/macos' } as Record<string, string>,
}

export const PRICING = {
  title: 'Simple, per-user pricing',
  lead: 'Every plan includes the private network and remote desktop. Start free, upgrade for more devices.',
}

/**
 * Shown before `/api/stripe/plans` answers, or when it cannot: that endpoint
 * is the single source of truth, and both pages replace this with its answer.
 * Keep it in step with the server matrix, or the first paint states a price
 * the checkout will not honour.
 */
export const FALLBACK_PLANS: Plan[] = [
  { id: 'free', name: 'Free', price_cents: 0, features: ['3 devices', 'Private network (overlay mesh)', 'Chat: 10 members'] },
  { id: 'pro', name: 'Pro', price_cents: 800, features: ['30 devices', 'Exit nodes + MagicDNS', 'Unlimited members'] },
  { id: 'business', name: 'Business', price_cents: 1600, features: ['300 devices', 'Everything in Pro', 'Priority support'] },
]

export const CTA = {
  title: 'Take your devices with you',
  lead: 'Free for up to 3 devices — remote desktop, private mesh, tunnels, chat and calls.',
  button: 'Create Your Workspace — Free',
}
