<template>
  <div class="arch">
    <svg
      viewBox="0 0 560 500"
      width="100%"
      role="img"
      aria-labelledby="arch-title arch-desc"
    >
      <title id="arch-title">Roomler network architecture</title>
      <desc id="arch-desc">
        Four machines — a laptop, a workstation, a home server behind its LAN, and a
        phone — joined in a direct encrypted mesh with stable 100.64 addresses. Thin
        control channels link each machine to the roomler.ai control plane, and a
        live remote desktop stream runs from the workstation to the laptop's browser.
      </desc>
      <defs>
        <linearGradient id="agDesk" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stop-color="rgba(0, 150, 136, 0.35)" />
          <stop offset="1" stop-color="rgba(0, 121, 107, 0.15)" />
        </linearGradient>
        <clipPath id="agScreen"><rect x="69" y="201" width="62" height="40" rx="3" /></clipPath>
      </defs>

      <!-- Control plane: coordination only — keys, ACLs, presence. -->
      <g class="layer" :class="layerCls('control')">
        <circle class="ripple" cx="277" cy="63" r="52" fill="none" stroke="rgba(0, 150, 136, 0.35)" />
        <circle class="ripple" cx="277" cy="63" r="52" fill="none" stroke="rgba(0, 150, 136, 0.35)" style="animation-delay: 1.8s" />
        <path
          d="M240 92 h84 a20 20 0 0 0 7 -39 a30 30 0 0 0 -53 -18 a26 26 0 0 0 -45 18 a20 20 0 0 0 7 39 z"
          fill="#ffffff"
          stroke="rgba(0, 150, 136, 0.45)"
          stroke-width="1.5"
        />
        <text x="277" y="60" text-anchor="middle" font-size="12" font-weight="700" fill="#00796B">roomler.ai</text>
        <text x="277" y="75" text-anchor="middle" font-size="9" fill="rgba(26, 26, 46, 0.55)">control plane</text>
        <path class="ctrl-line" d="M234 84 Q130 130 100 198" />
        <path class="ctrl-line" d="M324 84 Q432 130 464 196" />
        <path class="ctrl-line" d="M250 92 Q175 255 150 388" />
        <path class="ctrl-line" d="M310 92 Q385 255 414 394" />
      </g>

      <!-- WireGuard-style overlay: a direct, end-to-end encrypted mesh. -->
      <g class="layer" :class="layerCls('mesh')">
        <line class="mesh-line" x1="136" y1="222" x2="432" y2="214" />
        <line class="mesh-line" x1="100" y1="250" x2="143" y2="388" style="animation-delay: -0.3s" />
        <line class="mesh-line" x1="128" y1="248" x2="398" y2="404" style="animation-delay: -0.6s" />
        <line class="mesh-line" x1="436" y1="240" x2="176" y2="404" style="animation-delay: -0.9s" />
        <line class="mesh-line" x1="460" y1="250" x2="417" y2="394" style="animation-delay: -1.2s" />
        <line class="mesh-line" x1="196" y1="428" x2="396" y2="428" style="animation-delay: -1.5s" />
        <circle class="pkt" cx="100" cy="250" r="3" style="--dx: 43px; --dy: 138px; animation-duration: 2.4s" />
        <circle class="pkt" cx="436" cy="240" r="3" style="--dx: -260px; --dy: 164px; animation-duration: 3s; animation-delay: 0.8s" />
        <circle class="pkt" cx="196" cy="428" r="3" style="--dx: 200px; --dy: 0px; animation-duration: 2.6s; animation-delay: 1.4s" />
        <circle class="pkt" cx="128" cy="248" r="3" style="--dx: 270px; --dy: 156px; animation-duration: 3.2s; animation-delay: 2s" />
        <rect x="245" y="308" width="110" height="20" rx="10" fill="#ffffff" stroke="rgba(0, 150, 136, 0.35)" />
        <path d="M253 317 v-2 a3 3 0 0 1 6 0 v2" fill="none" stroke="#00796B" stroke-width="1.3" />
        <rect x="252" y="317" width="8" height="6" rx="1" fill="#00796B" />
        <text x="308" y="321.5" text-anchor="middle" font-size="9" fill="#00796B">end-to-end encrypted</text>
      </g>

      <!-- Machines: anywhere — home, office, cloud. -->
      <g class="layer" :class="layerCls('machines')">
        <!-- Home LAN behind the server (subnet-router hint). -->
        <ellipse cx="150" cy="428" rx="92" ry="54" fill="rgba(0, 150, 136, 0.04)" stroke="rgba(26, 26, 46, 0.25)" stroke-dasharray="4 5" />

        <!-- Laptop (you). -->
        <rect x="68" y="200" width="64" height="42" rx="3" fill="#1a1a2e" stroke="rgba(0, 150, 136, 0.5)" />
        <path d="M60 242 h80 l7 9 h-94 z" fill="#cfd8dc" />
        <circle cx="135" cy="197" r="3.5" fill="#4caf50" />
        <circle class="ring" cx="135" cy="197" r="3.5" fill="none" stroke="rgba(76, 175, 80, 0.5)" />
        <text x="100" y="272" text-anchor="middle" font-size="11" font-weight="600" fill="#1a1a2e">laptop — you</text>
        <text class="ip" x="100" y="286" text-anchor="middle" font-size="10" fill="#00796B">100.64.0.12</text>

        <!-- Workstation (the machine you reach). -->
        <rect x="434" y="198" width="58" height="38" rx="3" fill="#1a1a2e" stroke="rgba(0, 150, 136, 0.5)" />
        <rect x="440" y="206" width="28" height="3" rx="1.5" fill="#009688" opacity="0.8" />
        <rect x="440" y="213" width="40" height="3" rx="1.5" fill="#009688" opacity="0.5" />
        <rect x="440" y="220" width="22" height="3" rx="1.5" fill="#009688" opacity="0.65" />
        <rect x="459" y="236" width="6" height="9" fill="#cfd8dc" />
        <rect x="448" y="245" width="28" height="3.5" rx="1.75" fill="#cfd8dc" />
        <circle cx="490" cy="195" r="3.5" fill="#4caf50" />
        <circle class="ring" cx="490" cy="195" r="3.5" fill="none" stroke="rgba(76, 175, 80, 0.5)" style="animation-delay: 0.5s" />
        <text x="462" y="268" text-anchor="middle" font-size="11" font-weight="600" fill="#1a1a2e">workstation</text>
        <text class="ip" x="462" y="282" text-anchor="middle" font-size="10" fill="#00796B">100.64.0.23</text>

        <!-- Home server inside its LAN. -->
        <rect x="130" y="392" width="40" height="50" rx="4" fill="#ffffff" stroke="rgba(26, 26, 46, 0.4)" />
        <rect x="136" y="400" width="28" height="3" rx="1.5" fill="rgba(26, 26, 46, 0.25)" />
        <rect x="136" y="407" width="28" height="3" rx="1.5" fill="rgba(26, 26, 46, 0.25)" />
        <circle cx="138" cy="432" r="2" fill="#4caf50" />
        <circle class="ring" cx="138" cy="432" r="2" fill="none" stroke="rgba(76, 175, 80, 0.5)" style="animation-delay: 1s" />
        <circle cx="145" cy="432" r="2" fill="#009688" />
        <text x="150" y="461" text-anchor="middle" font-size="11" font-weight="600" fill="#1a1a2e">home server</text>
        <text class="ip" x="150" y="475" text-anchor="middle" font-size="10" fill="#00796B">100.64.0.31</text>
        <text x="150" y="490" text-anchor="middle" font-size="9.5" fill="rgba(26, 26, 46, 0.55)">subnet router · 192.168.1.0/24</text>

        <!-- Phone. -->
        <rect x="399" y="398" width="27" height="50" rx="6" fill="#1a1a2e" stroke="rgba(0, 150, 136, 0.5)" />
        <rect x="403" y="406" width="19" height="32" rx="2" fill="#263238" />
        <rect x="406" y="410" width="13" height="3" rx="1.5" fill="#009688" />
        <rect x="409" y="442" width="7" height="2" rx="1" fill="#cfd8dc" />
        <circle cx="427" cy="395" r="3.5" fill="#4caf50" />
        <circle class="ring" cx="427" cy="395" r="3.5" fill="none" stroke="rgba(76, 175, 80, 0.5)" style="animation-delay: 1.5s" />
        <text x="412" y="466" text-anchor="middle" font-size="11" font-weight="600" fill="#1a1a2e">phone</text>
        <text class="ip" x="412" y="480" text-anchor="middle" font-size="10" fill="#00796B">100.64.0.44</text>
      </g>

      <!-- Remote desktop: the workstation's screen, live in the laptop's browser. -->
      <g class="layer" :class="layerCls('remote')">
        <path d="M136 214 Q283 156 430 206" fill="none" stroke="rgba(239, 83, 80, 0.18)" stroke-width="7" />
        <path class="rd-line" d="M136 214 Q283 156 430 206" fill="none" />
        <rect x="217" y="164" width="132" height="20" rx="10" fill="#ffffff" stroke="rgba(239, 83, 80, 0.4)" />
        <text x="283" y="177.5" text-anchor="middle" font-size="9.5" fill="#ef5350">remote desktop · 60 fps</text>
        <!-- Browser on the laptop showing the workstation's desktop. -->
        <rect x="70" y="202" width="60" height="7" fill="#37474f" />
        <circle cx="74" cy="205.5" r="1.2" fill="#ef5350" />
        <circle cx="78" cy="205.5" r="1.2" fill="#ffb300" />
        <circle cx="82" cy="205.5" r="1.2" fill="#4caf50" />
        <rect x="70" y="211" width="60" height="29" fill="url(#agDesk)" />
        <rect x="74" y="215" width="24" height="14" rx="1.5" fill="rgba(224, 242, 241, 0.9)" />
        <rect x="102" y="219" width="20" height="16" rx="1.5" fill="rgba(224, 242, 241, 0.75)" />
        <rect class="shimmer" x="64" y="195" width="14" height="60" fill="rgba(255, 255, 255, 0.14)" clip-path="url(#agScreen)" />
        <rect class="rd-live" x="69" y="201" width="62" height="40" rx="3" fill="none" stroke="#ef5350" stroke-width="1.5" />
      </g>
    </svg>

    <div class="arch-legend d-flex flex-wrap ga-2 justify-center mt-4" role="group" aria-label="Architecture layers">
      <v-chip
        v-for="l in layers"
        :key="l.id"
        link
        size="small"
        :color="l.color"
        :variant="active === l.id ? 'flat' : 'tonal'"
        :aria-pressed="pinned === l.id"
        @mouseenter="hovered = l.id"
        @mouseleave="hovered = null"
        @focus="hovered = l.id"
        @blur="hovered = null"
        @click="pinned = pinned === l.id ? null : l.id"
      >
        {{ l.label }}
      </v-chip>
    </div>
    <p class="arch-caption text-body-2 text-center mt-2" aria-live="polite">
      <!-- Keyed span = the new text fades in on swap; no leave phase to
           wedge (Transition out-in stalls on interrupted swaps). -->
      <span :key="caption" class="cap-swap">{{ caption }}</span>
    </p>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'

type LayerId = 'machines' | 'mesh' | 'remote' | 'control'

const layers = [
  {
    id: 'machines',
    label: 'Machines',
    color: '#455a64',
    caption:
      'Laptop, workstation, home server, phone — at home, in the office, in the cloud. Enroll each one in minutes.',
  },
  {
    id: 'mesh',
    label: 'Encrypted mesh',
    color: '#00796B',
    caption:
      'Every machine gets a stable 100.64.x.x address and talks to every other one directly — WireGuard-style, encrypted end to end.',
  },
  {
    id: 'remote',
    label: 'Remote desktop',
    color: '#ef5350',
    caption:
      'Click a machine, get its desktop in a browser tab — pixel-fresh at 60 fps, streamed through its own encrypted tunnel.',
  },
  {
    id: 'control',
    label: 'Control plane',
    color: '#009688',
    caption:
      'roomler.ai only coordinates: keys, ACLs, presence. Your traffic never flows through our servers.',
  },
] as const

const DEFAULT_CAPTION =
  'Your devices, one encrypted mesh — coordinated from the cloud, connected directly.'

const hovered = ref<LayerId | null>(null)
const pinned = ref<LayerId | null>(null)
const cycled = ref<LayerId | null>(null)
const active = computed(() => hovered.value ?? pinned.value ?? cycled.value)
const caption = computed(
  () => layers.find((l) => l.id === active.value)?.caption ?? DEFAULT_CAPTION,
)

function layerCls(id: LayerId) {
  return { dim: active.value !== null && active.value !== id, hot: active.value === id }
}

// Auto-cycle the spotlight through the story (with a rest frame showing
// the whole scene), pausing while the visitor hovers or pins a chip.
// Never started under prefers-reduced-motion.
const CYCLE: (LayerId | null)[] = ['machines', 'mesh', 'remote', 'control', null]
let cycleTimer: number | undefined
onMounted(() => {
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return
  cycleTimer = window.setInterval(() => {
    if (hovered.value || pinned.value) return
    const i = CYCLE.indexOf(cycled.value)
    cycled.value = CYCLE[(i + 1) % CYCLE.length] ?? null
  }, 4000)
})
onUnmounted(() => {
  if (cycleTimer) window.clearInterval(cycleTimer)
})
</script>

<style scoped>
.arch {
  width: 100%;
  max-width: 560px;
  margin-inline: auto;
}

.ip {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}

/* Layer spotlight — the legend dims everything but the active layer. */
.layer {
  transition: opacity 0.35s ease;
}
.layer.dim {
  opacity: 0.15;
}
.layer.hot .mesh-line {
  stroke-width: 2.25;
}
.layer.hot .ctrl-line {
  stroke-width: 1.75;
}
.layer.hot .rd-line {
  stroke-width: 3;
}

/* Line families. Keyframe offsets equal the dasharray sums so the
   marching loops are seamless. */
.ctrl-line {
  fill: none;
  stroke: rgba(0, 121, 107, 0.5);
  stroke-width: 1.25;
  stroke-linecap: round;
  stroke-dasharray: 2 6;
  animation: arch-march-8 2.6s linear infinite;
}
.mesh-line {
  stroke: rgba(0, 150, 136, 0.55);
  stroke-width: 1.75;
  stroke-linecap: round;
  stroke-dasharray: 7 9;
  animation: arch-march-16 1.8s linear infinite;
}
.rd-line {
  stroke: #ef5350;
  stroke-width: 2.5;
  stroke-linecap: round;
  stroke-dasharray: 10 7;
  animation: arch-march-17 1.1s linear infinite;
}
@keyframes arch-march-8 {
  to {
    stroke-dashoffset: -8;
  }
}
@keyframes arch-march-16 {
  to {
    stroke-dashoffset: -16;
  }
}
@keyframes arch-march-17 {
  to {
    stroke-dashoffset: -17;
  }
}

/* Packets ride the straight mesh edges — plain compositor-friendly
   translate with per-dot endpoints in CSS vars. */
.pkt {
  fill: #009688;
  will-change: transform;
  animation: arch-packet 2.6s ease-in-out infinite;
}
@keyframes arch-packet {
  0% {
    transform: translate(0, 0);
    opacity: 0;
  }
  8% {
    opacity: 1;
  }
  92% {
    opacity: 1;
  }
  100% {
    transform: translate(var(--dx), var(--dy));
    opacity: 0;
  }
}

/* Node heartbeat + control-plane ripple (SVG spin on the app's
   pulse-green idiom — scale a ring instead of a box-shadow). */
.ring,
.ripple {
  transform-box: fill-box;
  transform-origin: center;
}
.ring {
  animation: arch-ring 2.2s ease-out infinite;
}
.ripple {
  animation: arch-ripple 3.6s ease-out infinite;
}
@keyframes arch-ring {
  0% {
    transform: scale(1);
    opacity: 0.55;
  }
  100% {
    transform: scale(2.4);
    opacity: 0;
  }
}
@keyframes arch-ripple {
  0% {
    transform: scale(0.85);
    opacity: 0.5;
  }
  100% {
    transform: scale(1.45);
    opacity: 0;
  }
}

/* Live-stream affordances on the laptop's browser. */
.shimmer {
  animation: arch-shimmer 2.8s ease-in-out infinite;
}
@keyframes arch-shimmer {
  0% {
    transform: translateX(-18px) skewX(-18deg);
  }
  65% {
    transform: translateX(78px) skewX(-18deg);
  }
  100% {
    transform: translateX(78px) skewX(-18deg);
  }
}
.rd-live {
  animation: arch-live 2s ease-in-out infinite;
}
@keyframes arch-live {
  0%,
  100% {
    opacity: 0.9;
  }
  50% {
    opacity: 0.4;
  }
}

.arch-caption {
  color: rgba(26, 26, 46, 0.7);
  min-height: 2.9em;
}
.cap-swap {
  display: inline-block;
  animation: cap-in 0.18s ease;
}
@keyframes cap-in {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}

/* Continuous animation must honour reduced motion: freeze to a static
   poster frame (dashed tunnels stay, transient decorations go). */
@media (prefers-reduced-motion: reduce) {
  .arch * {
    animation: none !important;
  }
  .pkt,
  .ring,
  .ripple,
  .shimmer {
    display: none;
  }
}
</style>
