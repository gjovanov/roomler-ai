// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! S2 config surface — the curated, secret-free slice of [`AgentConfig`]
//! that the LocalAPI `ConfigGet` / `ConfigSet` verbs expose to the
//! desktop app / CLI.
//!
//! One registry drives both verbs: every key carries an editor `kind`
//! (see [`tunnel_core::localapi::ConfigEntry`]), a one-line description,
//! and a per-key parse/validate in [`apply`]. Values travel as strings —
//! bools as `"true"`/`"false"`, lists comma-separated, structured keys
//! (`forward_acl`, `virtual_desktop_apps`) as JSON documents.
//!
//! Deliberately NOT here: secrets (`agent_token`,
//! `overlay_wg_secret_key`), identity (`machine_id`; `machine_name` has
//! its own `SetDeviceName` verb), `[[tunnel_routes]]` (the `Route*`
//! verbs + Routes pane own those), `[[orgs]]` (multi-org P1 — identity +
//! per-org secrets managed by `roomlerd enroll` / the `org` CLI
//! verbs, same policy as `tunnel_routes`; a future desktop org pane gets
//! dedicated LocalAPI verbs, not surface keys), and the crash-bookkeeping
//! fields. Every key is read at daemon startup, so the whole surface is
//! `restart_required = true`.

use crate::config::{AgentConfig, EncoderPreferenceChoice};
use roomler_localapi::ConfigEntry;

/// Mirror of `tunnel_core::overlay::direct::MAX_DIRECT_PORT_BASE`
/// (`u16::MAX - PUBLIC_DIAL_PORT_OFFSET - DIRECT_PORT_BAND`): the largest
/// `overlay_direct_port` base whose public-dial band still fits under 65535.
/// Duplicated rather than imported because `tunnel_core::overlay` lives
/// behind the `overlay` CARGO FEATURE and this surface must compile in every
/// feature combination. Source of truth is `direct.rs`; keep in sync.
const MAX_OVERLAY_DIRECT_PORT_BASE: u32 = 64_759;

/// `(key, kind, description)` for the whole surface, in display order.
/// `kind` is the client-side editor hint contract — see [`ConfigEntry`].
/// CONTRACT (rc.280): a key whose daemon read goes through
/// `tunnel_core::env::node_env` must ALSO appear in
/// [`crate::config::env_bridge_bools`] / `env_bridge_numerics`, else
/// `roomler config set <key>` writes TOML the daemon ignores. The parity
/// test `env_bridge_pairs_have_surface_parity` locks the mapping (and
/// covers set/echo for every bridged key, replacing per-key boilerplate).
const KEYS: &[(&str, &str, &str)] = &[
    (
        "overlay_enabled",
        "bool",
        "Join the L3 overlay mesh (WireGuard-style private network). Default: off.",
    ),
    (
        "overlay_multi_org",
        "bool",
        "Multi-org: let secondary [[orgs]] entries with overlay_mode=\"tun\" join \
         their own tenant's mesh over the ONE shared TUN (same server_url as the \
         primary required). Default: off.",
    ),
    (
        "overlay_advertised_routes",
        "list",
        "CIDRs this node offers to route for overlay peers (subnet router); admin approval required. Comma-separated.",
    ),
    (
        "overlay_exit_node_enabled",
        "bool",
        "Offer this node as an overlay exit node (advertises 0.0.0.0/0; admin approval required). Default: off.",
    ),
    (
        "overlay_exit_node",
        "string",
        "Route ALL of this node's internet egress through the named mesh peer (name or node-id hex). Empty = normal routing.",
    ),
    (
        "advertise_routes",
        "list",
        "CIDRs this host advertises for the tunnel/SOCKS mesh; admin approval required. Comma-separated.",
    ),
    (
        "advertise_local_subnets",
        "bool",
        "Auto-detect and advertise directly-connected IPv4 subnets (untrusted until admin-approved). Default: on.",
    ),
    (
        "auto_grant_session",
        "bool",
        "Auto-approve incoming remote-control session requests without an operator prompt. Default: on.",
    ),
    (
        "enable_remote_browse",
        "bool",
        "Answer remote filesystem-browse requests from the controller. Default: on.",
    ),
    (
        "exec_enabled",
        "bool",
        "Run Fleet-RPC commands sent by the server (commands inherit the daemon's SYSTEM/root identity). Default: OFF.",
    ),
    (
        "macos_supervise_gui_worker",
        "bool",
        "macOS only. Let the root daemon spawn and babysit the GUI-session worker (FR-43 P1). Stands down whenever the LaunchAgent is loaded, so one enrollment is never served twice. Default: OFF.",
    ),
    (
        "power_policy",
        "never|on-ac|always",
        "Ask the OS to stay awake so this device stays reachable (FR-55). `on-ac` is the setting a laptop usually wants. A live remote-control or SSH session ALWAYS holds the machine awake regardless of this. ⚠️ macOS clamshell sleep (lid closed, no external display) ignores it — an OS limit, not a setting. Default: never.",
    ),
    (
        "remote_config_enabled",
        "bool",
        "Accept configuration pushed by the control plane. NEVER settable by the server — it is what keeps exec_enabled/ssh_enabled refusable by a compromised one. Turning it ON delegates that last refusal. Default: OFF.",
    ),
    (
        "ssh_enabled",
        "bool",
        "Serve SSH in-process on this node's overlay address (intercepted before the OS; sessions inherit the daemon's SYSTEM/root identity). Default: OFF.",
    ),
    (
        "ssh_activity_log",
        "bool",
        "Report SSH session activity to the org: commands and their exit codes, and that a shell / SFTP / forward happened. NEVER session content — no pty stream, no command output. Default: OFF.",
    ),
    (
        "ssh_port",
        "string",
        "TCP port intercepted on the overlay address when ssh_enabled is on (1-65535). Empty = built-in default (2222).",
    ),
    (
        "ssh_authorized_keys",
        "list",
        "Comma-separated OpenSSH public keys allowed to open an SSH session. Empty = nobody (ssh_enabled alone grants no access). Set ssh_account_mode too, or these keys authenticate and run nothing.",
    ),
    (
        "ssh_account_mode",
        "string",
        "What an ssh_authorized_keys session runs as: daemon | console_user | named:<account>. Empty = sessions authenticate but run nothing (listing a key must not silently hand out SYSTEM/root).",
    ),
    (
        "ssh_max_privilege",
        "string",
        "Ceiling on what a SERVER-GRANTED ssh session may run as: daemon (or empty) = no device-side limit; console_user = a grant asking for the daemon identity is refused. The device's answer to 'what do I still refuse when the server asking is the compromised thing'.",
    ),
    // `ssh_host_key` is deliberately ABSENT from this surface: it is private
    // key material, and everything here is readable over the LocalAPI.
    (
        "encoder_preference",
        "enum:auto|hardware|software",
        "Video encoder selection: auto (HW probe then fallback), hardware, or software.",
    ),
    (
        "update_check_interval_h",
        "string",
        "Hours between self-update checks (1-8760). Empty = built-in default (24 h).",
    ),
    (
        "overlay_quic",
        "tribool",
        "QUIC-over-TURN overlay carrier. Built-in default: off.",
    ),
    (
        "overlay_direct",
        "tribool",
        "Direct (LAN / hole-punched) overlay carriers. Built-in default: on.",
    ),
    (
        "overlay_derp",
        "tribool",
        "DERP (WebSocket-relay) overlay fallback tier. Built-in default: on.",
    ),
    (
        "overlay_server_relay_strategy",
        "tribool",
        "U2 — accept the server's computed relay-tier verdict instead of the local derivation. Built-in default: off.",
    ),
    (
        "overlay_derp_floor",
        "tribool",
        "Overlay v3 Phase A — DERP always-on floor: keep the central /derp mux open + registered for the whole session, advertise the capability, and floor fresh pairs at birth. Built-in default: on since rc.400.",
    ),
    (
        "overlay_org_relay",
        "tribool",
        "FR-19 P4b - ride a tenant-owned ORG RELAY when the server mints a session for a \
         pair (org switch on, an ACL rule granting each member the relay node, an approved \
         and serving relay). Advertised on the join as supports_org_relay; the serving half \
         is relay_server_enabled. Built-in default: OFF.",
    ),
    (
        "relay_server_enabled",
        "tribool",
        "FR-19 - offer this node as an ORG RELAY: bind relay_server_port and answer \
         reachability probes (it forwards nothing until the relay data path ships). \
         Device-local by design and never server-pushable: this is the refusal that \
         survives a compromised server. Built-in default: OFF.",
    ),
    (
        "relay_server_port",
        "number",
        "FR-19 - UDP port for the org-relay listener (1-65535). Built-in default: 3478, \
         measured rather than guessed - the corp-managed target host reaches 3478 on an \
         arbitrary public IP and no other port. A successful bind does NOT prove \
         reachability: a coturn DNAT can consume the port in PREROUTING while ss shows it \
         free.",
    ),
    (
        "overlay_netcheck",
        "tribool",
        "Overlay v3 Phase B — netcheck: measure egress capabilities (relay-band probe over the dialer path, STUN/NAT, /derp health) every ~20 min and publish the capability vector. Built-in default: on.",
    ),
    (
        "tunnel_derp_fallback",
        "tribool",
        "R4 — tunnel quic-derp-v1 fallback: after repeated quick tunnel session deaths (a corp capture window killing fresh TURN/TLS legs), lead the next attempt with QUIC over the ESTABLISHED /derp WS. Client-side only. Built-in default: off.",
    ),
    (
        "tunnel_peers_survive_reattach",
        "tribool",
        "R3 — keep established tunnel QUIC peers alive across a control-WS reattach instead of tearing them down on every transient WS drop, so a QUIC/derp data plane survives a corp-VPN control-WS blip. Needs the server-side grace. Agent (target) side. Built-in default: off.",
    ),
    (
        "overlay_mbb",
        "tribool",
        "Make-before-break overlay carrier upgrades. Built-in default: on.",
    ),
    (
        "overlay_lan_iface_filter",
        "tribool",
        "LAN-gather virtual-interface filter (skip WSL/Hyper-V/other-VPN adapters). Built-in default: on.",
    ),
    (
        "overlay_wsl_mirrored_guard",
        "tribool",
        "WSL2 mirrored-networking guard: a mirrored guest shares the Windows host's adapters, so skip its LAN gather (binding the host's address starves the host agent). Built-in default: on.",
    ),
    (
        "overlay_init_auth_first",
        "tribool",
        "Auth-first handshake routing on a multi-org carrier plane: route an inbound WG initiation by trial-authentication instead of the source shortcut (fixes the dual-org direct lockout). Built-in default: on.",
    ),
    (
        "overlay_srflx_seek",
        "tribool",
        "srflx SEEKING mode: when the STUN gather finds no public candidate, keep re-gathering with backoff + on interface events instead of staying NONE for the daemon lifetime. Built-in default: on.",
    ),
    (
        "ws_replaced_exit",
        "tribool",
        "LEGACY ReplacedByNewer escalation: exit the process after 3 displacements in the window instead of backing off in-process. Built-in default: off (W4d — zombie-WS storms must not tear down the overlay).",
    ),
    (
        "overlay_warm_relay",
        "tribool",
        "C4 stage 1: keep one standing warm TURN/UDP allocation (established while UDP works) alive across VPN transitions — measurement-only, nothing routes over it yet. Built-in default: off.",
    ),
    (
        "overlay_quic_async",
        "tribool",
        "Raw-first QUIC-over-TURN upgrade: commit the raw relay immediately, rendezvous in the background (90s window), swap in on success. Off restores the blocking 8s pre-install window. Built-in default: on.",
    ),
    (
        "overlay_vpn_vantage",
        "tribool",
        "R2: srflx gather falls back to the wildcard public-dial socket when every LAN-bound vantage is dead (full-tunnel VPN rescue — on AnyConnect-class clients the tunnel is the only path that passes UDP). Built-in default: on.",
    ),
    (
        "overlay_netd",
        "tribool",
        "Track A stage 1 (SCAFFOLD): spawn the session-independent network daemon (roomlerd netd) as a second supervisor child. netd hosts nothing yet; flag read at service start. Built-in default: off.",
    ),
    (
        "overlay_pathmon",
        "string",
        "Overlay PathMonitor mode: on (authoritative — built-in default) | shadow (compare-only revert rail) | off. Env: ROOMLERD_OVERLAY_PATHMON.",
    ),
    (
        "overlay_demote",
        "string",
        "B2 - score-driven demotion of degraded-but-live direct carriers: shadow (count only - built-in default) | on | off. Env: ROOMLERD_OVERLAY_DEMOTE.",
    ),
    (
        "overlay_rpf",
        "string",
        "P4 - ingress filtering of inbound overlay packets: drops a SOURCE address the sending peer does not own, and a DESTINATION outside the subnets this node advertises. warn (count + log, still deliver - built-in default) | enforce | off. Env: ROOMLERD_OVERLAY_RPF.",
    ),
    (
        "overlay_route_events",
        "tribool",
        "Event-driven route guard (OS route-table change subscription; the blind tick backstops it). Built-in default: on.",
    ),
    (
        "overlay_route_tick_secs",
        "number",
        "Route-guard blind-tick seconds while the route-event subscription is live (2-300; 2 = pre-demotion war cadence). Built-in default: 30. Always 2 s without a live subscription.",
    ),
    (
        "overlay_netmon",
        "tribool",
        "netstate - the process-wide network monitor: ONE OS change subscription, typed \
         snapshots/deltas, non-blocking fan-out (the route-event feed and the PR-2 \
         reaction fast lanes ride it). Built-in default: on.",
    ),
    (
        "overlay_netmon_debounce_ms",
        "number",
        "netstate - debounce window in ms coalescing OS signal bursts (a VPN connect \
         injects dozens of routes) into one delta (100-5000). Built-in default: 750.",
    ),
    (
        "netstack_socks_port",
        "number",
        "Multi-org: the loopback SOCKS5 port serving THIS org's userspace netstack \
         (overlay_mode=\"netstack\"). One TCP listener per org — two orgs configured onto \
         one port means the second joins no mesh. The primary reads \
         ROOMLERD_OVERLAY_NETSTACK_SOCKS instead; set this on an [[orgs]] entry. \
         Built-in default: unset (OS-TUN mode).",
    ),
    (
        "rc_max_sessions",
        "number",
        "Concurrent remote-control sessions this agent accepts (1-8). Same-profile DC viewers share one capture+encoder (see shared_encoder); distinct profiles run their own — weak-GPU hosts may prefer 1. Built-in default: 2.",
    ),
    (
        "overlay_direct_port",
        "number",
        "Stable UDP base port for the overlay direct sockets (per-interface LAN; the public/srflx dialer takes base+256). Stateful corp firewalls grandfather pre-VPN UDP flows — a stable port lets a rebuilt carrier reuse the same 5-tuple instead of relay-locking. A swallowed base walks an 8-port band, then the same walk at base+512 (Hyper-V/WSL reserve invisible pools that move between boots). 0 = ephemeral ports. Built-in default: DERIVED per machine (43648 + machine-id-hash slot, 43648..43896) so siblings behind one NAT never collide; set 43648 explicitly to pin the old fleet-wide constant. Env: ROOMLERD_OVERLAY_DIRECT_PORT.",
    ),
    (
        "overlay_iface_metric",
        "number",
        "The overlay NIC's IPv4 interface metric (Windows). Windows ranks a route by route metric + INTERFACE metric; corp endpoint managers (Check Point, AnyConnect) mirror overlay prefixes at route metric 1 on an interface also pinned to 1, producing an exact tie that Windows breaks by lower ifIndex — the VPN's — and the per-destination pick is sticky, so peers stay captured across restarts. Unlike metric-0 routes (which those products delete), an interface metric has no route-monitor hook, so 0 wins outright. Raise only to make the overlay deliberately lose against another interface. Built-in default: 0. Env: ROOMLERD_OVERLAY_IFACE_METRIC.",
    ),
    (
        "shared_encoder",
        "tribool",
        "P5 shared-floor encoder: concurrent same-profile DC viewers share one capture+encoder with floor-merged rate/dials. off = one pipeline per session (rc.302 behaviour). Built-in default: on.",
    ),
    (
        "overlay_relay_tls",
        "tribool",
        "Force overlay coturn allocations onto the TURNS/TCP (TLS) tier — corp-VPN probe. Built-in default: off.",
    ),
    (
        "overlay_shared_carrier",
        "tribool",
        "Multi-org v2 shared carrier plane: every org's engine shares ONE process-wide direct-socket set (receiver-index demux) instead of racing the per-org port band. Built-in default: off.",
    ),
    (
        "overlay_roam",
        "tribool",
        "WG-style endpoint roaming: adopt a peer's observed source after an authenticated inbound from it (repoints the carrier in place). Completes a punch from a symmetric-NAT peer and heals a mid-session NAT rebind. off = strict no-roam demux. Built-in default: on.",
    ),
    (
        "overlay_plane_watchdog",
        "tribool",
        "Carrier-plane socket-liveness watchdog: force a debounced plane rebuild when the shared punch-socket keepalive fails N consecutive cycles (reader-less/wedged socket). off = warn-only. Built-in default: on.",
    ),
    (
        "overlay_session_trace",
        "tribool",
        "Diagnostic: per-session plane-demux + carrier-health INFO traces (inbound src vs expected, poke/proof/rx state). Verbose; enable briefly on an affected host to diagnose a specific peer's carrier. Built-in default: off.",
    ),
    (
        "overlay_disco_respond",
        "tribool",
        "Answer out-of-tunnel disco echoes on the carrier socket (path liveness, answered by the daemon itself — no OS, firewall or tunnel session involved). Answering only; this node does not probe. Built-in default: on.",
    ),
    (
        "overlay_disco_probe",
        "tribool",
        "Probe peers with out-of-tunnel disco echoes and record per-path loss + RTT. Measurement only — nothing acts on the table (scoring is a later stage). Built-in default: off; enable only where every peer already answers.",
    ),
    (
        "overlay_answer_while_followed",
        "tribool",
        "Answer a peer's direct handshake even while that tier is suppressed, when accepting cannot cost the relay (it becomes a shadow probe). The demote-follow hold-down otherwise stops this node ANSWERING for up to 15 min, so two followed ends go mutually deaf and a good LAN pair sits on relay. Built-in default: ON since 0.4.2 (set false as the kill switch).",
    ),
    (
        "overlay_tun_stable_guid",
        "tribool",
        "Stable Wintun adapter identity (constant requested GUID + boot stray-adapter sweep; Windows). Built-in default: on.",
    ),
    (
        "overlay_route_evict",
        "tribool",
        "Route-war eviction of competing VPN-installed routes for overlay prefixes (Windows). Built-in default: on.",
    ),
    (
        "overlay_route_reclaim",
        "tribool",
        "Route-war stolen-path reclaim (targeted evict + cache pin for tie-captured destinations) and evict-on-change debounce (Windows). Built-in default: on.",
    ),
    (
        "overlay_tun_persist",
        "tribool",
        "Keep the overlay TUN device alive across signaling reconnects (process-lifetime cache). Built-in default: on.",
    ),
    (
        "overlay_route_metric0",
        "tribool",
        "Install defended peer /32s (and the ULA /96 + connected /10) at route metric 0 so they outrank a corp VPN's metric-1 mirror routes (Windows). Built-in default: off — an opt-in experiment that auto-yields to metric 1 where a VPN route monitor deletes routes that would win.",
    ),
    (
        "overlay_route_win",
        "tribool",
        "Win the contested prefixes outright instead of evicting a competitor off them forever (Windows). Built-in default: off. Two halves: it pins the overlay adapter's IPv6 interface metric, the lever IPv4 has had since rc.410 and IPv6 never got; and it asserts the derived-ULA /96 AND the connected v4 prefix (the carved block or legacy /10) at the defended route metric 1 instead of the stock connected-route 256. Measured against a corp VPN, v6 was 261 for us vs 26 for the VPN — lost outright, which is why the route guard evicts the VPN's mirrored /96 about 20 times a minute forever on a host whose IPv4 is quiet; the v4 half closes the same gap on the carved block, where the VPN holds it at effective 2 against our 256. Not the metric-0 variant a VPN route monitor deletes outright — it uses the same metric 1 IPv4 runs fleet-wide.",
    ),
    (
        "local_turn",
        "tribool",
        "Loopback-TURN relay for controllers on the same corporate network. Built-in default: on.",
    ),
    (
        "dns_aaaa",
        "tribool",
        "MagicDNS AAAA (IPv6) answers for overlay names. Built-in default: on.",
    ),
    (
        "magicdns_hosts",
        "tribool",
        "MagicDNS hosts-file fallback: when the OS DNS path is MEASURED not to \
         reach the local resolver (a corporate DNS-enforcement layer refusing \
         the host's own queries), write overlay names into the hosts file \
         instead, and remove them again as soon as DNS works. Built-in default: \
         off.",
    ),
    (
        "auto_update",
        "tribool",
        "Periodic self-update checks (also gates web-pushed updates). Built-in default: on.",
    ),
    (
        "logs_upload_disabled",
        "tribool",
        "Disable centralized diagnostic-log upload. Built-in default: uploads on.",
    ),
    (
        "rate_factor_h264",
        "string",
        "H.264 maxrate ceiling factor, % (50-400). Env: ROOMLERD_RATE_FACTOR_H264. Empty = built-in 150. Restart required.",
    ),
    (
        "rate_factor_hevc",
        "string",
        "HEVC maxrate ceiling factor, % (50-400). Env: ROOMLERD_RATE_FACTOR_HEVC. Empty = built-in 125. Restart required.",
    ),
    (
        "rate_factor_vp9",
        "string",
        "VP9 maxrate ceiling factor, % (50-400). Env: ROOMLERD_RATE_FACTOR_VP9. Empty = built-in 125. Restart required.",
    ),
    (
        "rate_factor_av1",
        "string",
        "AV1 maxrate ceiling factor, % (50-400). Env: ROOMLERD_RATE_FACTOR_AV1. Empty = built-in 100. Restart required.",
    ),
    (
        "lanczos_min_pct",
        "string",
        "P7 - minimum linear downscale (percent, 0-100) at which the Lanczos-3 text-sharp filter engages; shallower shrinks use box. Empty = built-in 34 (covers the Smoother rungs; 56 restores the pre-P7 gate; 0 = always). Env: ROOMLERD_LANCZOS_MIN_PCT. Restart required.",
    ),
    (
        "nvenc_spatial_aq",
        "tribool",
        "P7 - NVENC spatial AQ. Built-in default: OFF (AQ steals bits from desktop text); true restores it for camera-heavy hosts. Env: ROOMLERD_NVENC_SPATIAL_AQ. Restart required.",
    ),
    (
        "scale_cq_boost",
        "string",
        "P7 - CQ sharpening steps granted at deep resolution rungs (0-12; spends the maxrate-floor headroom on text). Empty = built-in 4; 0 disables. Env: ROOMLERD_SCALE_CQ_BOOST. Restart required.",
    ),
    (
        "idle_refine",
        "tribool",
        "P7 - idle native-rung refinement: lift the resolution cap when the scene settles so text is crisp at rest; motion restores it in ~300 ms. Built-in default: on (Smoother scope). Env: ROOMLERD_IDLE_REFINE. Restart required.",
    ),
    (
        "idle_refine_balanced",
        "tribool",
        "P7 - idle refinement on Balanced+relay sessions (lifts the B1 physics cap at idle). Built-in default: on since P7c (field-proven on the winhost-b relay); off restores the un-refined Balanced rung. Env: ROOMLERD_IDLE_REFINE_BALANCED. Restart required.",
    ),
    (
        "gpu_scale",
        "tribool",
        "HW-downscale Phase B - GPU scale-before-readback (D3D11 VideoProcessor) on DXGI-direct capture: the Smoother rung is scaled on the GPU and the readback shrinks with it. Built-in default: on; off reverts to the Phase-A CPU resample. Env: ROOMLERD_GPU_SCALE. Restart required.",
    ),
    (
        "overlay_lan_capture_probe",
        "tribool",
        "FR-33 - probe each LAN prefix for a corp-VPN split-prefix capture (own address on interface A, traffic to the prefix leaves via interface B) and surface it in status / why / the RC pill. Built-in default: on (a read-only route lookup per LAN address per netstate snapshot). Env: ROOMLERD_OVERLAY_LAN_CAPTURE_PROBE. Restart required.",
    ),
    (
        "relay_ceiling_learn",
        "tribool",
        "FR-35 - let the constrained (relay) ceiling grow above the nominal 3 Mbps on delivery evidence (AIMD pinned at the ceiling, the window carried >=70% of it, no decrease/stall for 10 s, viewer age within 1.5x floor) and remember the pair's stable rate so the next session opens there. Built-in default: on. Env: ROOMLERD_RELAY_CEILING_LEARN. Restart required.",
    ),
    (
        "drm_capture",
        "tribool",
        "FR-36 - capture the scanout framebuffer via DRM/KMS, BELOW the compositor. The only backend that can see a Wayland desktop, a locked screen or the login greeter (the xdg portal refuses all three). Built-in default: OFF - it carries no damage information, so enabling it where X11 works costs the FR-29 idle-CPU win. Env: ROOMLERD_DRM_CAPTURE. Restart required.",
    ),
    (
        "uinput",
        "tribool",
        "FR-36 - inject input through /dev/uinput, below the compositor. Pair with drm_capture on a Wayland host: XTest reaches Xwayland clients ONLY, so without this a captured Wayland session is read-only. Built-in default: OFF - a uinput device is host-global and injects into whatever has focus, including the greeter and lock screen. Env: ROOMLERD_UINPUT. Restart required.",
    ),
    (
        "portal_capture",
        "tribool",
        "FR-45 - capture a Wayland desktop through xdg-desktop-portal ScreenCast + PipeWire. The ATTENDED path: needs a logged-in user session, and the first use shows that user a consent dialog (later ones restore the grant without asking). Serves hosts DRM cannot reach - no scanout, nested compositors. Built-in default: OFF - an unattended host would wait forever on a dialog nobody answers. Tried after DRM, before X11. Env: ROOMLERD_PORTAL_CAPTURE. Restart required.",
    ),
    (
        "portal_input",
        "tribool",
        "FR-45 P4 - inject input through the portal RemoteDesktop interface, riding the SAME portal session (one consent dialog covers see+touch). Built-in default: OFF - a WithInput session needs its own see+touch consent + restore token, so enabling it makes every portal capture prompt afresh (and block or fall through if unanswered), regressing capture where capture-only works; it has also not yet been field-proven to land input. On = the portal session can be controlled; inert unless portal_capture is on. NOTE (measured on GNOME): this key alone is not enough - the portal's consent dialog carries a SEPARATE 'Allow Remote Interaction' switch that defaults OFF, so a human who only clicks Share grants capture and the session runs view-only. Env: ROOMLERD_PORTAL_INPUT. Restart required.",
    ),
    (
        "mutter_capture",
        "tribool",
        "FR-45 P5 - take screencast frames from org.gnome.Mutter.ScreenCast DIRECTLY instead of the desktop portal. For hosts where no portal backend can run at all (measured on WSL2: xdg-desktop-portal-gnome exits without a GNOME session, while mutter itself works). GNOME-only. Built-in default: OFF. ** UNATTENDED - this shows NO consent dialog; its peer is drm_capture, not portal_capture.** Env: ROOMLERD_MUTTER_CAPTURE. Restart required.",
    ),
    (
        "window_capture",
        "tribool",
        "FR-56 P4 - capture ONE application window instead of the whole monitor (the RAIL-shaped half of Remote Apps: the viewer sees one app, not the desktop). Built-in default: OFF. ** ATTENDED BY CONSTRUCTION - the portal answers this by showing the person at the screen a WINDOW PICKER, and nothing agent-side can name a window (GNOME refuses Introspect.GetWindows), so on an unattended host the capture never starts.** Env: ROOMLERD_WINDOW_CAPTURE. Restart required.",
    ),
    (
        "x11_damage",
        "tribool",
        "FR-29 - skip the XShm readback when XDAMAGE proves the screen is unchanged. Built-in default: on; took a Linux host's idle capture from 45.8% of a core to 2.8%. Env: ROOMLERD_X11_DAMAGE. Restart required.",
    ),
    (
        "overlay_key_rotation",
        "tribool",
        "FR-40 - honour rc:agent.key_rotate: an admin retiring this device's overlay (WireGuard) key from the dashboard. The device mints the new key locally, persists it and re-joins the mesh under it; the server never sees a private key. Built-in default: on (a kill switch, not a gate - the order leaks nothing). Env: ROOMLERD_OVERLAY_KEY_ROTATION. Restart required.",
    ),
    (
        "relay_max_hi_kbps",
        "string",
        "FR-35 - upper bound (kbps, 0-100000) for the learned relay ceiling. Empty = built-in 8000 (one pair measured: sustained 6-9 Mbps, choked at 12.8); 0 = learning off. Env: ROOMLERD_RELAY_MAX_HI_KBPS. Restart required.",
    ),
    (
        "idle_refine_max_edge",
        "string",
        "P7 - long-edge cap for the refined rung (0-8192). Empty/0 = full native. Env: ROOMLERD_IDLE_REFINE_MAX_EDGE. Restart required.",
    ),
    (
        "idle_refine_min_frame_kb",
        "string",
        "P7c - encoded-size floor (KiB, 0-256) for a frame to count as motion in the idle-refine machine; caret/keystroke deltas stay invisible so terminals keep the crisp rung while typing. Defined at the 1024x640 reference rung and scaled by the live encode area (P7c-2 - a fixed floor oscillated across rungs). Empty = built-in 12; 0 = every real frame counts (pre-P7c). Env: ROOMLERD_IDLE_REFINE_MIN_FRAME_KB. Restart required.",
    ),
    (
        "idle_refine_major_area_permille",
        "string",
        "P8a-2 - MAJOR-motion area floor (permille of the frame, 0-1000) on capture-tracked backends (DXGI-direct/WGC): only damage at/above it restores the resolution cap; smaller damage (typing, popups, windowed terminal scrolls, PiP video) stays at native so text is sharp all the time. Empty = built-in 400 (40%); 0 = any non-empty tracked damage counts (pre-P8a-2 posture). Env: ROOMLERD_IDLE_REFINE_MAJOR_AREA_PERMILLE. Restart required.",
    ),
    (
        "idle_refine_settle_ms",
        "string",
        "P8a-2 - up-flip settle (ms, 100-5000) on capture-tracked backends: the cap lifts this long after the last major-damage frame (damage truth needs no 1s window drain). Empty = built-in 500. Env: ROOMLERD_IDLE_REFINE_SETTLE_MS. Restart required.",
    ),
    (
        "idle_refine_settle_constrained_ms",
        "string",
        "Phase B - tracked settle (ms, 100-10000) on CONSTRAINED transports: the cap lifts only after this long without major damage, because the refined IDR itself costs link time and a 500 ms settle fired on ordinary drag pauses (field: freezing/lag). Empty = built-in 1200 (2000 before the constrained HRD trim bounded the IDR). Env: ROOMLERD_IDLE_REFINE_SETTLE_CONSTRAINED_MS. Restart required.",
    ),
    (
        "constrained_cq_relief",
        "string",
        "Constrained-motion CQ relief (softening steps, 0-12) applied at the resolution rung of a RELAY session; the rung exists for motion fluidity and softer frames arrive steadily instead of in lumps (field 2026-08-21: the sharpening bias at the rung was the 9 fps equilibrium). At-rest native quality is untouched; an explicit resolution pick is exempt. Empty = built-in 4; 0 = no relief. Env: ROOMLERD_CONSTRAINED_CQ_RELIEF. Restart required.",
    ),
    (
        "constrained_queue_ms",
        "string",
        "Constrained send-queue byte budget (ms of the relay ceiling, 0-2000): frame production skips while more than this much link time is queued, converting viewer lag into a small fps reduction (field 2026-08-21: the drag-start freeze was ~0.5-1 MB of native motion frames queued on a ~2 Mbps relay). Empty = built-in 450; 0 = unbounded (pre-rc.442). Env: ROOMLERD_CONSTRAINED_QUEUE_MS. Restart required.",
    ),
    (
        "constrained_hrd_pct",
        "string",
        "HRD/VBV window for CONSTRAINED sessions (percent of maxrate, 25-200). Empty = built-in 200 (the rc.234 2x window; rc.442 defaulted 75 to bound IDR transit and rc.443 reverted it - av1_qsv errors and hangs on a forced IDR that exceeds a sub-1x reservoir). Sub-100 values are per-host experiments only. Env: ROOMLERD_CONSTRAINED_HRD_PCT. Restart required.",
    ),
    (
        "direct_queue_ms",
        "string",
        "DIRECT-path send-queue byte budget (ms of the path's rate ceiling, 0-2000; it was ms of the AIMD's live target until FR-74 P1 measured that as a self-reinforcing trap): frame production skips while more than this much link time is queued, bounding the standing lag a drag burst can build on a direct session (field 2026-08-26: 100-345 KB queued = the sluggish, rubber-band drag). Empty = built-in 150; 0 = unbounded (pre-P1 posture). Env: ROOMLERD_DIRECT_QUEUE_MS. Restart required.",
    ),
    (
        "direct_hrd_pct",
        "string",
        "HRD/VBV window for DIRECT sessions (percent of maxrate, 25-200). Empty = built-in 100 - half the rc.234 2x window, which legalised drag-start bursts of seconds' worth of bits (the standing-queue lag). av1_* encoders are floored at 200 regardless (rc.443: Intel AV1 VDENC errors on an over-reservoir IDR instead of QP-clamping). Env: ROOMLERD_DIRECT_HRD_PCT. Restart required.",
    ),
    (
        "bg_rebuild",
        "tribool",
        "Background encoder rebuild (2026-08-27, drag-latency P3). Default ON: on encoders with no in-place bitrate reconfigure (QSV/AMF), a bitrate change opens the replacement on a blocking thread while the current encoder keeps producing, then swaps between frames - no mid-drag stall, and rate drops land DURING motion as smaller frames instead of production skips. false = the rc.445 motion-defer (applies held until 1.2s of quiet, then a blocking re-open). Env: ROOMLERD_BG_REBUILD. Restart required.",
    ),
    (
        "par_convert",
        "tribool",
        "Parallel colour conversion (2026-08-27, drag-latency P5). Default ON: big frames run the BGRA->NV12/I444 convert in row bands across threads - byte-identical output, roughly halves the convert share of encode time at 2880x1800+. false = single-threaded convert. Env: ROOMLERD_PAR_CONVERT. Restart required.",
    ),
    (
        "fps_pace",
        "tribool",
        "fps-first cadence pacing on HW encoders (2026-08-27, drag-latency P5). Default ON: when the encoder cannot hold target fps, frames are consumed on an EVEN grid at the sustainable rate (5 fps steps, floor 15) instead of dropping ~33% at random phases - even cadence beats a jittery higher rate. While engaged the encode-pressure bitrate factor is masked at 1.0 (pixels-bound HW encode time does not respond to bitrate); the resolution tier stays the second lever. false = unpaced pre-P5 behaviour. Env: ROOMLERD_FPS_PACE. Restart required.",
    ),
    (
        "relay_idr_thrift",
        "tribool",
        "Relay IDR thrift (2026-08-27, FR-10). Default ON: constrained (relay) sessions suppress the idle-settle keyframe (a quality refresh, not a correctness need on a reliable DataChannel - the request-driven resync stays) and space deferred bitrate re-opens to >=15s unless the move is >=40%. Each such IDR was a single ~300 KB frame = 1.2-1.5s of a ~2 Mbps relay (the CORPLAP-3 bulky lumps). false = previous relay behaviour. Direct sessions unaffected. Env: ROOMLERD_RELAY_IDR_THRIFT. Restart required.",
    ),
    (
        "send_stall_ms",
        "number",
        "Blocked-send congestion threshold in ms (2026-08-28, FR-15 P2 follow-up). Default 250; 0 disables. A frame that sat longer than this inside the DataChannel send call is unambiguous congestion - the pipe refused to drain - and the pump feeds the AIMD a congestion sample. This is the one congestion signal that needs NO clock sync and NO viewer and works on both transports, which matters on a relay where the measured-rate clamp is direct-only and the age loop rides a probe the congestion itself biases. Acted on for CONSTRAINED sessions only; direct keeps the measured ceiling. Env: ROOMLER_NODE_SEND_STALL_MS. Restart required.",
    ),
    (
        "relay_age_feedback",
        "tribool",
        "Relay age feedback (2026-08-27, FR-15). Default ON: the viewer reports the true paint AGE of the frames it showed (the FR-1 P7 clock probe) on its rc:decodestat window; the agent learns the session's age FLOOR and treats sustained excess (>=70ms over floor for 2 consecutive windows) on a CONSTRAINED transport as over-rate - capping send-fps and feeding the AIMD a congestion sample, so the decrease lands through the normal (FR-10-deferred) apply path. It exists because a relay backlog sits BELOW every agent counter: the field measured 1000ms of viewer age against a 26KB agent queue. false = open-loop 0.4.7 relay posture. Direct sessions unaffected. Env: ROOMLERD_RELAY_AGE_FEEDBACK. Restart required.",
    ),
    (
        "measured_ceiling",
        "tribool",
        "Measured-rate stage 1 (2026-08-27). Default ON: the bitrate ceiling is clamped to 85% of the session's MEASURED drain rate while an estimate holds, so the encoder converges just under the pipe instead of congesting the send queue on every drag burst (the chunky production skips). Only ever lowers the nominal ceiling; confidence decays after 60s without evidence. false = observe-and-report only. Env: ROOMLERD_MEASURED_CEILING. Restart required.",
    ),
    (
        "encoder_inplace_rate",
        "tribool",
        "In-place encoder rate changes (2026-09-02, FR-62 A1). Default OFF: a QSV rate move REBUILDS the encoder (a 0.65-0.87 s blocking open on Iris-Xe-class, the reason the defer/swap machinery exists), and the NVENC in-place move writes a 1x HRD buffer. ON: QSV writes bit_rate + rc_max_rate + rc_buffer_size on the AVCodecContext (qsvenc's per-frame update_bitrate resets the BRC, no rebuild) and NVENC sizes the buffer to the window the session opened with. Ships OFF and inert until A0 clears the QSV MFXVideoENCODE_Reset on real Iris-Xe silicon; OFF is byte-for-byte the pre-A1 behaviour. Env: ROOMLERD_ENCODER_INPLACE_RATE. Restart required.",
    ),
    (
        "ice_relay_tcp",
        "tribool",
        "Pin remote-control's ICE to a TURN relay (diagnostic). Default OFF: the session takes whatever pair ICE nominates, and `constrained` is MEASURED from that pair (a public relay candidate = constrained; the loopback-TURN does not count). ON forces the relay path, which is bandwidth- and head-of-line-limited, so the encoder runs its constrained posture. ⚠️ This DEGRADES a session that would otherwise be direct - it is a test pin, not a tuning knob, and a device left with it set will be slow for no visible reason. It exists as a key because the constrained posture is otherwise only reproducible when a corporate VPN happens to be up, which makes every constrained-path acceptance test hostage to one laptop's network state; virtual-desktop mode sets the same flag for the same reason. Clear it (empty = default) when the measurement is done. WARNING: on a VIRTUAL-DESKTOP host with a hostile NAT the vd startup auto-pins this to 1, and its check for an explicit operator override reads the OS env var ONLY - so setting this key to false there does not defeat the auto-pin; use a real ROOMLERD_ICE_RELAY_TCP=0 for that one case. Env: ROOMLERD_ICE_RELAY_TCP. Restart required.",
    ),
    (
        "relay_max_kbps",
        "number",
        "Bitrate ceiling for a CONSTRAINED (relay) remote-control transport, kbps. Built-in default: 3000; clamped 100-100000. A single TURN relay carries roughly 1-4 Mbps and head-of-line blocks on TCP, so a ceiling sized for a direct pair (a 1920x1200 pair resolves ~12 Mbps) collapses it. LOWER it on a relay population that is thinner than the default assumes. RAISE it only to build a deliberate over-drive for a rate-control measurement: field 2026-09-03, a forced-relay cell on CORPLAP-1 opened at the 2.55 Mbps plan rate into a coturn carrying ~3 Mbps, which is NOT an over-drive - the AIMD simply climbed to the cap and viewer age stayed at 30-49 ms, so the FR-63 A/B had nothing to measure. At 12000 the same real pipe and real encoder give a genuine 4x over-drive on demand, instead of waiting for a corporate VPN to produce one. Pairs with ice_relay_tcp. Env: ROOMLERD_RELAY_MAX_KBPS. Restart required.",
    ),
    (
        "rate_slow_start",
        "tribool",
        "Slow-start the session opener (2026-09-03, FR-63). Default OFF. A session commits to a bitrate before it has any evidence about the pipe, and the same host over-drove from BOTH directions on one day: opened at a REMEMBERED 6134627 -> 6287ms of viewer paint age; opened at the NOMINAL relay cap 2550000 into a path measured at ~213000 -> 444ms of queue, 1550ms paint, and six windows collapsing back down. No constant is safe, because a constant is an assumption about a band. ON: open at 300000 (lifted by any PROVEN floor, e.g. the FR-59 P8 remembered-slow-pair open) and DOUBLE per clean window until the ceiling; the first congestion evidence ends the ramp and hands control back to the normal controller. A fast pair reaches a 6.1 Mbps ceiling in 5 windows. Only ever LOWERS the opening commitment - it can never raise a rate above what the controller already allows. Env: ROOMLERD_RATE_SLOW_START. Restart required.",
    ),
    (
        "rate_prior_decay",
        "tribool",
        "A remembered rate is a prior, not a pin (2026-09-04, FR-70 P1). Default ON. The rate memory's number for a pair opens the session (FR-59 P8) and, until something measures the pipe, also stands in for the measurement: the legibility floor is relieved to 85% of it and the send-queue byte budget is denominated in it. Field 2026-09-04 (CORPLAP-1 -> neo16): a 200 kbps memory held a session at 200 kbps for four minutes with goodput=None, zero send stalls and zero viewer-congested windows - the queue budget (16 KB) tripped on every drag frame, each trip was an AIMD decrease, and a queue that never forms is a pipe that can never be measured, so nothing could ever contradict the memory. ON: while no live measurement exists the stand-in climbs x1.25 per 10 clean windows (+12.5% per 5 s, the AIMD's own slow-band step) toward the 1.5 Mbps band, the floor and the budget follow it up, and a live measurement (blocked-send goodput or the viewer's arrival rate while its queue grows) becomes the new base at once. A genuinely slow pipe is over-driven by no more than the AIMD would have and gets MEASURED within a window or two; a misremembered fast pair reaches the band in ~100 s. The session end records the measurement or the decayed prior instead of the last window's applied rate, which on a lumpy relay is wherever the last decrease left it. false = FR-59 P8 verbatim (the seed is a constant for the session). Env: ROOMLERD_RATE_PRIOR_DECAY. Restart required.",
    ),
    (
        "transit_classify",
        "tribool",
        "Classify each constrained viewer window by which plane is the limiter (2026-09-05, FR-71 T1a). Default ON, and SHADOW ONLY: the heartbeat prints pipe_state (clear / overproduced / transit-stalled / viewer-late / unknown) with per-state counts, and nothing acts on the verdict until T1b (transit_hold). It exists because every rate loop reads the fused paint age as the encoder having produced too much: on 2026-09-04 a DERP/TCP head-of-line block held frames in transit for 4.9 s while the send queue held 1485 bytes, and the rate was cut into a link that was never the limiter. The verdict reads the FR-70 M0 age split (sender / transit / viewer) beside the sender's own queue: a full or gated send queue is overproduced whatever the path does; transit 200 ms over its learned floor with the browser near its floor is transit-stalled; the browser 100 ms over its floor, or struggling, is viewer-late; no split (a pre-M0 viewer) is unknown and changes nothing. false = no classification, no counters. Env: ROOMLERD_TRANSIT_CLASSIFY. Restart required.",
    ),
    (
        "transit_hold",
        "tribool",
        "Act on a transit-stalled window (2026-09-05, FR-71 T1b). Default OFF for one release: a controller change ships behind the shadow classifier's evidence. ON: when transit_classify calls a constrained viewer window transit-stalled (the send queue passed every sender-side check while the frames were held beyond it), the rate loops leave that window alone - the opener's ramp neither steps nor ends, the FR-15 age loop does not fire, the FR-59 P3 arrival clamp is held rather than re-armed or released, and the rate prior takes no push-back; the FR-59 P4 drain still runs, because a pause is a drain, not a cut. The heartbeat counts held windows as transit_holds. It exists because the alternative is what happened on 2026-09-04: a 4.9 s DERP head-of-line block read as over-production and the rate was cut into a link that was never the limiter. Needs transit_classify. false = classify only, act as today. Env: ROOMLERD_TRANSIT_HOLD. Restart required.",
    ),
    (
        "media_thread",
        "tribool",
        "The encoder runs on its own OS thread per session (2026-09-05, FR-70 M1). Default ON since 0.4.70 - M1c met its gate on all three CORPLAP hosts on 0.4.69 (encode and capture averages unchanged, the loop's worst pass per window down on every host, the >50 ms windows fewer). ON: the FFmpeg encoder lives on a dedicated thread named rc-enc-<session> behind a command channel - the pump sends each frame and awaits the packets, and every rate move, keyframe request and background-rebuild adoption is a message applied in order - instead of encoding under block_in_place on whichever async worker happens to poll the pump. Nothing the pump decides changes: same frame, same decision, same packet, one thread hop later. What changes is that the async runtime is never held for the 5-30 ms of an encode (the send task, the control channel and the heartbeats stop sharing a worker with it) and hardware encoders that are thread-affine (Media Foundation per-thread COM, QSV sessions) are driven from one thread for the whole session. A thread that cannot be spawned falls back to the inline path with a warning; a thread that dies surfaces as the next encode's error, which the existing error ladder turns into a rebuild. Gate for flipping the default: FR-65's iter_ms_max / pump_stalls / apply_ms_max on the three CORPLAP hosts, unchanged or better. false = the inline encode, the pre-0.4.70 path. Env: ROOMLERD_MEDIA_THREAD. Restart required.",
    ),
    (
        "pump_stall_watch",
        "tribool",
        "Pump stall watch (2026-09-03, FR-65 P0). Default ON: a send-pump iteration slower than pump_stall_warn_ms is logged once with its phase breakdown (capture/scale/encode/apply/send), and the per-heartbeat apply_us / apply_us_max / iter_us_max / pump_stalls counters are published. Costs two Instant::now() per iteration (~20-40ns against a 16.7ms budget at 60fps) and logs nothing until an iteration actually overruns. It exists because a 2s blocking encoder open hid for months: the pump measured capture/scale/encode/send and the stall appeared in NONE of them - the apply/rebuild phase was untimed, and a per-heartbeat AVERAGE cannot represent a single outlier even where it is counted. false = no timing, no counters. Env: ROOMLERD_PUMP_STALL_WATCH. Restart required.",
    ),
    (
        "pump_stall_warn_ms",
        "number",
        "Pump stall threshold in ms (2026-09-03, FR-65 P0). Built-in default: 100; clamped 10-5000. Lowered from the 250 this shipped with, because the first field data said 250 was blind to the class that actually hurts: a corp-VPN host reported iter_ms_max=107.6 - real 100ms+ passes, matching the operator's own '>100ms' and '>148ms' age reports - while pump_stalls stayed 0. Deliberately a FLAT wall-clock threshold, NOT a multiple of the frame budget: the pump lowers target_fps BECAUSE it is already struggling, so a budget-relative bar RISES as the session degrades and stops reporting precisely when the trouble starts. Env: ROOMLERD_PUMP_STALL_WARN_MS. Restart required.",
    ),
    (
        "bg_rebuild_constrained",
        "tribool",
        "Off-thread encoder rebuild on CONSTRAINED transports too (2026-09-03, FR-65). Default ON: a rebuild-mode encoder open is 0.65-0.87s of BLOCKING work on Iris-Xe-class silicon, and running it on the send pump stalls capture, encode and send together - measured as a ~2s hole. The open now runs on spawn_blocking for constrained paths as it already did for direct ones. Changes only WHERE the open runs, never WHEN the change lands: adoption stays gated on the same quiet window the defer policy uses, so the swap's IDR still arrives on a static scene - adopting mid-motion on a thin pipe is the 2026-08-27 relay regression that put the !constrained guard there originally. false = rebuild inline on constrained paths (pre-FR-65). Env: ROOMLERD_BG_REBUILD_CONSTRAINED. Restart required.",
    ),
    (
        "slow_link_floor",
        "tribool",
        "Slow-link floor relief (2026-09-01, FR-59 P1). Default ON: on a CONSTRAINED transport the AIMD legibility floor descends toward the session MEASURED drain rate instead of pinning at the flat 1.5 Mbps MIN_BITRATE_BPS. That flat floor is calibrated for the 2-9 Mbps band every measured relay sat in; on a slower link it is not a floor but a PIN, because it is also where the multiplicative decrease bottoms out - field 2026-09-01 measured a 395 kbps pipe met by a 1.5 Mbps floor, 3.8x over, with the excess landing as 2.3-7.1 s of viewer paint age. Evidence-gated: with no held goodput estimate the nominal floor stands, so a session that never measures is byte-for-byte unchanged. Never descends below slow_link_min_bitrate. false = flat floor (pre-FR-59). Env: ROOMLERD_SLOW_LINK_FLOOR. Restart required.",
    ),
    (
        "slow_link_min_bitrate",
        "number",
        "Absolute stop for the FR-59 P1 floor relief, bps (50000-1500000). Empty = built-in 200000. Below roughly this a full-resolution frame is illegible at any QP, so the honest lever is fewer PIXELS rather than fewer bits; the relief exists to let the AIMD converge onto a slow pipe, not to chase it to zero. A value at or above the nominal 1.5 Mbps floor is inert by construction. Env: ROOMLERD_SLOW_LINK_MIN_BITRATE. Restart required.",
    ),
    (
        "constrained_queue_measured",
        "tribool",
        "Constrained queue budget denominated in the MEASURED rate (2026-09-01, FR-59 P2). Default ON: the constrained send-queue byte budget is re-derived each iteration from the session measured drain rate instead of being resolved once against the nominal relay ceiling. A budget expressed in MILLISECONDS is a lie unless the bits-per-second it divides by is the pipe: constrained_queue_ms 450 against a nominal 3 Mbps is 168750 bytes, which on a measured 395 kbps link is 3.4 SECONDS of standing queue - and the gate never fired while the viewer sat seconds behind. A held measurement may only ever LOWER the reference. Note this consumes the same lumpy TURN-TCP estimate measured_ceiling deliberately refuses for the CEILING; the asymmetry is the point, since an under-estimate here shrinks the budget (more shedding, LOWER latency) where an under-estimated ceiling collapses quality. false = pre-FR-59. Env: ROOMLERD_CONSTRAINED_QUEUE_MEASURED. Restart required.",
    ),
    (
        "seed_contradiction",
        "tribool",
        "Abandon a contradicted rate-memory seed (2026-09-01, FR-59 P6). Default ON: a held goodput measurement more than 2x below the FR-35 learned or seeded ceiling abandons it back to the nominal band. The rate memory keys on the nominated ICE pair remote address, which on a RELAYED session is the relay address rather than the viewer - so one fast day writes a number every later session through that relay inherits for the memory 7-day TTL, whatever network the client is on today (field 2026-09-01: a 5069353 bps seed opened a session on a hotspot measured at 395122 bps, 12.8x under it). Applies to an in-session learned ceiling too, since a measurement is evidence either way and re-climbing is something the learner already does. false = keep the seed until the AIMD walks it down. Env: ROOMLERD_SEED_CONTRADICTION. Restart required.",
    ),
    (
        "viewer_rate_clamp",
        "tribool",
        "Viewer-reported link clamp (2026-09-01, FR-59 P3). Default ON: the VIEWER reports the bytes/s it actually received and how much its transit queue GREW this window, and on a constrained transport a sustained growing queue caps send-fps, feeds the AIMD a congestion sample, and bounds the ceiling at 90% of the measured arrival rate. It exists because the agent structurally cannot see this: on a relayed path its own send channel reads empty (field 2026-09-01: bytes_inflight 1-4 KB, send_wait_max_ms 0.1 ms) while seconds of video sit in the relay and the carrier. Unlike the FR-15 age report it needs NO clock probe - a byte count is local and the queue drift is a difference of two intervals, so the unknown offset cancels - which matters because on exactly these links the age is absent or rejected in most windows. The arrival rate may bound the ceiling ONLY while the queue is growing, since otherwise it is merely whatever the agent happened to send. false = observe-and-report only. Env: ROOMLERD_VIEWER_RATE_CLAMP. Restart required.",
    ),
    (
        "queue_drain",
        "tribool",
        "Queue drain (2026-09-01, FR-59 P4). Default ON: when the viewer reports a transit queue deeper than a rate cut can clear in reasonable time, the pump STOPS producing for a bounded sub-second pause so the queue drains. A rate cut alone drains at capacity minus inflow, which is the slowest possible way - converging to 90% of a 400 kbps pipe clears a 2 s backlog at 40 kbps, i.e. over ~20 s, which is why a field session stayed seconds behind even after it stopped growing. Pausing sets inflow to zero so the same backlog clears in the ~2 s it represents. Deliberately no forced keyframe on resume: a pause loses no frames so the delta chain survives, and an IDR at these rates is itself seconds of transit. Skipping production rather than discarding the agent queue is the only lever that reaches a queue living in the relay and the carrier - those bytes are already sent and cannot be recalled. false = rate control only. Env: ROOMLERD_QUEUE_DRAIN. Restart required.",
    ),
    (
        "slow_link_profile",
        "tribool",
        "Slow-link opening profile (2026-09-01, FR-59 P5). Default ON: a CONSTRAINED session whose pair the rate memory remembers at or below slow_link_profile_bps opens with a 1280 long-edge cap and 15 fps instead of native. The bitrate levers (FR-59 P1-P4) can make the encoder TRACK a 400 kbps pipe but cannot make 1920x1200 at 30 fps legible through it - that is about 1.7 KB per frame; halving the long edge quarters the pixels and halving the rate doubles the per-frame budget, together about 8x the bits per pixel. Resolved ONCE at pump start and never as a mid-session rung, because every rung flip pays a BLOCKING encoder open (0.65-0.87 s measured on Iris Xe) plus a fresh IDR - which is why priority_res_cap is off by default. A pair with NO memory never engages it: an unknown link is not a slow one, and guessing soft would degrade the first session on every healthy relay. false = open at the normal size. Env: ROOMLERD_SLOW_LINK_PROFILE. Restart required.",
    ),
    (
        "slow_link_profile_bps",
        "number",
        "Remembered rate at or below which the FR-59 P5 slow-link profile engages, bps. Empty = built-in 1000000; 0 = never engage. Env: ROOMLERD_SLOW_LINK_PROFILE_BPS. Restart required.",
    ),
    (
        "area_min_bitrate",
        "tribool",
        "Area-scaled AIMD bitrate floor (2026-08-26). Default ON: the flat 1.5 Mbps floor was a 1080p legibility tuning and is unreadable mush at 5+ MPix; the scaled floor is ~3.1 Mbps at 2880x1800, capped 4 Mbps, unconstrained sessions only (a relay's 3 Mbps clamp keeps the flat floor so the MD keeps room). false = flat 1.5 Mbps floor. Env: ROOMLERD_AREA_MIN_BITRATE. Restart required.",
    ),
    (
        "priority_res_cap",
        "tribool",
        "rc.445 - restore the pre-rc.445 Priority-dial resolution caps (Smoother 1024 everywhere / Balanced 1280 on relay). Default OFF: every mid-motion rung flip costs a blocking encoder open (0.65-0.87s measured on Iris Xe) and the field verdict was that never flipping beats the rung; the dial's bit-shedding moved to the ceiling factors. Env: ROOMLERD_PRIORITY_RES_CAP. Restart required.",
    ),
    (
        "smoother_rate_pct",
        "string",
        "rc.445 - Smoother's bitrate-ceiling factor (percent, 30-100): a lower ceiling makes the HRD raise QP during motion continuously (smaller frames, steadier fps) with ZERO encoder rebuilds; at-rest quality untouched. Empty = built-in 70. Env: ROOMLERD_SMOOTHER_RATE_PCT. Restart required.",
    ),
    (
        "balanced_rate_pct",
        "string",
        "rc.445 - Balanced's bitrate-ceiling factor (percent, 30-100). Empty = built-in 85. Env: ROOMLERD_BALANCED_RATE_PCT. Restart required.",
    ),
    (
        "scale_threads",
        "string",
        "HW-downscale Phase A - worker threads (1-8) for the CPU resampler's row-banded passes; a lever for weak hosts where the Smoother rung's downscale eats the frame budget. Empty = built-in 1 (inline, no threads). Env: ROOMLERD_SCALE_THREADS. Restart required.",
    ),
    (
        "ice_follow_renomination",
        "enum:auto|always|never",
        "Media-ICE nomination-follow policy. auto (empty) = upward-only + stale-failover (recommended); always = legacy follow-everything (thrash-prone, diagnostics only); never = pin to first nomination. Env: ROOMLER_ICE_FOLLOW_RENOMINATION.",
    ),
    (
        "ice_warm_standby",
        "tribool",
        "Keepalive pings on validated-but-unselected media ICE pairs (keeps the real-path fallback's NAT mapping alive). Built-in default: on. Env: ROOMLER_ICE_WARM_STANDBY.",
    ),
    (
        "ice_overlay_host_deprioritize",
        "tribool",
        "Rank overlay-TUN host candidates below srflx in media ICE (media prefers the real path). Built-in default: on. Env: ROOMLER_ICE_OVERLAY_HOST_DEPRIORITIZE.",
    ),
    (
        "overlay_tier_detect",
        "tribool",
        "Clamp media bitrate when the overlay carrier under a nominated pair is relay-tier. Built-in default: on. Env: ROOMLERD_OVERLAY_TIER_DETECT.",
    ),
    (
        "overlay_rtt_q",
        "tribool",
        "B1 - feed the 15 s overlay RTT probes into the PathMonitor quality plane (Q-only, never eligibility). Built-in default: on. Env: ROOMLERD_OVERLAY_RTT_Q.",
    ),
    (
        "overlay_upward_probe",
        "tribool",
        "B3 - probe an eligible higher tier from a healthy srflx/public incumbent every >=120 s (MBB; incumbent held until latch). Built-in default: on. Env: ROOMLERD_OVERLAY_UPWARD_PROBE.",
    ),
    (
        "relay_probe",
        "tribool",
        "Multi-region relay PoPs - probe the server-pushed region list (timed STUN per PoP) and report RTTs; the server derives this node's relay_home from them. Built-in default: on. Env: ROOMLERD_RELAY_PROBE.",
    ),
    (
        "text_mod_neutralize",
        "tribool",
        "KeyText typing: temporarily release physically-held Shift/Ctrl/Alt the remote layout does not want around each character tap (fixes wrong/dead symbols on non-US layouts). Built-in default: on. Env: ROOMLERD_TEXT_MOD_NEUTRALIZE. Restart required.",
    ),
    (
        "forward_acl",
        "json",
        "Agent-side allowlist for tunnel forwards (JSON: {\"enabled\": bool, \"allowlist\": [...]}).",
    ),
    (
        "virtual_desktop_apps",
        "json",
        "Remote app launcher config for virtual-desktop hosts (JSON; browser only ever sends an allowlist key).",
    ),
];

/// The full editable surface with current values from `cfg`.
pub fn entries(cfg: &AgentConfig) -> Vec<ConfigEntry> {
    KEYS.iter()
        .map(|(key, kind, description)| ConfigEntry {
            key: (*key).to_string(),
            value: current_value(cfg, key),
            kind: (*kind).to_string(),
            restart_required: true,
            description: (*description).to_string(),
        })
        .collect()
}

/// One entry by key (post-apply echo). `None` = unknown key.
pub fn entry_for(cfg: &AgentConfig, key: &str) -> Option<ConfigEntry> {
    KEYS.iter()
        .find(|(k, _, _)| *k == key)
        .map(|(k, kind, description)| ConfigEntry {
            key: (*k).to_string(),
            value: current_value(cfg, k),
            kind: (*kind).to_string(),
            restart_required: true,
            description: (*description).to_string(),
        })
}

fn current_value(cfg: &AgentConfig, key: &str) -> Option<String> {
    match key {
        "overlay_enabled" => Some(fmt_bool(cfg.overlay_enabled)),
        "overlay_multi_org" => Some(fmt_bool(cfg.overlay_multi_org)),
        "overlay_advertised_routes" => Some(cfg.overlay_advertised_routes.join(",")),
        "overlay_exit_node_enabled" => Some(fmt_bool(cfg.overlay_exit_node_enabled)),
        "overlay_exit_node" => cfg.overlay_exit_node.clone(),
        "advertise_routes" => Some(cfg.advertise_routes.join(",")),
        "advertise_local_subnets" => Some(fmt_bool(cfg.advertise_local_subnets)),
        "auto_grant_session" => Some(fmt_bool(cfg.auto_grant_session)),
        "enable_remote_browse" => Some(fmt_bool(cfg.enable_remote_browse)),
        "exec_enabled" => Some(fmt_bool(cfg.exec_enabled)),
        "macos_supervise_gui_worker" => Some(fmt_bool(cfg.macos_supervise_gui_worker)),
        "power_policy" => Some(if cfg.power_policy.is_empty() {
            "never".to_string()
        } else {
            cfg.power_policy.clone()
        }),
        "remote_config_enabled" => Some(fmt_bool(cfg.remote_config_enabled)),
        "ssh_enabled" => Some(fmt_bool(cfg.ssh_enabled)),
        "ssh_port" => cfg.ssh_port.map(|p| p.to_string()),
        "ssh_authorized_keys" => Some(cfg.ssh_authorized_keys.join(",")),
        "ssh_account_mode" => cfg.ssh_account_mode.clone(),
        "ssh_max_privilege" => cfg.ssh_max_privilege.clone(),
        "ssh_activity_log" => Some(fmt_bool(cfg.ssh_activity_log)),
        "encoder_preference" => Some(
            match cfg.encoder_preference {
                EncoderPreferenceChoice::Auto => "auto",
                EncoderPreferenceChoice::Hardware => "hardware",
                EncoderPreferenceChoice::Software => "software",
            }
            .to_string(),
        ),
        "update_check_interval_h" => cfg.update_check_interval_h.map(|h| h.to_string()),
        "overlay_quic" => cfg.overlay_quic.map(fmt_bool),
        "overlay_direct" => cfg.overlay_direct.map(fmt_bool),
        "overlay_derp" => cfg.overlay_derp.map(fmt_bool),
        "overlay_server_relay_strategy" => cfg.overlay_server_relay_strategy.map(fmt_bool),
        "overlay_derp_floor" => cfg.overlay_derp_floor.map(fmt_bool),
        "overlay_org_relay" => cfg.overlay_org_relay.map(fmt_bool),
        "overlay_netcheck" => cfg.overlay_netcheck.map(fmt_bool),
        "relay_server_enabled" => cfg.relay_server_enabled.map(fmt_bool),
        "relay_server_port" => cfg.relay_server_port.map(|v| v.to_string()),
        "tunnel_derp_fallback" => cfg.tunnel_derp_fallback.map(fmt_bool),
        "tunnel_peers_survive_reattach" => cfg.tunnel_peers_survive_reattach.map(fmt_bool),
        "overlay_mbb" => cfg.overlay_mbb.map(fmt_bool),
        "overlay_lan_iface_filter" => cfg.overlay_lan_iface_filter.map(fmt_bool),
        "overlay_wsl_mirrored_guard" => cfg.overlay_wsl_mirrored_guard.map(fmt_bool),
        "overlay_init_auth_first" => cfg.overlay_init_auth_first.map(fmt_bool),
        "overlay_srflx_seek" => cfg.overlay_srflx_seek.map(fmt_bool),
        "ws_replaced_exit" => cfg.ws_replaced_exit.map(fmt_bool),
        "overlay_warm_relay" => cfg.overlay_warm_relay.map(fmt_bool),
        "overlay_quic_async" => cfg.overlay_quic_async.map(fmt_bool),
        "overlay_vpn_vantage" => cfg.overlay_vpn_vantage.map(fmt_bool),
        "overlay_netd" => cfg.overlay_netd.map(fmt_bool),
        "overlay_pathmon" => cfg.overlay_pathmon.clone(),
        "overlay_demote" => cfg.overlay_demote.clone(),
        "overlay_rpf" => cfg.overlay_rpf.clone(),
        "overlay_route_events" => cfg.overlay_route_events.map(fmt_bool),
        "overlay_route_tick_secs" => cfg.overlay_route_tick_secs.map(|v| v.to_string()),
        "overlay_netmon" => cfg.overlay_netmon.map(fmt_bool),
        "overlay_netmon_debounce_ms" => cfg.overlay_netmon_debounce_ms.map(|v| v.to_string()),
        "netstack_socks_port" => cfg.netstack_socks_port.map(|v| v.to_string()),
        "rc_max_sessions" => cfg.rc_max_sessions.map(|v| v.to_string()),
        "overlay_direct_port" => cfg.overlay_direct_port.map(|v| v.to_string()),
        "overlay_iface_metric" => cfg.overlay_iface_metric.map(|v| v.to_string()),
        "shared_encoder" => cfg.shared_encoder.map(fmt_bool),
        "overlay_relay_tls" => cfg.overlay_relay_tls.map(fmt_bool),
        "overlay_shared_carrier" => cfg.overlay_shared_carrier.map(fmt_bool),
        "overlay_roam" => cfg.overlay_roam.map(fmt_bool),
        "overlay_plane_watchdog" => cfg.overlay_plane_watchdog.map(fmt_bool),
        "overlay_session_trace" => cfg.overlay_session_trace.map(fmt_bool),
        "overlay_disco_respond" => cfg.overlay_disco_respond.map(fmt_bool),
        "overlay_disco_probe" => cfg.overlay_disco_probe.map(fmt_bool),
        "overlay_answer_while_followed" => cfg.overlay_answer_while_followed.map(fmt_bool),
        "overlay_tun_stable_guid" => cfg.overlay_tun_stable_guid.map(fmt_bool),
        "overlay_route_evict" => cfg.overlay_route_evict.map(fmt_bool),
        "overlay_route_reclaim" => cfg.overlay_route_reclaim.map(fmt_bool),
        "overlay_tun_persist" => cfg.overlay_tun_persist.map(fmt_bool),
        "overlay_route_metric0" => cfg.overlay_route_metric0.map(fmt_bool),
        "overlay_route_win" => cfg.overlay_route_win.map(fmt_bool),
        "local_turn" => cfg.local_turn.map(fmt_bool),
        "dns_aaaa" => cfg.dns_aaaa.map(fmt_bool),
        "magicdns_hosts" => cfg.magicdns_hosts.map(fmt_bool),
        "auto_update" => cfg.auto_update.map(fmt_bool),
        "logs_upload_disabled" => cfg.logs_upload_disabled.map(fmt_bool),
        "rate_factor_h264" => cfg.rate_factor_h264.map(|p| p.to_string()),
        "rate_factor_hevc" => cfg.rate_factor_hevc.map(|p| p.to_string()),
        "rate_factor_vp9" => cfg.rate_factor_vp9.map(|p| p.to_string()),
        "rate_factor_av1" => cfg.rate_factor_av1.map(|p| p.to_string()),
        "lanczos_min_pct" => cfg.lanczos_min_pct.map(|p| p.to_string()),
        "nvenc_spatial_aq" => cfg.nvenc_spatial_aq.map(fmt_bool),
        "scale_cq_boost" => cfg.scale_cq_boost.map(|p| p.to_string()),
        "idle_refine" => cfg.idle_refine.map(fmt_bool),
        "idle_refine_balanced" => cfg.idle_refine_balanced.map(fmt_bool),
        "gpu_scale" => cfg.gpu_scale.map(fmt_bool),
        "overlay_lan_capture_probe" => cfg.overlay_lan_capture_probe.map(fmt_bool),
        "relay_ceiling_learn" => cfg.relay_ceiling_learn.map(fmt_bool),
        "drm_capture" => cfg.drm_capture.map(fmt_bool),
        "uinput" => cfg.uinput.map(fmt_bool),
        "portal_capture" => cfg.portal_capture.map(fmt_bool),
        "portal_input" => cfg.portal_input.map(fmt_bool),
        "mutter_capture" => cfg.mutter_capture.map(fmt_bool),
        "window_capture" => cfg.window_capture.map(fmt_bool),
        "x11_damage" => cfg.x11_damage.map(fmt_bool),
        "overlay_key_rotation" => cfg.overlay_key_rotation.map(fmt_bool),
        "idle_refine_max_edge" => cfg.idle_refine_max_edge.map(|p| p.to_string()),
        "relay_max_hi_kbps" => cfg.relay_max_hi_kbps.map(|p| p.to_string()),
        "idle_refine_min_frame_kb" => cfg.idle_refine_min_frame_kb.map(|p| p.to_string()),
        "idle_refine_major_area_permille" => {
            cfg.idle_refine_major_area_permille.map(|p| p.to_string())
        }
        "idle_refine_settle_ms" => cfg.idle_refine_settle_ms.map(|p| p.to_string()),
        "idle_refine_settle_constrained_ms" => {
            cfg.idle_refine_settle_constrained_ms.map(|p| p.to_string())
        }
        "constrained_cq_relief" => cfg.constrained_cq_relief.map(|p| p.to_string()),
        "constrained_queue_ms" => cfg.constrained_queue_ms.map(|p| p.to_string()),
        "constrained_hrd_pct" => cfg.constrained_hrd_pct.map(|p| p.to_string()),
        "direct_queue_ms" => cfg.direct_queue_ms.map(|p| p.to_string()),
        "direct_hrd_pct" => cfg.direct_hrd_pct.map(|p| p.to_string()),
        "area_min_bitrate" => cfg.area_min_bitrate.map(fmt_bool),
        "measured_ceiling" => cfg.measured_ceiling.map(fmt_bool),
        "encoder_inplace_rate" => cfg.encoder_inplace_rate.map(fmt_bool),
        "ice_relay_tcp" => cfg.ice_relay_tcp.map(fmt_bool),
        "relay_max_kbps" => cfg.relay_max_kbps.map(|p| p.to_string()),
        "rate_slow_start" => cfg.rate_slow_start.map(fmt_bool),
        "rate_prior_decay" => cfg.rate_prior_decay.map(fmt_bool),
        "transit_classify" => cfg.transit_classify.map(fmt_bool),
        "transit_hold" => cfg.transit_hold.map(fmt_bool),
        "media_thread" => cfg.media_thread.map(fmt_bool),
        "pump_stall_watch" => cfg.pump_stall_watch.map(fmt_bool),
        "pump_stall_warn_ms" => cfg.pump_stall_warn_ms.map(|p| p.to_string()),
        "bg_rebuild_constrained" => cfg.bg_rebuild_constrained.map(fmt_bool),
        "slow_link_floor" => cfg.slow_link_floor.map(fmt_bool),
        "slow_link_min_bitrate" => cfg.slow_link_min_bitrate.map(|p| p.to_string()),
        "constrained_queue_measured" => cfg.constrained_queue_measured.map(fmt_bool),
        "seed_contradiction" => cfg.seed_contradiction.map(fmt_bool),
        "viewer_rate_clamp" => cfg.viewer_rate_clamp.map(fmt_bool),
        "queue_drain" => cfg.queue_drain.map(fmt_bool),
        "slow_link_profile" => cfg.slow_link_profile.map(fmt_bool),
        "slow_link_profile_bps" => cfg.slow_link_profile_bps.map(|p| p.to_string()),
        "bg_rebuild" => cfg.bg_rebuild.map(fmt_bool),
        "par_convert" => cfg.par_convert.map(fmt_bool),
        "fps_pace" => cfg.fps_pace.map(fmt_bool),
        "relay_idr_thrift" => cfg.relay_idr_thrift.map(fmt_bool),
        "relay_age_feedback" => cfg.relay_age_feedback.map(fmt_bool),
        "send_stall_ms" => cfg.send_stall_ms.map(|v| v.to_string()),
        "priority_res_cap" => cfg.priority_res_cap.map(fmt_bool),
        "smoother_rate_pct" => cfg.smoother_rate_pct.map(|p| p.to_string()),
        "balanced_rate_pct" => cfg.balanced_rate_pct.map(|p| p.to_string()),
        "scale_threads" => cfg.scale_threads.map(|p| p.to_string()),
        "ice_follow_renomination" => cfg
            .ice_follow_renomination
            .map(|b| if b { "always" } else { "never" }.to_string()),
        "ice_warm_standby" => cfg.ice_warm_standby.map(fmt_bool),
        "ice_overlay_host_deprioritize" => cfg.ice_overlay_host_deprioritize.map(fmt_bool),
        "overlay_tier_detect" => cfg.overlay_tier_detect.map(fmt_bool),
        "overlay_rtt_q" => cfg.overlay_rtt_q.map(fmt_bool),
        "overlay_upward_probe" => cfg.overlay_upward_probe.map(fmt_bool),
        "relay_probe" => cfg.relay_probe.map(fmt_bool),
        "text_mod_neutralize" => cfg.text_mod_neutralize.map(fmt_bool),
        "forward_acl" => serde_json::to_string(&cfg.forward_acl).ok(),
        "virtual_desktop_apps" => serde_json::to_string(&cfg.virtual_desktop_apps).ok(),
        _ => None,
    }
}

/// Parse + validate + write one key into `cfg`. `value: None` clears the
/// key back to its built-in default. Returns a human-readable error (no
/// partial writes — `cfg` is only mutated on the success path of each
/// arm).
pub fn apply(cfg: &mut AgentConfig, key: &str, value: Option<&str>) -> Result<(), String> {
    match key {
        "overlay_enabled" => cfg.overlay_enabled = parse_bool_or(value, false)?,
        "overlay_multi_org" => cfg.overlay_multi_org = parse_bool_or(value, false)?,
        "overlay_advertised_routes" => cfg.overlay_advertised_routes = parse_cidr_list(value)?,
        "overlay_exit_node_enabled" => cfg.overlay_exit_node_enabled = parse_bool_or(value, false)?,
        "overlay_exit_node" => {
            cfg.overlay_exit_node = value
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }
        "advertise_routes" => cfg.advertise_routes = parse_cidr_list(value)?,
        "advertise_local_subnets" => cfg.advertise_local_subnets = parse_bool_or(value, true)?,
        "auto_grant_session" => cfg.auto_grant_session = parse_bool_or(value, true)?,
        "enable_remote_browse" => cfg.enable_remote_browse = parse_bool_or(value, true)?,
        // Clearing the key (`value: None`) resets to OFF, not ON — the
        // fail-safe direction for a gate that grants root.
        "exec_enabled" => cfg.exec_enabled = parse_bool_or(value, false)?,
        "macos_supervise_gui_worker" => {
            cfg.macos_supervise_gui_worker = parse_bool_or(value, false)?
        }
        // FR-55 — validated HERE rather than at read time, so a typo is
        // refused when it is made instead of silently becoming `never` weeks
        // later on a device nobody is looking at.
        "power_policy" => {
            let v = value.unwrap_or("never").trim().to_ascii_lowercase();
            if !matches!(v.as_str(), "never" | "on-ac" | "on_ac" | "ac" | "always") {
                return Err(format!(
                    "power_policy must be one of never | on-ac | always (got {v:?})"
                ));
            }
            cfg.power_policy = v;
        }
        // Same fail-safe direction, and note WHERE this is settable from:
        // locally (this surface — CLI, desktop companion), never from a
        // server push. A future config-push handler must reject this field
        // explicitly; if the server could set it, every other gate here would
        // be one push away from meaningless. See `docs/remote-config.md`.
        "remote_config_enabled" => cfg.remote_config_enabled = parse_bool_or(value, false)?,
        // Same fail-safe direction as `exec_enabled`: clearing the key means
        // OFF. An SSH session is strictly more than a bounded command.
        "ssh_enabled" => cfg.ssh_enabled = parse_bool_or(value, false)?,
        // Clearing it means OFF, like every other reporting/capability switch.
        "ssh_activity_log" => cfg.ssh_activity_log = parse_bool_or(value, false)?,
        "ssh_port" => {
            cfg.ssh_port = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let p: u16 = v
                        .parse()
                        .map_err(|_| format!("ssh_port must be a number (got {v:?})"))?;
                    if p == 0 {
                        return Err("ssh_port must be between 1 and 65535".into());
                    }
                    Some(p)
                }
            }
        }
        // Clearing the key empties the list, i.e. revokes everyone — the
        // fail-safe direction, and the fastest way to shut SSH access off
        // without touching the transport.
        "ssh_authorized_keys" => {
            cfg.ssh_authorized_keys = value
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        }
        // Validated on the way IN rather than at session time: a typo should be
        // a rejected `config set`, not a session that authenticates and then
        // refuses every command for a reason the operator has to go digging in
        // the daemon log to find.
        "ssh_account_mode" => {
            cfg.ssh_account_mode = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let ok = v == "daemon"
                        || v == "console_user"
                        || v.strip_prefix("named:")
                            .is_some_and(|a| !a.trim().is_empty());
                    if !ok {
                        return Err(format!(
                            "ssh_account_mode must be daemon | console_user | named:<account> \
                             (got {v:?})"
                        ));
                    }
                    Some(v.to_string())
                }
            }
        }
        // Only the two values that name a comparable privilege level. `named:`
        // is deliberately NOT accepted: a ceiling has to be an ordering, and
        // "is `named:svc-backup` above or below `console_user`?" has no answer
        // — so it would be a setting whose meaning nobody could state.
        "ssh_max_privilege" => {
            cfg.ssh_max_privilege = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) if v == "daemon" || v == "console_user" => Some(v.to_string()),
                Some(other) => {
                    return Err(format!(
                        "ssh_max_privilege must be daemon | console_user (got {other:?}); \
                         empty means no device-side limit"
                    ));
                }
            }
        }
        "encoder_preference" => {
            cfg.encoder_preference = match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
                None | Some("") | Some("auto") => EncoderPreferenceChoice::Auto,
                Some("hardware") | Some("hw") | Some("mf") => EncoderPreferenceChoice::Hardware,
                Some("software") | Some("sw") | Some("openh264") => {
                    EncoderPreferenceChoice::Software
                }
                Some(other) => {
                    return Err(format!(
                        "encoder_preference must be auto|hardware|software (got {other:?})"
                    ));
                }
            }
        }
        "update_check_interval_h" => {
            cfg.update_check_interval_h = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let h: u32 = v.parse().map_err(|_| {
                        format!("update_check_interval_h must be a number (got {v:?})")
                    })?;
                    if !(1..=8760).contains(&h) {
                        return Err("update_check_interval_h must be between 1 and 8760".into());
                    }
                    Some(h)
                }
            }
        }
        "overlay_quic" => cfg.overlay_quic = parse_tribool(value)?,
        "overlay_direct" => cfg.overlay_direct = parse_tribool(value)?,
        "overlay_derp" => cfg.overlay_derp = parse_tribool(value)?,
        "overlay_server_relay_strategy" => {
            cfg.overlay_server_relay_strategy = parse_tribool(value)?
        }
        "overlay_derp_floor" => cfg.overlay_derp_floor = parse_tribool(value)?,
        "overlay_org_relay" => cfg.overlay_org_relay = parse_tribool(value)?,
        "overlay_netcheck" => cfg.overlay_netcheck = parse_tribool(value)?,
        "relay_server_enabled" => cfg.relay_server_enabled = parse_tribool(value)?,
        "relay_server_port" => {
            cfg.relay_server_port = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let n: u32 = v
                        .parse()
                        .map_err(|_| format!("relay_server_port must be a number (got {v:?})"))?;
                    // No 0-means-ephemeral escape hatch here, deliberately: a
                    // relay peers must be able to FIND is useless on a port the
                    // operator cannot state, and 3478 is the only port E2E-3
                    // measured the target population reaching.
                    if n == 0 || n > 65535 {
                        return Err(format!("relay_server_port must be 1-65535 (got {n})"));
                    }
                    Some(n)
                }
            }
        }
        "tunnel_derp_fallback" => cfg.tunnel_derp_fallback = parse_tribool(value)?,
        "tunnel_peers_survive_reattach" => {
            cfg.tunnel_peers_survive_reattach = parse_tribool(value)?
        }
        "overlay_mbb" => cfg.overlay_mbb = parse_tribool(value)?,
        "overlay_lan_iface_filter" => cfg.overlay_lan_iface_filter = parse_tribool(value)?,
        "overlay_wsl_mirrored_guard" => cfg.overlay_wsl_mirrored_guard = parse_tribool(value)?,
        "overlay_init_auth_first" => cfg.overlay_init_auth_first = parse_tribool(value)?,
        "overlay_srflx_seek" => cfg.overlay_srflx_seek = parse_tribool(value)?,
        "ws_replaced_exit" => cfg.ws_replaced_exit = parse_tribool(value)?,
        "overlay_warm_relay" => cfg.overlay_warm_relay = parse_tribool(value)?,
        "overlay_quic_async" => cfg.overlay_quic_async = parse_tribool(value)?,
        "overlay_vpn_vantage" => cfg.overlay_vpn_vantage = parse_tribool(value)?,
        "overlay_netd" => cfg.overlay_netd = parse_tribool(value)?,
        "overlay_pathmon" => {
            cfg.overlay_pathmon = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let mode = v.to_ascii_lowercase();
                    if !matches!(mode.as_str(), "on" | "shadow" | "off") {
                        return Err("overlay_pathmon must be on | shadow | off".to_string());
                    }
                    Some(mode)
                }
            }
        }
        "overlay_demote" => {
            cfg.overlay_demote = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let mode = v.to_ascii_lowercase();
                    if !matches!(mode.as_str(), "on" | "shadow" | "off") {
                        return Err("overlay_demote must be on | shadow | off".to_string());
                    }
                    Some(mode)
                }
            }
        }
        "overlay_rpf" => {
            cfg.overlay_rpf = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let mode = v.to_ascii_lowercase();
                    if !matches!(mode.as_str(), "warn" | "enforce" | "off") {
                        return Err("overlay_rpf must be warn | enforce | off".to_string());
                    }
                    Some(mode)
                }
            }
        }
        "overlay_route_events" => cfg.overlay_route_events = parse_tribool(value)?,
        "overlay_route_tick_secs" => {
            cfg.overlay_route_tick_secs = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let s: u32 = v.parse().map_err(|_| {
                        format!("overlay_route_tick_secs must be a number (got {v:?})")
                    })?;
                    if !(2..=300).contains(&s) {
                        return Err("overlay_route_tick_secs must be between 2 and 300".into());
                    }
                    Some(s)
                }
            }
        }
        "overlay_netmon" => cfg.overlay_netmon = parse_tribool(value)?,
        "overlay_netmon_debounce_ms" => {
            cfg.overlay_netmon_debounce_ms = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let ms: u32 = v.parse().map_err(|_| {
                        format!("overlay_netmon_debounce_ms must be a number (got {v:?})")
                    })?;
                    if !(100..=5000).contains(&ms) {
                        return Err(
                            "overlay_netmon_debounce_ms must be between 100 and 5000".into()
                        );
                    }
                    Some(ms)
                }
            }
        }
        "netstack_socks_port" => {
            cfg.netstack_socks_port = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let n: u16 = v.parse().map_err(|_| {
                        format!("netstack_socks_port must be a port number (got {v:?})")
                    })?;
                    if n < 1024 {
                        return Err("netstack_socks_port must be >= 1024 \
                                    (it binds a loopback listener)"
                            .into());
                    }
                    Some(n)
                }
            }
        }
        "rc_max_sessions" => {
            cfg.rc_max_sessions = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let n: u32 = v
                        .parse()
                        .map_err(|_| format!("rc_max_sessions must be a number (got {v:?})"))?;
                    if !(1..=8).contains(&n) {
                        return Err("rc_max_sessions must be between 1 and 8".into());
                    }
                    Some(n)
                }
            }
        }
        "overlay_iface_metric" => {
            cfg.overlay_iface_metric = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let n: u32 = v.parse().map_err(|_| {
                        format!("overlay_iface_metric must be a number (got {v:?})")
                    })?;
                    if n > 9999 {
                        return Err(format!("overlay_iface_metric must be 0..=9999 (got {n})"));
                    }
                    Some(n)
                }
            }
        }
        "overlay_direct_port" => {
            cfg.overlay_direct_port = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => None,
                Some(v) => {
                    let n: u32 = v
                        .parse()
                        .map_err(|_| format!("overlay_direct_port must be a number (got {v:?})"))?;
                    // 0 = ephemeral (explicit opt-out); otherwise a base whose
                    // public-dial band must still fit under the port space.

                    let max = MAX_OVERLAY_DIRECT_PORT_BASE;
                    if n > max {
                        return Err(format!(
                            "overlay_direct_port must be 0 (ephemeral) or 1-{max} \
                             (the public-dial band must fit under 65535)"
                        ));
                    }
                    Some(n)
                }
            }
        }
        "shared_encoder" => cfg.shared_encoder = parse_tribool(value)?,
        "overlay_relay_tls" => cfg.overlay_relay_tls = parse_tribool(value)?,
        "overlay_shared_carrier" => cfg.overlay_shared_carrier = parse_tribool(value)?,
        "overlay_roam" => cfg.overlay_roam = parse_tribool(value)?,
        "overlay_plane_watchdog" => cfg.overlay_plane_watchdog = parse_tribool(value)?,
        "overlay_session_trace" => cfg.overlay_session_trace = parse_tribool(value)?,
        "overlay_disco_respond" => cfg.overlay_disco_respond = parse_tribool(value)?,
        "overlay_disco_probe" => cfg.overlay_disco_probe = parse_tribool(value)?,
        "overlay_answer_while_followed" => {
            cfg.overlay_answer_while_followed = parse_tribool(value)?
        }
        "overlay_tun_stable_guid" => cfg.overlay_tun_stable_guid = parse_tribool(value)?,
        "overlay_route_evict" => cfg.overlay_route_evict = parse_tribool(value)?,
        "overlay_route_reclaim" => cfg.overlay_route_reclaim = parse_tribool(value)?,
        "overlay_tun_persist" => cfg.overlay_tun_persist = parse_tribool(value)?,
        "overlay_route_metric0" => cfg.overlay_route_metric0 = parse_tribool(value)?,
        "overlay_route_win" => cfg.overlay_route_win = parse_tribool(value)?,
        "local_turn" => cfg.local_turn = parse_tribool(value)?,
        "dns_aaaa" => cfg.dns_aaaa = parse_tribool(value)?,
        "magicdns_hosts" => cfg.magicdns_hosts = parse_tribool(value)?,
        "auto_update" => cfg.auto_update = parse_tribool(value)?,
        "logs_upload_disabled" => cfg.logs_upload_disabled = parse_tribool(value)?,
        "rate_factor_h264" => cfg.rate_factor_h264 = parse_rate_factor(key, value)?,
        "rate_factor_hevc" => cfg.rate_factor_hevc = parse_rate_factor(key, value)?,
        "rate_factor_vp9" => cfg.rate_factor_vp9 = parse_rate_factor(key, value)?,
        "rate_factor_av1" => cfg.rate_factor_av1 = parse_rate_factor(key, value)?,
        "lanczos_min_pct" => cfg.lanczos_min_pct = parse_u32_range(key, value, 0, 100)?,
        "nvenc_spatial_aq" => cfg.nvenc_spatial_aq = parse_tribool(value)?,
        "scale_cq_boost" => cfg.scale_cq_boost = parse_u32_range(key, value, 0, 12)?,
        "idle_refine" => cfg.idle_refine = parse_tribool(value)?,
        "idle_refine_balanced" => cfg.idle_refine_balanced = parse_tribool(value)?,
        "gpu_scale" => cfg.gpu_scale = parse_tribool(value)?,
        "overlay_lan_capture_probe" => cfg.overlay_lan_capture_probe = parse_tribool(value)?,
        "relay_ceiling_learn" => cfg.relay_ceiling_learn = parse_tribool(value)?,
        "drm_capture" => cfg.drm_capture = parse_tribool(value)?,
        "uinput" => cfg.uinput = parse_tribool(value)?,
        "portal_capture" => cfg.portal_capture = parse_tribool(value)?,
        "portal_input" => cfg.portal_input = parse_tribool(value)?,
        "mutter_capture" => cfg.mutter_capture = parse_tribool(value)?,
        "window_capture" => cfg.window_capture = parse_tribool(value)?,
        "x11_damage" => cfg.x11_damage = parse_tribool(value)?,
        "overlay_key_rotation" => cfg.overlay_key_rotation = parse_tribool(value)?,
        "idle_refine_max_edge" => cfg.idle_refine_max_edge = parse_u32_range(key, value, 0, 8192)?,
        "relay_max_hi_kbps" => cfg.relay_max_hi_kbps = parse_u32_range(key, value, 0, 100_000)?,
        "idle_refine_min_frame_kb" => {
            cfg.idle_refine_min_frame_kb = parse_u32_range(key, value, 0, 256)?
        }
        "idle_refine_major_area_permille" => {
            cfg.idle_refine_major_area_permille = parse_u32_range(key, value, 0, 1000)?
        }
        "idle_refine_settle_ms" => {
            cfg.idle_refine_settle_ms = parse_u32_range(key, value, 100, 5000)?
        }
        "idle_refine_settle_constrained_ms" => {
            cfg.idle_refine_settle_constrained_ms = parse_u32_range(key, value, 100, 10000)?
        }
        "constrained_cq_relief" => cfg.constrained_cq_relief = parse_u32_range(key, value, 0, 12)?,
        "constrained_queue_ms" => cfg.constrained_queue_ms = parse_u32_range(key, value, 0, 2000)?,
        "constrained_hrd_pct" => cfg.constrained_hrd_pct = parse_u32_range(key, value, 25, 200)?,
        "direct_queue_ms" => cfg.direct_queue_ms = parse_u32_range(key, value, 0, 2000)?,
        "direct_hrd_pct" => cfg.direct_hrd_pct = parse_u32_range(key, value, 25, 200)?,
        "area_min_bitrate" => cfg.area_min_bitrate = parse_tribool(value)?,
        "measured_ceiling" => cfg.measured_ceiling = parse_tribool(value)?,
        "encoder_inplace_rate" => cfg.encoder_inplace_rate = parse_tribool(value)?,
        "ice_relay_tcp" => cfg.ice_relay_tcp = parse_tribool(value)?,
        "relay_max_kbps" => cfg.relay_max_kbps = parse_u32_range(key, value, 100, 100_000)?,
        "rate_slow_start" => cfg.rate_slow_start = parse_tribool(value)?,
        "rate_prior_decay" => cfg.rate_prior_decay = parse_tribool(value)?,
        "transit_classify" => cfg.transit_classify = parse_tribool(value)?,
        "transit_hold" => cfg.transit_hold = parse_tribool(value)?,
        "media_thread" => cfg.media_thread = parse_tribool(value)?,
        "pump_stall_watch" => cfg.pump_stall_watch = parse_tribool(value)?,
        "pump_stall_warn_ms" => cfg.pump_stall_warn_ms = parse_u32_range(key, value, 10, 5000)?,
        "bg_rebuild_constrained" => cfg.bg_rebuild_constrained = parse_tribool(value)?,
        "slow_link_floor" => cfg.slow_link_floor = parse_tribool(value)?,
        "slow_link_min_bitrate" => {
            cfg.slow_link_min_bitrate = parse_u32_range(key, value, 50_000, 1_500_000)?
        }
        "constrained_queue_measured" => cfg.constrained_queue_measured = parse_tribool(value)?,
        "seed_contradiction" => cfg.seed_contradiction = parse_tribool(value)?,
        "viewer_rate_clamp" => cfg.viewer_rate_clamp = parse_tribool(value)?,
        "queue_drain" => cfg.queue_drain = parse_tribool(value)?,
        "slow_link_profile" => cfg.slow_link_profile = parse_tribool(value)?,
        "slow_link_profile_bps" => {
            cfg.slow_link_profile_bps = parse_u32_range(key, value, 0, 100_000_000)?
        }
        "bg_rebuild" => cfg.bg_rebuild = parse_tribool(value)?,
        "par_convert" => cfg.par_convert = parse_tribool(value)?,
        "fps_pace" => cfg.fps_pace = parse_tribool(value)?,
        "relay_idr_thrift" => cfg.relay_idr_thrift = parse_tribool(value)?,
        "relay_age_feedback" => cfg.relay_age_feedback = parse_tribool(value)?,
        "send_stall_ms" => cfg.send_stall_ms = parse_u32_range(key, value, 0, 10_000)?,
        "priority_res_cap" => cfg.priority_res_cap = parse_tribool(value)?,
        "smoother_rate_pct" => cfg.smoother_rate_pct = parse_u32_range(key, value, 30, 100)?,
        "balanced_rate_pct" => cfg.balanced_rate_pct = parse_u32_range(key, value, 30, 100)?,
        "scale_threads" => cfg.scale_threads = parse_u32_range(key, value, 1, 8)?,
        "ice_follow_renomination" => {
            cfg.ice_follow_renomination =
                match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
                    None | Some("") | Some("auto") => None,
                    Some("always") => Some(true),
                    Some("never") => Some(false),
                    Some(other) => {
                        return Err(format!(
                            "ice_follow_renomination must be auto|always|never (got {other:?})"
                        ));
                    }
                }
        }
        "ice_warm_standby" => cfg.ice_warm_standby = parse_tribool(value)?,
        "ice_overlay_host_deprioritize" => {
            cfg.ice_overlay_host_deprioritize = parse_tribool(value)?
        }
        "overlay_tier_detect" => cfg.overlay_tier_detect = parse_tribool(value)?,
        "overlay_rtt_q" => cfg.overlay_rtt_q = parse_tribool(value)?,
        "overlay_upward_probe" => cfg.overlay_upward_probe = parse_tribool(value)?,
        "relay_probe" => cfg.relay_probe = parse_tribool(value)?,
        "text_mod_neutralize" => cfg.text_mod_neutralize = parse_tribool(value)?,
        "forward_acl" => {
            cfg.forward_acl = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => Default::default(),
                Some(v) => serde_json::from_str(v)
                    .map_err(|e| format!("forward_acl: invalid JSON: {e}"))?,
            }
        }
        "virtual_desktop_apps" => {
            cfg.virtual_desktop_apps = match value.map(str::trim).filter(|s| !s.is_empty()) {
                None => Default::default(),
                Some(v) => serde_json::from_str(v)
                    .map_err(|e| format!("virtual_desktop_apps: invalid JSON: {e}"))?,
            }
        }
        other => return Err(format!("unknown or non-editable config key {other:?}")),
    }
    Ok(())
}

/// P7 — shared bounded-u32 parse for the plain numeric keys: empty clears
/// (built-in applies), numeric must be within `lo..=hi`.
fn parse_u32_range(
    key: &str,
    value: Option<&str>,
    lo: u32,
    hi: u32,
) -> Result<Option<u32>, String> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => {
            let n: u32 = v
                .parse()
                .map_err(|_| format!("{key} must be a number (got {v:?})"))?;
            if !(lo..=hi).contains(&n) {
                return Err(format!("{key} must be between {lo} and {hi}"));
            }
            Ok(Some(n))
        }
    }
}

/// Shared parse/validate for the four `rate_factor_*` keys: empty clears
/// (built-in applies), numeric must be 50–400 %.
fn parse_rate_factor(key: &str, value: Option<&str>) -> Result<Option<u32>, String> {
    match value.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => {
            let pct: u32 = v
                .parse()
                .map_err(|_| format!("{key} must be a number (got {v:?})"))?;
            if !(50..=400).contains(&pct) {
                return Err(format!("{key} must be between 50 and 400 (percent)"));
            }
            Ok(Some(pct))
        }
    }
}

fn fmt_bool(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

/// `"true"/"1"/"yes"/"on"` → true; `"false"/"0"/"no"/"off"` → false
/// (case-insensitive). Anything else is an error.
fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(format!("expected true/false (got {other:?})")),
    }
}

/// Plain-bool keys: `None`/empty clears back to the serde default.
fn parse_bool_or(v: Option<&str>, default: bool) -> Result<bool, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(default),
        Some(v) => parse_bool(v),
    }
}

/// Tri-state keys: `None`/empty = unset (built-in default applies).
fn parse_tribool(v: Option<&str>) -> Result<Option<bool>, String> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(v) => parse_bool(v).map(Some),
    }
}

/// S2 wizard — validate a comma-separated CIDR list WITHOUT a config
/// (the setup wizard checks route inputs before the MSI runs and the
/// single-use enrollment token is burned). Same parser [`apply`] uses,
/// so a value that validates here can't fail there.
pub fn validate_cidr_list(v: &str) -> Result<(), String> {
    parse_cidr_list(Some(v)).map(|_| ())
}

/// Comma-separated CIDR list; each token must be `addr/prefix` with a
/// prefix that fits the address family. `None`/empty clears the list.
fn parse_cidr_list(v: Option<&str>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for token in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (addr, prefix) = token
            .split_once('/')
            .ok_or_else(|| format!("{token:?} is not CIDR notation (addr/prefix)"))?;
        let ip: std::net::IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("{token:?}: bad IP address"))?;
        let bits: u8 = prefix
            .trim()
            .parse()
            .map_err(|_| format!("{token:?}: bad prefix length"))?;
        let max = if ip.is_ipv4() { 32 } else { 128 };
        if bits > max {
            return Err(format!("{token:?}: prefix /{bits} exceeds /{max}"));
        }
        out.push(token.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_gets_and_applies() {
        let mut cfg = crate::config::test_fixture();
        let all = entries(&cfg);
        assert_eq!(all.len(), KEYS.len());
        // Every listed key roundtrips through entry_for + a no-op apply
        // of its own current value (or a clear for unset optionals).
        for e in &all {
            assert!(e.restart_required);
            assert!(!e.description.is_empty());
            apply(&mut cfg, &e.key, e.value.as_deref()).expect("self-value apply");
            let echoed = entry_for(&cfg, &e.key).expect("known key");
            assert_eq!(echoed.value, e.value, "roundtrip for {}", e.key);
        }
    }

    #[test]
    fn tribool_set_and_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_quic", Some("on")).unwrap();
        assert_eq!(cfg.overlay_quic, Some(true));
        assert_eq!(
            entry_for(&cfg, "overlay_quic").unwrap().value.as_deref(),
            Some("true")
        );
        apply(&mut cfg, "overlay_quic", Some("0")).unwrap();
        assert_eq!(cfg.overlay_quic, Some(false));
        apply(&mut cfg, "overlay_quic", None).unwrap();
        assert_eq!(cfg.overlay_quic, None);
        assert!(apply(&mut cfg, "overlay_quic", Some("maybe")).is_err());
    }

    /// FR-36 — the DRM-capture / uinput pair, set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule; both shipped as env-only first
    /// and this closes that gap).
    ///
    /// ⚠️ The property worth locking is that **unset means OFF for both**, and
    /// that unset is distinguishable from an explicit `false`. Both switch on
    /// behaviour a host must opt into: DRM capture carries no damage
    /// information (turning it on where X11 works costs the FR-29 idle-CPU
    /// win), and a uinput device is host-global, injecting into whatever has
    /// focus — including the greeter and the lock screen.
    #[test]
    fn fr36_drm_capture_and_uinput_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(cfg.drm_capture, None, "unset by default");
        assert_eq!(cfg.uinput, None, "unset by default");

        for key in [
            "drm_capture",
            "uinput",
            "x11_damage",
            "portal_capture",
            "portal_input",
            "mutter_capture",
            "window_capture",
        ] {
            apply(&mut cfg, key, Some("on")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("true"),
                "{key} echoes what was set"
            );
            apply(&mut cfg, key, Some("off")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("false")
            );
            apply(&mut cfg, key, None).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value,
                None,
                "{key} clears back to unset, which is NOT the same as false"
            );
            assert!(apply(&mut cfg, key, Some("sometimes")).is_err());
        }
        assert_eq!(cfg.drm_capture, None);
        assert_eq!(cfg.uinput, None);
    }

    /// The bridge is what makes a config key reach the backend at all — the
    /// gates read `ROOMLERD_*` via `env::flag`, and `main.rs` feeds those
    /// fallbacks from this array. A key that is absent here is editable and
    /// inert, which looks exactly like a broken feature.
    #[test]
    fn fr36_keys_are_wired_into_the_env_bridge() {
        let mut cfg = crate::config::test_fixture();
        cfg.drm_capture = Some(true);
        cfg.uinput = Some(true);
        cfg.x11_damage = Some(false);
        cfg.portal_capture = Some(true);
        cfg.portal_input = Some(false);
        cfg.mutter_capture = Some(true);
        cfg.window_capture = Some(true);
        let bridged = crate::config::env_bridge_bools(&cfg);
        for (name, want) in [
            ("DRM_CAPTURE", Some(true)),
            ("UINPUT", Some(true)),
            ("X11_DAMAGE", Some(false)),
            ("PORTAL_CAPTURE", Some(true)),
            ("PORTAL_INPUT", Some(false)),
            ("MUTTER_CAPTURE", Some(true)),
            ("WINDOW_CAPTURE", Some(true)),
        ] {
            let got = bridged
                .iter()
                .find(|(k, _)| *k == name)
                .unwrap_or_else(|| panic!("{name} missing from env_bridge_bools"));
            assert_eq!(got.1, want, "{name} bridges the configured value");
        }
    }

    /// rc.276 — the forced-TLS-relay probe key set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_relay_tls_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_relay_tls", Some("on")).unwrap();
        assert_eq!(cfg.overlay_relay_tls, Some(true));
        assert_eq!(
            entry_for(&cfg, "overlay_relay_tls")
                .unwrap()
                .value
                .as_deref(),
            Some("true")
        );
        apply(&mut cfg, "overlay_relay_tls", Some("0")).unwrap();
        assert_eq!(cfg.overlay_relay_tls, Some(false));
        apply(&mut cfg, "overlay_relay_tls", None).unwrap();
        assert_eq!(cfg.overlay_relay_tls, None);
        assert!(apply(&mut cfg, "overlay_relay_tls", Some("maybe")).is_err());
    }

    /// Multi-org v2 shared-carrier soak flag set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_shared_carrier_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_shared_carrier", Some("on")).unwrap();
        assert_eq!(cfg.overlay_shared_carrier, Some(true));
        assert_eq!(
            entry_for(&cfg, "overlay_shared_carrier")
                .unwrap()
                .value
                .as_deref(),
            Some("true")
        );
        apply(&mut cfg, "overlay_shared_carrier", Some("0")).unwrap();
        assert_eq!(cfg.overlay_shared_carrier, Some(false));
        apply(&mut cfg, "overlay_shared_carrier", None).unwrap();
        assert_eq!(cfg.overlay_shared_carrier, None);
        assert!(apply(&mut cfg, "overlay_shared_carrier", Some("maybe")).is_err());
    }

    /// A3 — WG-style roaming kill switch set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_roam_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_roam", Some("off")).unwrap();
        assert_eq!(cfg.overlay_roam, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_roam").unwrap().value.as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_roam", Some("1")).unwrap();
        assert_eq!(cfg.overlay_roam, Some(true));
        apply(&mut cfg, "overlay_roam", None).unwrap();
        assert_eq!(cfg.overlay_roam, None);
        assert!(apply(&mut cfg, "overlay_roam", Some("maybe")).is_err());
    }

    /// B4 — plane-watchdog kill switch set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_plane_watchdog_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_plane_watchdog", Some("off")).unwrap();
        assert_eq!(cfg.overlay_plane_watchdog, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_plane_watchdog")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_plane_watchdog", Some("1")).unwrap();
        assert_eq!(cfg.overlay_plane_watchdog, Some(true));
        apply(&mut cfg, "overlay_plane_watchdog", None).unwrap();
        assert_eq!(cfg.overlay_plane_watchdog, None);
        assert!(apply(&mut cfg, "overlay_plane_watchdog", Some("maybe")).is_err());
    }

    /// C1 — the disco responder kill switch set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_disco_probe_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_disco_probe", Some("on")).unwrap();
        assert_eq!(cfg.overlay_disco_probe, Some(true));
        apply(&mut cfg, "overlay_disco_probe", None).unwrap();
        assert_eq!(cfg.overlay_disco_probe, None);
        assert!(apply(&mut cfg, "overlay_disco_probe", Some("maybe")).is_err());
    }

    #[test]
    fn overlay_disco_respond_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_disco_respond", Some("off")).unwrap();
        assert_eq!(cfg.overlay_disco_respond, Some(false));
        apply(&mut cfg, "overlay_disco_respond", None).unwrap();
        assert_eq!(cfg.overlay_disco_respond, None);
        assert!(apply(&mut cfg, "overlay_disco_respond", Some("maybe")).is_err());
    }

    /// Diagnostic session-trace kill switch set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_session_trace_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_session_trace", Some("on")).unwrap();
        assert_eq!(cfg.overlay_session_trace, Some(true));
        apply(&mut cfg, "overlay_session_trace", None).unwrap();
        assert_eq!(cfg.overlay_session_trace, None);
        assert!(apply(&mut cfg, "overlay_session_trace", Some("maybe")).is_err());
    }

    /// The WSL2 mirrored-networking guard key set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule). The kill switch matters here:
    /// the guard SUPPRESSES a guest's whole LAN gather, so a misdetection
    /// must be recoverable with `roomler config set …` and no rebuild.
    #[test]
    fn overlay_wsl_mirrored_guard_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_wsl_mirrored_guard", Some("off")).unwrap();
        assert_eq!(cfg.overlay_wsl_mirrored_guard, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_wsl_mirrored_guard")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_wsl_mirrored_guard", None).unwrap();
        assert_eq!(
            cfg.overlay_wsl_mirrored_guard, None,
            "clear → built-in default"
        );
        // The env bridge must carry it, or `config set` writes TOML the daemon
        // silently ignores — the exact failure the bridge test guards against.
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_WSL_MIRRORED_GUARD"),
            "key must be bridged to the daemon's env"
        );
    }

    /// The srflx SEEKING key set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule). This is the kill switch for
    /// the W5 self-healing NONE-regather; off restores the pre-W5
    /// return-the-sink (sticky NONE) behaviour.
    #[test]
    fn overlay_srflx_seek_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_srflx_seek", Some("off")).unwrap();
        assert_eq!(cfg.overlay_srflx_seek, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_srflx_seek")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_srflx_seek", None).unwrap();
        assert_eq!(cfg.overlay_srflx_seek, None, "clear → built-in default");
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_SRFLX_SEEK"),
            "key must be bridged to the daemon's env"
        );
    }

    /// W4(d) — the legacy ReplacedByNewer process-exit escalation is
    /// opt-in via `ws_replaced_exit`; the built-in default (cleared) is
    /// the in-process backoff ladder.
    #[test]
    fn ws_replaced_exit_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(cfg.ws_replaced_exit, None, "built-in default is OFF");
        apply(&mut cfg, "ws_replaced_exit", Some("on")).unwrap();
        assert_eq!(cfg.ws_replaced_exit, Some(true));
        assert_eq!(
            entry_for(&cfg, "ws_replaced_exit")
                .unwrap()
                .value
                .as_deref(),
            Some("true")
        );
        apply(&mut cfg, "ws_replaced_exit", None).unwrap();
        assert_eq!(cfg.ws_replaced_exit, None, "clear → built-in default");
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "WS_REPLACED_EXIT"),
            "key must be bridged to the daemon's env"
        );
    }

    /// C4 stage 1 — the warm-relay opt-in key: tribool, default OFF,
    /// bridged to the daemon env.
    #[test]
    fn overlay_warm_relay_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(cfg.overlay_warm_relay, None, "built-in default is OFF");
        apply(&mut cfg, "overlay_warm_relay", Some("on")).unwrap();
        assert_eq!(cfg.overlay_warm_relay, Some(true));
        apply(&mut cfg, "overlay_warm_relay", None).unwrap();
        assert_eq!(cfg.overlay_warm_relay, None, "clear → built-in default");
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_WARM_RELAY"),
            "key must be bridged to the daemon's env"
        );
    }

    /// R2 — the public-dial srflx fallback key: tribool, default None
    /// (daemon built-in ON), bridged to the daemon env.
    #[test]
    fn overlay_vpn_vantage_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(cfg.overlay_vpn_vantage, None, "unset → daemon default (on)");
        apply(&mut cfg, "overlay_vpn_vantage", Some("off")).unwrap();
        assert_eq!(cfg.overlay_vpn_vantage, Some(false));
        apply(&mut cfg, "overlay_vpn_vantage", None).unwrap();
        assert_eq!(cfg.overlay_vpn_vantage, None, "clear → built-in default");
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_VPN_VANTAGE"),
            "key must be bridged to the daemon's env"
        );
    }

    /// W6 phase 3 — the raw-first QUIC upgrade kill switch: tribool,
    /// default ON, bridged to the daemon env.
    #[test]
    fn overlay_quic_async_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(
            cfg.overlay_quic_async, None,
            "built-in default is ON via env fallback"
        );
        apply(&mut cfg, "overlay_quic_async", Some("off")).unwrap();
        assert_eq!(cfg.overlay_quic_async, Some(false));
        apply(&mut cfg, "overlay_quic_async", None).unwrap();
        assert_eq!(cfg.overlay_quic_async, None, "clear → built-in default");
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_QUIC_ASYNC"),
            "key must be bridged to the daemon's env"
        );
    }

    /// Track A stage 1 — the netd scaffold key: tribool, default OFF,
    /// supervisor-scoped (deliberately NOT env-bridged — the worker's
    /// runtime never reads it).
    #[test]
    fn overlay_netd_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(cfg.overlay_netd, None, "built-in default is OFF");
        apply(&mut cfg, "overlay_netd", Some("on")).unwrap();
        assert_eq!(cfg.overlay_netd, Some(true));
        apply(&mut cfg, "overlay_netd", None).unwrap();
        assert_eq!(cfg.overlay_netd, None, "clear → built-in default");
    }

    /// The auth-first type-1 routing key set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule). This is the kill switch for
    /// the dual-org lockout fix — it must be flippable per host with
    /// `roomler config set …` and no rebuild.
    #[test]
    fn overlay_init_auth_first_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_init_auth_first", Some("off")).unwrap();
        assert_eq!(cfg.overlay_init_auth_first, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_init_auth_first")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_init_auth_first", None).unwrap();
        assert_eq!(
            cfg.overlay_init_auth_first, None,
            "clear → built-in default"
        );
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .any(|(k, _)| *k == "OVERLAY_INIT_AUTH_FIRST"),
            "key must be bridged to the daemon's env"
        );
    }

    /// rc.275 — the LAN-gather virtual-interface filter key set/echo/clear
    /// (per the every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_lan_iface_filter_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_lan_iface_filter", Some("off")).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_lan_iface_filter")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_lan_iface_filter", Some("1")).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, Some(true));
        apply(&mut cfg, "overlay_lan_iface_filter", None).unwrap();
        assert_eq!(cfg.overlay_lan_iface_filter, None);
        assert!(apply(&mut cfg, "overlay_lan_iface_filter", Some("maybe")).is_err());
    }

    /// rc.279 — the stable-Wintun-identity key set/echo/clear (per the
    /// every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_tun_stable_guid_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_tun_stable_guid", Some("off")).unwrap();
        assert_eq!(cfg.overlay_tun_stable_guid, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_tun_stable_guid")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_tun_stable_guid", Some("1")).unwrap();
        assert_eq!(cfg.overlay_tun_stable_guid, Some(true));
        apply(&mut cfg, "overlay_tun_stable_guid", None).unwrap();
        assert_eq!(cfg.overlay_tun_stable_guid, None);
        assert!(apply(&mut cfg, "overlay_tun_stable_guid", Some("maybe")).is_err());
    }

    /// rc.279 — the route-war eviction kill-switch key set/echo/clear (per
    /// the every-new-env-gets-a-config-key rule).
    #[test]
    fn overlay_route_evict_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_route_evict", Some("off")).unwrap();
        assert_eq!(cfg.overlay_route_evict, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_route_evict")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_route_evict", Some("1")).unwrap();
        assert_eq!(cfg.overlay_route_evict, Some(true));
        apply(&mut cfg, "overlay_route_evict", None).unwrap();
        assert_eq!(cfg.overlay_route_evict, None);
        assert!(apply(&mut cfg, "overlay_route_evict", Some("maybe")).is_err());
    }

    /// rc.280 — parity lock between the editable surface and the config→env
    /// bridge (the S2 fallback map). Asserts (1) every bridged suffix maps
    /// back to an editable surface key of the right kind (no orphan bridge
    /// entries), and (2) a surface write is visible to the bridge — the mass
    /// set/echo that replaces per-key boilerplate tests for bridged keys.
    /// The inverse (every tribool key must be bridged) is deliberately NOT
    /// asserted: some tribool keys are read straight from config by agent
    /// code rather than via `node_env`.
    #[test]
    fn env_bridge_pairs_have_surface_parity() {
        let mut cfg = crate::config::test_fixture();
        let bool_suffixes: Vec<&'static str> = crate::config::env_bridge_bools(&cfg)
            .iter()
            .map(|(s, _)| *s)
            .collect();
        for suffix in &bool_suffixes {
            let key = suffix.to_ascii_lowercase();
            let entry = entry_for(&cfg, &key)
                .unwrap_or_else(|| panic!("bridged suffix {suffix} has no surface key '{key}'"));
            assert_eq!(
                entry.kind, "tribool",
                "bridged bool '{key}' must be a tribool key"
            );
            apply(&mut cfg, &key, Some("1")).unwrap_or_else(|e| panic!("set {key}: {e}"));
        }
        assert!(
            crate::config::env_bridge_bools(&cfg)
                .iter()
                .all(|(_, v)| *v == Some(true)),
            "every bridged bool must reflect the surface write"
        );
        for (suffix, _) in crate::config::env_bridge_numerics(&cfg) {
            let key = suffix.to_ascii_lowercase();
            let entry = entry_for(&cfg, &key)
                .unwrap_or_else(|| panic!("bridged suffix {suffix} has no surface key '{key}'"));
            // Historically "string" (the rate_factor keys predate the
            // "number" editor kind); newer numerics use "number" like
            // overlay_route_tick_secs. Either is a validated numeric edit.
            assert!(
                matches!(entry.kind.as_str(), "string" | "number"),
                "bridged numeric '{key}' must be a validated string/number key, got {:?}",
                entry.kind
            );
        }
    }

    /// PR-D — the PathMonitor mode key: multi-state (on|shadow|off), so a
    /// validated string, not a tribool (per the every-new-env rule's
    /// enum-not-tribool clause). Case-normalized; empty/None clears.
    #[test]
    fn overlay_pathmon_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_pathmon", Some("shadow")).unwrap();
        assert_eq!(cfg.overlay_pathmon.as_deref(), Some("shadow"));
        assert_eq!(
            entry_for(&cfg, "overlay_pathmon").unwrap().value.as_deref(),
            Some("shadow")
        );
        apply(&mut cfg, "overlay_pathmon", Some("ON")).unwrap();
        assert_eq!(cfg.overlay_pathmon.as_deref(), Some("on"));
        apply(&mut cfg, "overlay_pathmon", None).unwrap();
        assert_eq!(cfg.overlay_pathmon, None);
        assert!(apply(&mut cfg, "overlay_pathmon", Some("sideways")).is_err());
    }

    /// B2 — the demotion-mode key set/echo/clear + validation.
    #[test]
    fn overlay_rpf_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_rpf", Some("enforce")).unwrap();
        assert_eq!(cfg.overlay_rpf.as_deref(), Some("enforce"));
        assert_eq!(
            entry_for(&cfg, "overlay_rpf").unwrap().value.as_deref(),
            Some("enforce")
        );
        apply(&mut cfg, "overlay_rpf", Some("WARN")).unwrap();
        assert_eq!(cfg.overlay_rpf.as_deref(), Some("warn"));
        apply(&mut cfg, "overlay_rpf", Some("off")).unwrap();
        assert_eq!(cfg.overlay_rpf.as_deref(), Some("off"));
        apply(&mut cfg, "overlay_rpf", None).unwrap();
        assert_eq!(cfg.overlay_rpf, None);
        // `shadow` is B2's vocabulary, not this key's — reject it rather than
        // silently landing on the default.
        assert!(apply(&mut cfg, "overlay_rpf", Some("shadow")).is_err());
        assert!(apply(&mut cfg, "overlay_rpf", Some("yes")).is_err());
    }

    #[test]
    fn overlay_demote_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_demote", Some("on")).unwrap();
        assert_eq!(cfg.overlay_demote.as_deref(), Some("on"));
        assert_eq!(
            entry_for(&cfg, "overlay_demote").unwrap().value.as_deref(),
            Some("on")
        );
        apply(&mut cfg, "overlay_demote", Some("SHADOW")).unwrap();
        assert_eq!(cfg.overlay_demote.as_deref(), Some("shadow"));
        apply(&mut cfg, "overlay_demote", None).unwrap();
        assert_eq!(cfg.overlay_demote, None);
        assert!(apply(&mut cfg, "overlay_demote", Some("maybe")).is_err());
    }

    /// P4/PR-D — the event-driven route-guard key set/echo/clear.
    #[test]
    fn overlay_route_tick_secs_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_route_tick_secs", Some("120")).unwrap();
        assert_eq!(cfg.overlay_route_tick_secs, Some(120));
        assert_eq!(
            entry_for(&cfg, "overlay_route_tick_secs")
                .unwrap()
                .value
                .as_deref(),
            Some("120")
        );
        apply(&mut cfg, "overlay_route_tick_secs", None).unwrap();
        assert_eq!(cfg.overlay_route_tick_secs, None);
        assert!(apply(&mut cfg, "overlay_route_tick_secs", Some("1")).is_err());
        assert!(apply(&mut cfg, "overlay_route_tick_secs", Some("301")).is_err());
        assert!(apply(&mut cfg, "overlay_route_tick_secs", Some("fast")).is_err());
    }

    #[test]
    fn overlay_direct_port_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_direct_port", Some("43648")).unwrap();
        assert_eq!(cfg.overlay_direct_port, Some(43648));
        assert_eq!(
            entry_for(&cfg, "overlay_direct_port")
                .unwrap()
                .value
                .as_deref(),
            Some("43648")
        );
        // 0 = explicit ephemeral opt-out; stored, not rejected.
        apply(&mut cfg, "overlay_direct_port", Some("0")).unwrap();
        assert_eq!(cfg.overlay_direct_port, Some(0));
        apply(&mut cfg, "overlay_direct_port", None).unwrap();
        assert_eq!(cfg.overlay_direct_port, None);
        // A base whose public-dial band would overflow the port space is
        // rejected (mirrors `direct::MAX_DIRECT_PORT_BASE`); garbage too.
        let max = MAX_OVERLAY_DIRECT_PORT_BASE;
        apply(&mut cfg, "overlay_direct_port", Some(&max.to_string())).unwrap();
        assert_eq!(cfg.overlay_direct_port, Some(max));
        assert!(
            apply(
                &mut cfg,
                "overlay_direct_port",
                Some(&(max + 1).to_string())
            )
            .is_err()
        );
        assert!(apply(&mut cfg, "overlay_direct_port", Some("65535")).is_err());
        assert!(apply(&mut cfg, "overlay_direct_port", Some("forty")).is_err());
    }

    #[test]
    fn rc_max_sessions_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "rc_max_sessions", Some("3")).unwrap();
        assert_eq!(cfg.rc_max_sessions, Some(3));
        assert_eq!(
            entry_for(&cfg, "rc_max_sessions").unwrap().value.as_deref(),
            Some("3")
        );
        apply(&mut cfg, "rc_max_sessions", None).unwrap();
        assert_eq!(cfg.rc_max_sessions, None);
        assert!(apply(&mut cfg, "rc_max_sessions", Some("0")).is_err());
        assert!(apply(&mut cfg, "rc_max_sessions", Some("9")).is_err());
        assert!(apply(&mut cfg, "rc_max_sessions", Some("many")).is_err());
    }

    #[test]
    fn shared_encoder_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "shared_encoder", Some("off")).unwrap();
        assert_eq!(cfg.shared_encoder, Some(false));
        assert_eq!(
            entry_for(&cfg, "shared_encoder").unwrap().value.as_deref(),
            Some("false")
        );
        apply(&mut cfg, "shared_encoder", Some("on")).unwrap();
        assert_eq!(cfg.shared_encoder, Some(true));
        apply(&mut cfg, "shared_encoder", None).unwrap();
        assert_eq!(cfg.shared_encoder, None);
        assert!(apply(&mut cfg, "shared_encoder", Some("maybe")).is_err());
    }

    #[test]
    fn overlay_route_events_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "overlay_route_events", Some("off")).unwrap();
        assert_eq!(cfg.overlay_route_events, Some(false));
        assert_eq!(
            entry_for(&cfg, "overlay_route_events")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_route_events", None).unwrap();
        assert_eq!(cfg.overlay_route_events, None);
        assert!(apply(&mut cfg, "overlay_route_events", Some("maybe")).is_err());
    }

    #[test]
    fn rate_factor_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        for key in [
            "rate_factor_h264",
            "rate_factor_hevc",
            "rate_factor_vp9",
            "rate_factor_av1",
        ] {
            apply(&mut cfg, key, Some("120")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("120"),
                "set/echo for {key}"
            );
            assert!(apply(&mut cfg, key, Some("49")).is_err(), "{key} below 50");
            assert!(
                apply(&mut cfg, key, Some("401")).is_err(),
                "{key} above 400"
            );
            assert!(apply(&mut cfg, key, Some("fast")).is_err(), "{key} garbage");
            // Failed applies never partially wrote:
            assert_eq!(entry_for(&cfg, key).unwrap().value.as_deref(), Some("120"));
            apply(&mut cfg, key, None).unwrap();
            assert_eq!(entry_for(&cfg, key).unwrap().value, None, "clear {key}");
        }
    }

    #[test]
    fn ice_follow_renomination_enum_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "ice_follow_renomination", Some("always")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, Some(true));
        assert_eq!(
            entry_for(&cfg, "ice_follow_renomination")
                .unwrap()
                .value
                .as_deref(),
            Some("always")
        );
        apply(&mut cfg, "ice_follow_renomination", Some("never")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, Some(false));
        apply(&mut cfg, "ice_follow_renomination", Some("auto")).unwrap();
        assert_eq!(cfg.ice_follow_renomination, None);
        assert_eq!(
            entry_for(&cfg, "ice_follow_renomination").unwrap().value,
            None
        );
        assert!(apply(&mut cfg, "ice_follow_renomination", Some("on")).is_err());
    }

    #[test]
    fn ice_and_overlay_hatch_tribools_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        for key in [
            "ice_warm_standby",
            "ice_overlay_host_deprioritize",
            "overlay_tier_detect",
            "overlay_rtt_q",
            "overlay_upward_probe",
            "relay_probe",
            "text_mod_neutralize",
        ] {
            apply(&mut cfg, key, Some("off")).unwrap();
            assert_eq!(
                entry_for(&cfg, key).unwrap().value.as_deref(),
                Some("false"),
                "set/echo for {key}"
            );
            apply(&mut cfg, key, None).unwrap();
            assert_eq!(entry_for(&cfg, key).unwrap().value, None, "clear {key}");
        }
    }

    /// Fleet RPC gate 4 — set/echo/clear, and above all: clearing must land
    /// on OFF. This is the one refusal that survives a compromised control
    /// plane, so a `roomler config set exec_enabled` with no value must never
    /// be a way to turn it on.
    #[test]
    fn exec_enabled_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(
            current_value(&cfg, "exec_enabled").as_deref(),
            Some("false"),
            "Fleet RPC must be off on a fresh device"
        );
        apply(&mut cfg, "exec_enabled", Some("true")).unwrap();
        assert!(cfg.exec_enabled);
        assert_eq!(
            entry_for(&cfg, "exec_enabled").unwrap().value.as_deref(),
            Some("true")
        );
        apply(&mut cfg, "exec_enabled", None).unwrap();
        assert!(!cfg.exec_enabled, "clearing must fail SAFE, not open");
        assert!(apply(&mut cfg, "exec_enabled", Some("perhaps")).is_err());
    }

    /// The opt-in that keeps `exec_enabled` / `ssh_enabled` refusable by a
    /// compromised control plane (`docs/remote-config.md`). Locked here
    /// because the DEFAULT is the security property: a device that has not
    /// opted in cannot be opened by any server, so a stray `#[serde(default)]`
    /// change or a "sensible" default-on would silently undo the design.
    #[test]
    fn remote_config_opt_in_defaults_off_and_clears_off() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(
            current_value(&cfg, "remote_config_enabled").as_deref(),
            Some("false"),
            "a fresh device must not accept pushed config"
        );
        apply(&mut cfg, "remote_config_enabled", Some("true")).unwrap();
        assert!(cfg.remote_config_enabled);
        assert_eq!(
            entry_for(&cfg, "remote_config_enabled")
                .unwrap()
                .value
                .as_deref(),
            Some("true")
        );
        // Clearing opts back OUT. The fail-safe direction for this key is the
        // one that RESTORES the local veto.
        apply(&mut cfg, "remote_config_enabled", None).unwrap();
        assert!(
            !cfg.remote_config_enabled,
            "clearing must re-close the door"
        );
        assert!(apply(&mut cfg, "remote_config_enabled", Some("maybe")).is_err());
    }

    /// A config file written before this key existed must load as opted-OUT.
    /// Every device in the field predates it, so the `#[serde(default)]`
    /// behaviour here is what decides whether the fleet wakes up closed.
    #[test]
    fn absent_remote_config_key_loads_as_opted_out() {
        let cfg: crate::config::AgentConfig =
            toml::from_str("server_url = \"https://example.invalid\"\nws_url = \"wss://example.invalid/ws\"\nagent_token = \"t\"\nagent_id = \"a\"\ntenant_id = \"t\"\nmachine_id = \"m\"\nmachine_name = \"n\"\n")
                .expect("minimal config parses");
        assert!(
            !cfg.remote_config_enabled,
            "a config predating the key must not be opted in"
        );
    }

    #[test]
    fn bool_clear_restores_serde_default() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "auto_grant_session", Some("false")).unwrap();
        assert!(!cfg.auto_grant_session);
        apply(&mut cfg, "auto_grant_session", None).unwrap();
        assert!(cfg.auto_grant_session, "default_auto_grant_session is true");
        apply(&mut cfg, "overlay_enabled", None).unwrap();
        assert!(!cfg.overlay_enabled, "overlay default is off");
    }

    /// P2c — the multi-org TUN opt-in round-trips through set/echo and
    /// clears back to its off default.
    #[test]
    fn overlay_multi_org_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(
            current_value(&cfg, "overlay_multi_org").as_deref(),
            Some("false")
        );
        apply(&mut cfg, "overlay_multi_org", Some("true")).unwrap();
        assert!(cfg.overlay_multi_org);
        assert_eq!(
            current_value(&cfg, "overlay_multi_org").as_deref(),
            Some("true")
        );
        apply(&mut cfg, "overlay_multi_org", None).unwrap();
        assert!(!cfg.overlay_multi_org, "multi-org TUN default is off");
    }

    #[test]
    fn cidr_list_validates() {
        let mut cfg = crate::config::test_fixture();
        apply(
            &mut cfg,
            "advertise_routes",
            Some("192.168.1.0/24, 10.0.0.0/8"),
        )
        .unwrap();
        assert_eq!(cfg.advertise_routes, vec!["192.168.1.0/24", "10.0.0.0/8"]);
        assert!(apply(&mut cfg, "advertise_routes", Some("not-a-cidr")).is_err());
        assert!(apply(&mut cfg, "advertise_routes", Some("10.0.0.0/40")).is_err());
        // Bad input never partially wrote:
        assert_eq!(cfg.advertise_routes, vec!["192.168.1.0/24", "10.0.0.0/8"]);
        apply(&mut cfg, "advertise_routes", Some("")).unwrap();
        assert!(cfg.advertise_routes.is_empty());
    }

    #[test]
    fn json_keys_validate() {
        let mut cfg = crate::config::test_fixture();
        let acl = r#"{"enabled": false, "allowlist": []}"#;
        apply(&mut cfg, "forward_acl", Some(acl)).unwrap();
        assert!(!cfg.forward_acl.enabled);
        assert!(apply(&mut cfg, "forward_acl", Some("{nope")).is_err());
        apply(&mut cfg, "forward_acl", None).unwrap();
        assert!(cfg.forward_acl.enabled, "default ACL is enabled");
    }

    #[test]
    fn secrets_and_identity_are_not_exposed() {
        let cfg = crate::config::test_fixture();
        for hidden in [
            "agent_token",
            "overlay_wg_secret_key",
            "machine_id",
            "machine_name",
            "tunnel_routes",
        ] {
            assert!(entry_for(&cfg, hidden).is_none(), "{hidden} must be hidden");
            let mut c = cfg.clone();
            assert!(apply(&mut c, hidden, Some("x")).is_err());
        }
    }

    #[test]
    fn enum_and_interval_parse() {
        let mut cfg = crate::config::test_fixture();
        apply(&mut cfg, "encoder_preference", Some("hw")).unwrap();
        assert!(matches!(
            cfg.encoder_preference,
            EncoderPreferenceChoice::Hardware
        ));
        assert!(apply(&mut cfg, "encoder_preference", Some("fast")).is_err());
        apply(&mut cfg, "update_check_interval_h", Some("168")).unwrap();
        assert_eq!(cfg.update_check_interval_h, Some(168));
        assert!(apply(&mut cfg, "update_check_interval_h", Some("0")).is_err());
        assert!(apply(&mut cfg, "update_check_interval_h", Some("nope")).is_err());
        apply(&mut cfg, "update_check_interval_h", None).unwrap();
        assert_eq!(cfg.update_check_interval_h, None);
    }
}

#[cfg(test)]
mod netstack_port_surface_tests {
    use super::{apply, current_value};

    /// Multi-org netstack: the per-org port is a real config key, so it has
    /// to round-trip through the surface like every other one.
    #[test]
    fn netstack_socks_port_set_echo_clear() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(current_value(&cfg, "netstack_socks_port"), None);

        apply(&mut cfg, "netstack_socks_port", Some("41080")).unwrap();
        assert_eq!(cfg.netstack_socks_port, Some(41080));
        assert_eq!(
            current_value(&cfg, "netstack_socks_port").as_deref(),
            Some("41080")
        );

        // Privileged ports would need the daemon to bind below 1024 for a
        // loopback convenience listener — refuse rather than fail at bind.
        assert!(apply(&mut cfg, "netstack_socks_port", Some("80")).is_err());
        assert!(apply(&mut cfg, "netstack_socks_port", Some("nope")).is_err());
        assert_eq!(
            cfg.netstack_socks_port,
            Some(41080),
            "a rejected set is a no-op"
        );

        apply(&mut cfg, "netstack_socks_port", None).unwrap();
        assert_eq!(
            cfg.netstack_socks_port, None,
            "clearing returns to OS-TUN mode"
        );
    }

    /// The M5 privilege ceiling. Two values only — a ceiling has to be an
    /// ordering, and `named:<account>` has no place in one — and the reject
    /// happens on the way IN, so a typo is a failed `config set` rather than
    /// a device that silently kept accepting root sessions.
    #[test]
    fn ssh_max_privilege_set_echo_clear_and_validate() {
        let mut cfg = crate::config::test_fixture();
        assert_eq!(
            current_value(&cfg, "ssh_max_privilege"),
            None,
            "unset by default: arriving switched on would revoke a working config mid-roll"
        );

        apply(&mut cfg, "ssh_max_privilege", Some("console_user")).unwrap();
        assert_eq!(cfg.ssh_max_privilege.as_deref(), Some("console_user"));
        assert_eq!(
            current_value(&cfg, "ssh_max_privilege").as_deref(),
            Some("console_user")
        );

        apply(&mut cfg, "ssh_max_privilege", Some("daemon")).unwrap();
        assert_eq!(
            cfg.ssh_max_privilege.as_deref(),
            Some("daemon"),
            "explicit `daemon` is a stated no-limit, distinct from never having chosen"
        );

        // `named:` is accepted for ssh_account_mode and refused here on
        // purpose: "is named:svc-backup above or below console_user?" has no
        // answer, so it cannot bound anything.
        assert!(apply(&mut cfg, "ssh_max_privilege", Some("named:svc")).is_err());
        assert!(apply(&mut cfg, "ssh_max_privilege", Some("Console_User")).is_err());
        assert_eq!(
            cfg.ssh_max_privilege.as_deref(),
            Some("daemon"),
            "a rejected set is a no-op"
        );

        apply(&mut cfg, "ssh_max_privilege", None).unwrap();
        assert_eq!(cfg.ssh_max_privilege, None, "clearing returns to no limit");
    }
}
