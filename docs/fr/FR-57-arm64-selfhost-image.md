# FR-57: An arm64 self-host image

**Issue:** [#TBD](https://github.com/gjovanov/roomler-ai/issues) ·
Status: **P0 — spec** (2026-09-01) · Follows
[FR-42](FR-42-selfhost-verified-on-a-clean-box.md) (#967), which shipped the
amd64 image and put arm64 explicitly out of scope.

## Goal

`docker compose … pull` should work on a Raspberry Pi, an Apple Silicon Mac and
an ARM VPS — the three machines the self-hosting audience most often has spare.
Today those hosts fall through to a source build, which is the 20-minute path
the published image exists to remove.

## Why now, and not in FR-42

FR-42 ruled arm64 out for a reason that was correct at the time: cross-building
mediasoup and the Rust tree under QEMU takes hours, and **an arm64 image nobody
has run is a worse promise than no arm64 image**. That reason is about QEMU, not
about arm64 — GitHub's `ubuntu-24.04-arm` runners build natively, at roughly
amd64 speed, and can run the same smoke test on the architecture they built for.

The pull request that makes this urgent is external: it is the first thing
r/selfhosted asks, because that room runs Pis and Apple Silicon.

## Key design

**One job per architecture, each on its own native runner, joined by a
manifest.**

```
build (matrix: amd64 on ubuntu-latest, arm64 on ubuntu-24.04-arm)
  → build, smoke-test ON THAT ARCH, push BY DIGEST
merge
  → docker buildx imagetools create  → one multi-arch tag
```

⚠️ **Each arch smoke-tests on its own runner, natively.** An arm64 image checked
under QEMU emulation proves the bytes exist, not that they run — and "it builds"
was never the bar here: FR-42's smoke test exists because an image that starts
and serves nothing is worse than no image, since whoever pulls it did not read
the source.

⚠️ **Push by digest, then assemble.** Two jobs cannot both push the same tag —
the second silently wins and the manifest names one architecture. Digest push +
`imagetools create` is the shape that makes the tag a *list* rather than a race.

⚠️ **The amd64 path must not regress.** It is published, documented and measured
at 88 s. If arm64 fails, amd64 still publishes: the merge step tolerates a
missing arch and says which one is missing, rather than failing the whole run
and leaving the tag unmoved.

### What is genuinely unknown

The Dockerfile is arch-neutral and the runtime image ships only
`roomler-ai-api`, `derp-relay` and the built UI — **no agent media stack**
(`openh264`, `scrap`, `enigo` are agent-only). So the risk is concentrated in
three dependencies that carry architecture-specific build machinery:

- **`mediasoup-sys`** — C++ built through meson/ninja, with a vendored openssl,
  libsrtp, usrsctp and abseil. The most likely place to break.
- **the vendored `webrtc` tree** and its ICE/SRTP crates.
- **`ring`** — assembly per target.

None of these is known to fail on aarch64; none has been tried here. **The first
build is the experiment**, and this FR is written so that a failure is a
recorded result rather than a silent gap.

## Phases

| # | Phase | Kill switch | Status |
|---|-------|-------------|--------|
| P1 | Matrix the publish workflow by arch; smoke-test natively; join by manifest | drop the arm64 matrix entry — amd64 publishes exactly as today | planned |
| P2 | Publish a real multi-arch tag and pull it on an arm64 host | do not move `latest` | planned |
| P3 | Docs stop saying amd64-only | n/a | planned |

## Acceptance criteria

- [ ] `docker manifest inspect ghcr.io/gjovanov/roomler-ai:<tag>` lists **both**
      `linux/amd64` and `linux/arm64`
- [ ] the arm64 image is smoke-tested **on an arm64 runner** — `/health` 200 and
      the SPA served, not merely built
- [ ] an actual arm64 host pulls and runs it (`fedora-arm` is on the fleet)
- [ ] the amd64 timings do not regress from FR-42's measured 88 s
- [ ] `README.md` and `docs/self-hosting.md` no longer say amd64-only
- [ ] a failing arm64 build does not block the amd64 publish, and says so

## Out of scope

- **armv7 / 32-bit.** The Rust tree and mediasoup on 32-bit ARM is a different
  problem, and the audience asking is on 64-bit.
- **The agent binaries.** `release-agent.yml` already builds its own targets;
  this FR is only the server image.
- **Windows containers.**

## Open decisions

- Whether to keep publishing a **per-arch tag** (`:v0.4.43-arm64`) alongside the
  manifest. It helps debugging a specific arch and costs nothing; it also
  invites someone to pin one by accident. Leaning: push by digest only, and let
  the manifest be the single name anyone uses.

## Field-verification log

_(appended as it happens)_
