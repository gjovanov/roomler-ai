# FR-64 — Remote desktop never rides the overlay

**Issue:** [#1244](https://github.com/gjovanov/roomler-ai/issues/1244) · **Status:** open, AC1-AC5 read (AC2's p50 owes a read from pages loaded after 2026-11-08 15:56Z, when #3566's fix went live), two things owed (below) · **Plan:** `immutable-doodling-neumann` (approved 2026-09-02)

**Parent/siblings:** FR-59 (the regression that motivated the arc). This is one of three FRs from that plan (FR-62 encoder apply path, FR-63 the controller, FR-64 the data path).

**Where it stands, 2026-11-03, re-checked 2026-11-08.** C2 shipped as #1246. C1 changed shape: the `overlay_ice_candidates` switch never shipped, #1882 made the choice the device's (`AccessPolicy.rc_overlay`, default off, `docs/remote-control.md` §4.4), and FR-128 added `auto`. Two things keep this open:

- **#3359.** A session requested while the daemon starts keeps an empty filter for its whole life. In the week read, 4 of the 15 such sessions on mesh devices gathered their own overlay address. None nominated a path on it.
- **`auto` with the far TURN dead (FR-128, #3357).** With no UDP to coturn, `auto` lets a session ride the mesh over DERP. On a corp laptop on its VPN, TURN over TLS can still be alive, so `auto` can take this FR's exact path where `off` would take TURN. The lab cell for it was read on 2026-11-03 ([#3357](https://github.com/gjovanov/roomler-ai/issues/3357#issuecomment-6593946635)): with UDP blocked and TURN over TLS alive, under motion, `auto` rode DERP in 8 sessions of 9, held `off`'s fps and rate, and cost about 20 ms of transit, and this FR's 20–400 kbps did not reproduce. A real corporate link and the relay allowance's throttle were not reached. The default flip (#3366) is still a draft and #3357 waits on the operator, so this FR's "never" is not retired yet.

## Goal

A remote-desktop session never has its ICE pair nominated on an **overlay-adapter address**. On 2026-09-02 a corp laptop's session nominated `udp4 host 100.65.0.5 ↔ 100.65.0.6`, so the video rode WebRTC → overlay TUN → WireGuard → DERP over TLS → the corporate VPN, shed by our own DERP mux (100 ms queue-age bound), at 20–400 kbps with seconds of paint age — while the same host's native TURN pair would have carried it. The rc.319 interface filter never worked on Windows (webrtc-util reports every adapter with name `""`) or macOS (`utunN`), srflx and remote candidates are never filtered, and host↔host type-preference wins whenever the overlay pair connects. Decision (operator, 2026-09-02, after the trade-off analysis in the spec): **never for RC** — WebRTC is already end-to-end encrypted, the overlay layer buys RC nothing but a hostile floor.

Same root, second symptom: **#1237** — on multi-org hosts the two org runtimes evict each other's derived-ULA `/96` route every guard wave ("any interface other than ours"), a ~100/min forced-rekey storm on every peer; and the same missing notion of "own adapters" silently withholds the primary's block floor.

## Key design

- **C1** (kill switch `overlay_ice_candidates`, ON = today): own-adapter address `ip_filter` at gather (`SettingEngine::set_ip_filter`, addresses on our own adapters, never a CIDR — an ISP CGNAT address on the physical NIC is kept); the srflx mapped address filtered in the already-forked webrtc-ice (`webrtc-ice.patch`) plus a belt at the signaling hop; remote non-relay candidates equal to a LocalAPI `peers()` overlay IP or our own dropped in `add_remote_candidate` (relay-typed exempt, so the loopback TURN stays). PathClass-lite in the ICE-path log line and in the FR-35 memory key (`"{ip}|{class}"`); an additive `carrier` field on the LocalAPI peer entry.
- **C2** (#1237, first): an own-adapter registry in tunnel-core (`OWN_TUNS` registered in `SystemTun::up_with`, freed in `Drop`), exempted by every eviction helper and by `non_overlay_v4_addrs`; nobody asserts the whole `/96` — `defend_self_route` defends the connected block's derived prefix at `v6_onlink_plen` (byte-identical for single-org); a WARN when two orgs' blocks overlap. Switches `overlay_sibling_exempt`, `overlay_v6_defend_narrow`.

## Acceptance criteria

- [x] AC1 — CORPLAP-1 (multi-org, VPN on): `ICE: gathered local candidate` shows no `100.6x.`/`fd72:` host or srflx; `per-session ICE path detected` shows a relay/srflx pair outside the overlay ranges; no `overlay carrier under the nominated pair is not direct` line. *7 days to 2026-11-02: 228 candidates and 15 ICE paths, none on an overlay address, 0 of those lines. The session of 2026-10-30 14:29Z ran with `stun_udp=false` read 136 minutes before and 2 minutes after, on a `Relay/Relay` pair. That is taken as VPN on: the logs don't name the VPN. The same read on NEO16 finds 214 overlay candidates and 29 overlay pairs, so it can see them.*
- [~] AC2 — CORPLAP-3 on the LAN: the native `192.168.0.x` pair, p50 ≤ 50 ms. *7 days to 2026-11-02: 52 of 54 ICE paths have both ends on `192.168.*`, age p50 11 ms, p90 31 ms, over 6 468 windows. Held at partial 2026-11-08: the pair half holds, and the p50 is the viewer's paint age, which #3566 found off by a page↔worker clock drift that grows with how long the page was open before the session (near 160 ms after an hour, low on some hosts). This read can't tell which sessions started right after their page loaded, so the p50 half waits on a new read. That read is possible since #3566's fix, #3568, went live on 2026-11-08 at 15:56Z (`hosted-20261108-4297002`), but only from sessions whose page loaded after 15:56Z: an older tab reconnects in place after a roll and keeps the old arithmetic until it reloads, so the read must cut by time. It is still owed.*
- [x] AC3 — `rc_overlay=on` brings the overlay pair back (was `overlay_ice_candidates = true`, a switch that never shipped). *CORPLAP-2, 7 days to 2026-11-03: 100 sessions log `allowed (device policy rc_overlay=on)`, and 64 of them ran on a path holding the device's own overlay address, carrier `derp` on 63.*
- [x] AC4 — 24 h on neo16 / CORPLAP-1 / CORPLAP-3: sibling evictions **0** (today 718/day), forced-revalidation pokes at idle baseline (today ~100/min), no withheld block floor on the primary. *24 h to 2026-11-02 22:15Z: sibling evictions 0 on all three. Seconds holding a `net-change` line: 1 803, 157 and 827, so at most 1.25 a minute. 7 days to 2026-11-03: 0 `WITHHOLDING block-floor routes` lines on any of the three, with `installing` lines every day. CORPLAP-3 also evicted another product's route 204 times in those 24 h, which is not a sibling. The `DERP floor WITHHELD` lines, 4 to 8 a host, are a different gate, a peer that doesn't advertise the floor (`relay_link.rs:2150`).*
- [x] AC5 — Docs: `docs/remote-control.md` §4.4 holds the rule with its diagram and tables, in `docs/README.md`'s index, and `docs/multi-org.md` and `docs/overlay-nat-traversal.md` state it (the former's "ICE never gathers on roomler-*" was true only on Linux).

## Open decisions

- Proxy-only egress (443 only) loses RC under "never": a `turns:` listener on 443 on coturn, or the loopback-TURN opt-in.
- Whether an ICE restart on a mid-session carrier change is worth building later (not needed under "never").

## Out of scope

- The rate controller (FR-63) and the encoder apply path (FR-62); SSH/tunnels keep the overlay.

## Related

#1237, FR-59 #1163, FR-33 (VPN LAN capture), FR-19 #805 (org relay), FR-35, FR-62, FR-63, #1882, FR-128 #2989, #3357, #3359.

## Field verification log

| Read | Result |
|---|---|
| 2026-11-02 and 2026-11-03, prod `agent_logs`, 7 days, read-only, 0.4.181-0.4.197 | **AC1 to AC4:** in the criteria above. **The filter, every `overlay excluded` session in the week:** 445 had their blocks, 422 of them on mesh devices, and none gathered or used the device's own overlay address. 391 had an empty filter: 376 on devices with no overlay row, where that is correct, and 15 on mesh devices, 4 of which gathered their own overlay address as a host candidate. None nominated a path on it. Those are #3359. |
