import heroMesh from '@/assets/tutorial/hero-mesh.svg'
import heroRemoteDesktop from '@/assets/tutorial/remote-desktop.svg'
import heroPrivateNetwork from '@/assets/tutorial/private-network.svg'
import heroCollaboration from '@/assets/tutorial/collaboration.svg'

/**
 * FR-12 (#788) — the tour's content, kept OUT of the view so it stays
 * readable, diffable and unit-testable (a chapter whose deep link names a
 * route that doesn't exist is a broken promise, and a test can catch it).
 *
 * Prose follows README.md's voice, and the four heroes are the README's own
 * illustrations — one visual language across the site, the repo and the app.
 */

export interface TutorialStep {
  /** What the reader should do. */
  text: string
  /** A REAL in-app destination — never a screenshot of a button. */
  to?: { name: string; query?: Record<string, string> }
  /** Label for the link/button (defaults to "Open"). */
  linkLabel?: string
  /** A copyable command (tunnels / ssh chapters). */
  code?: string
  /** External docs. */
  href?: string
}

export interface TutorialChapter {
  /** URL hash + progress key. Stable — progress is stored against it. */
  id: string
  title: string
  icon: string
  /** One-line rail subtitle. */
  blurb: string
  hero?: string
  heroAlt?: string
  lead: string
  steps: TutorialStep[]
  detail: Array<{ label: string; text: string }>
}

export const TUTORIAL_CHAPTERS: TutorialChapter[] = [
  {
    id: 'get-started',
    title: 'Get started',
    icon: 'mdi-flag-outline',
    blurb: 'What Roomler is, in one screen',
    hero: heroMesh,
    heroAlt:
      'Machines joined in a direct encrypted mesh with stable private addresses, crossing NATs and firewalls',
    lead:
      'Roomler is three products on one small daemon: remote control of your machines, a private network that joins them wherever they are, and rooms for chat, files and calls. You install one agent per machine; everything else happens in this browser. Traffic between your machines is end-to-end encrypted and goes peer-to-peer whenever a path exists — the server coordinates, it never carries your pixels, keystrokes or files.',
    steps: [
      {
        text: 'Give this organization a name your team will recognize — it labels every device and invite.',
        to: { name: 'admin-settings' },
        linkLabel: 'Organization settings',
      },
      {
        text: 'Add your first machine: mint an enrollment token and run the one-line installer on it.',
        to: { name: 'devices', query: { enroll: '1' } },
        linkLabel: 'Enroll a device',
      },
      {
        text: 'Bring in the people you work with — they can join by invite link or email.',
        to: { name: 'invites' },
        linkLabel: 'Invites',
      },
    ],
    detail: [
      {
        label: 'One daemon per machine',
        text: 'The same agent is the remote-desktop target, the network node, the tunnel exit and the SSH server — not four services to install.',
      },
      {
        label: 'Nothing to install to watch',
        text: 'The viewing side is a plain browser tab. Only the machine you reach runs the agent.',
      },
      {
        label: 'Works from anywhere',
        text: 'Hotel Wi-Fi, NAT, corporate firewall: the connection cascade finds the fastest path that works and keeps re-trying for a better one.',
      },
    ],
  },
  {
    id: 'devices',
    title: 'Devices',
    icon: 'mdi-monitor-multiple',
    blurb: 'Enroll machines and keep them current',
    hero: heroMesh,
    heroAlt: 'A fleet of machines enrolled into one organization',
    lead:
      'Devices is the home page of your fleet. Every enrolled machine shows up here with its live status, operating system, overlay address, MagicDNS name and agent version. Two kinds live in the same grid: full daemons (remote desktop + network + tunnels) and tunnel-only clients. Enrolling is one command on the target machine — there is no manual key exchange.',
    steps: [
      {
        text: 'Mint an enrollment token, then paste the one-liner on the machine (Linux/macOS shell, or PowerShell on Windows).',
        to: { name: 'devices', query: { enroll: '1' } },
        linkLabel: 'Enroll a device',
      },
      {
        text: 'Give a device a friendly display name and tags — the name follows it into the network and MagicDNS.',
        to: { name: 'devices' },
        linkLabel: 'Open Devices',
      },
      {
        text: 'Tailor the grid: search across every column, sort, and pick which columns you see (your choice is remembered per device page).',
        to: { name: 'devices' },
        linkLabel: 'Open Devices',
      },
    ],
    detail: [
      {
        label: 'Install',
        text: 'One line per platform, or the roomler-setup wizard for a GUI. Windows MSI (per-user or per-machine, signed), Linux .deb/tarball with systemd, macOS .pkg.',
      },
      {
        label: 'Status',
        text: 'Online / stale / offline. "Stale" means the device heartbeats but no server holds its live socket — it self-heals within about two minutes.',
      },
      {
        label: 'Updates',
        text: 'Push a specific release to a device, or let it pick up the latest on its own. The server refuses a pin that would downgrade a device unless you force it.',
      },
    ],
  },
  {
    id: 'remote-desktop',
    title: 'Remote desktop',
    icon: 'mdi-remote-desktop',
    blurb: 'Use any machine from a browser tab',
    hero: heroRemoteDesktop,
    heroAlt:
      'A laptop showing the live desktop of an office PC inside a browser tab, mouse and keyboard flowing back',
    lead:
      'Open a machine and use it as if you were sitting in front of it. The picture is hardware-encoded where the GPU allows and fluid enough for real work; clipboard, file transfer and multi-monitor all work, and on Windows the session survives the lock screen, UAC prompts and logout. Every session is consent-gated and audit-logged, and the media never touches the server.',
    steps: [
      {
        text: 'Click Connect on any online device to open its screen in this tab.',
        to: { name: 'devices' },
        linkLabel: 'Open Devices',
      },
      {
        text: 'Decide how each device answers: ask the person at the machine every time, or grant unattended access.',
        to: { name: 'devices' },
        linkLabel: 'Consent per device',
      },
      {
        text: 'In a session, try the toolbar: multi-monitor, scaling, clipboard sync, file upload and Ctrl-Alt-Del.',
      },
    ],
    detail: [
      {
        label: 'Codecs',
        text: 'H.264, HEVC, AV1 and VP9 4:4:4, hardware-encoded via NVENC / Quick Sync / AMF / Media Foundation with automatic fallback to software.',
      },
      {
        label: 'Latency',
        text: 'A low-latency WebCodecs canvas path (Chrome-first) bypasses the browser video element’s jitter buffer for drag-and-type work.',
      },
      {
        label: 'Unattended',
        text: 'Runs as a service, survives logout, and can drive the Windows lock screen and pre-logon desktop. Headless Linux gets a virtual desktop.',
      },
      {
        label: 'Security',
        text: 'Consent-gated, audit-logged, end-to-end encrypted. The server relays signalling only.',
      },
    ],
  },
  {
    id: 'network',
    title: 'Private network',
    icon: 'mdi-lan-connect',
    blurb: 'Stable addresses and names for every machine',
    hero: heroPrivateNetwork,
    heroAlt:
      'Machines on different networks joined by a private overlay with stable addresses',
    lead:
      'Every enrolled machine gets a stable private address and a name that resolves from any other machine in your organization — across NAT, on hotel Wi-Fi, behind a corporate firewall. Connections go straight peer-to-peer when a path exists and fall back to an encrypted relay when nothing else gets through, so "is it reachable?" stops being a question.',
    steps: [
      {
        text: 'Read a device’s overlay address and MagicDNS name straight off the grid (the copy button next to it puts the name on your clipboard).',
        to: { name: 'devices' },
        linkLabel: 'Open Devices',
      },
      {
        text: 'Set the DNS suffix your names live under, so `laptop.<your-org>` resolves fleet-wide.',
        to: { name: 'network-dns' },
        linkLabel: 'MagicDNS settings',
      },
      {
        text: 'Shell into any machine by name — no sshd, no open port, no firewall rule.',
        code: 'roomler ssh <device-name>',
      },
      {
        text: 'Let one machine advertise the subnet behind it, so its LAN becomes reachable from the mesh.',
        to: { name: 'network-subnet-routes' },
        linkLabel: 'Subnet routes',
      },
    ],
    detail: [
      {
        label: 'Addressing',
        text: 'A private address per device plus a MagicDNS name, both stable across reboots and network changes.',
      },
      {
        label: 'Path selection',
        text: 'LAN, then direct over the internet, then a hole-punched path, then an encrypted relay — measured, never assumed, and continuously re-upgraded.',
      },
      {
        label: 'No privileges needed',
        text: 'Where a VPN adapter is unavailable or owned by someone else, a userspace mode gives the same mesh with zero routing changes.',
      },
      {
        label: 'SSH without sshd',
        text: 'Packets for the SSH port are intercepted below the OS, so a machine that cannot host sshd (or has no admin rights) still answers.',
      },
    ],
  },
  {
    id: 'tunnels',
    title: 'Tunnels',
    icon: 'mdi-transit-connection-variant',
    blurb: 'Reach a service without exposing it',
    hero: heroPrivateNetwork,
    heroAlt: 'A local port tunnelled to a service reachable only by a remote machine',
    lead:
      'A tunnel turns anything one of your machines can reach into a local port on yours — a database on a private subnet, an internal web app, a printer. Nothing is published to the internet, the payload stays encrypted end to end, and access is default-deny: a tunnel only works where you allowed it.',
    steps: [
      {
        text: 'Forward a remote service to a local port on your machine.',
        code: 'roomler forward --agent <device-name> --local 127.0.0.1:5432 --remote db:5432',
      },
      {
        text: 'Or route a whole browser through a device’s vantage point with a local SOCKS5 proxy.',
        code: 'roomler socks5 --agent <device-name> --listen 127.0.0.1:1080',
      },
      {
        text: 'Make a forward permanent: declare it in the device’s config and the daemon supervises it across reboots.',
        to: { name: 'devices', query: { type: 'both' } },
        linkLabel: 'Devices & tunnel clients',
      },
      {
        text: 'A machine that should only reach services (no remote desktop) can enroll as a tunnel-only client.',
        to: { name: 'devices', query: { enroll: '1' } },
        linkLabel: 'Enroll a tunnel client',
      },
    ],
    detail: [
      {
        label: 'Nothing exposed',
        text: 'No public port, no inbound firewall rule. The exit machine dials out, the same as every other connection here.',
      },
      {
        label: 'Whole-fleet proxy',
        text: 'SOCKS5 can target one device or the whole mesh, and carries TCP and UDP.',
      },
      {
        label: 'Supervised routes',
        text: 'Declared routes are reconciled on every start with backoff, and a revoked route stops retrying instead of hammering the server.',
      },
    ],
  },
  {
    id: 'acl',
    title: 'Access control',
    icon: 'mdi-shield-lock-outline',
    blurb: 'Who may reach what — default deny',
    hero: heroPrivateNetwork,
    heroAlt: 'Policy gates between machines on the private network',
    lead:
      'Membership of your organization is not the same as permission to reach a machine. Tunnel and overlay policies say which people and devices may reach which destinations, and both start closed: nothing is permitted until you write a rule. Every decision is recorded, and the device itself keeps a local veto that survives even a compromised control plane.',
    steps: [
      {
        text: 'Write your first rules — one page, Overlay and Tunnel tabs.',
        to: { name: 'network-acl' },
        linkLabel: 'Open ACL',
      },
      {
        text: 'Approve the subnets a device is allowed to advertise before its LAN becomes reachable.',
        to: { name: 'network-subnet-routes' },
        linkLabel: 'Subnet routes',
      },
      {
        text: 'Give people only the powers they need — roles carry device, remote-control, SSH and audit permissions separately.',
        to: { name: 'admin-roles' },
        linkLabel: 'Roles',
      },
      {
        text: 'Review what actually happened: every command and SSH session decision is logged.',
        to: { name: 'audit-exec' },
        linkLabel: 'Command audit',
      },
    ],
    detail: [
      {
        label: 'Default deny',
        text: 'An empty policy set grants nothing. Rules are additive and tenant-scoped.',
      },
      {
        label: 'Try before enforcing',
        text: 'Overlay policy has a warn mode: see what would be refused, in the logs, before anything is dropped.',
      },
      {
        label: 'The device has the last word',
        text: 'Remote command execution and SSH are each gated four times over — org switch, your permission, device policy, and a local setting on the machine itself.',
      },
    ],
  },
  {
    id: 'rooms',
    title: 'Rooms & chat',
    icon: 'mdi-forum-outline',
    blurb: 'Conversations, files and threads',
    hero: heroCollaboration,
    heroAlt: 'A team chatting, sharing files and meeting in rooms',
    lead:
      'Rooms are where the people side lives: threaded chat with mentions and reactions, files shared in context, and a call one click away. Rooms nest, so a team can keep a general room with focused children under it, and each room can be open to the org or private to its members.',
    steps: [
      {
        text: 'Create your first room and post something.',
        to: { name: 'rooms' },
        linkLabel: 'Open Rooms',
      },
      {
        text: 'Browse what already exists in the org and join what is relevant to you.',
        to: { name: 'explore' },
        linkLabel: 'Explore rooms',
      },
      {
        text: 'Invite the people who belong in it — by link, or straight to their email.',
        to: { name: 'invites' },
        linkLabel: 'Invites',
      },
      {
        text: 'Everything anyone shared in the org, searchable in one place.',
        to: { name: 'files' },
        linkLabel: 'Files',
      },
    ],
    detail: [
      {
        label: 'Chat',
        text: 'Markdown, mentions, emoji and reactions, threads, editing and pinning, with unread counts per room.',
      },
      {
        label: 'Files',
        text: 'Drag into a room or upload from the Files page; type is detected from the bytes, not from what the browser claimed.',
      },
      {
        label: 'Visibility',
        text: 'Open rooms anyone in the org can join; private rooms only their members can see.',
      },
    ],
  },
  {
    id: 'calls',
    title: 'Calls',
    icon: 'mdi-video-outline',
    blurb: 'Video, audio and screen sharing',
    hero: heroCollaboration,
    heroAlt: 'A video call with several participants and a shared screen',
    lead:
      'Any room can become a call. Video and audio run through a media server built for group calls, so a dozen people in one room is routine, and you can share a screen while you talk. A call in progress is visible from the app bar, so nobody has to be told twice that it started.',
    steps: [
      {
        text: 'Open a room and start a call from its header — anyone in the room can join it.',
        to: { name: 'rooms' },
        linkLabel: 'Open Rooms',
      },
      {
        text: 'Share your screen from the in-call toolbar while you talk.',
      },
      {
        text: 'Watch how the fleet and its calls are actually behaving over time.',
        to: { name: 'analytics' },
        linkLabel: 'Analytics',
      },
    ],
    detail: [
      {
        label: 'Group calls',
        text: 'A selective forwarding media server, not a mesh — participants scale without every browser encoding for everyone.',
      },
      {
        label: 'Screen sharing',
        text: 'Share a screen or window in the call, alongside camera and microphone.',
      },
      {
        label: 'Recording',
        text: 'Calls can be recorded per room where your plan allows it.',
      },
    ],
  },
]

/** Chapters keyed by their URL hash — the view resolves `#devices` etc. */
export function chapterById(id: string): TutorialChapter | undefined {
  return TUTORIAL_CHAPTERS.find((c) => c.id === id)
}
