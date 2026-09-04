# --- Stage 1: Rust build ---
FROM rust:1.88-bookworm AS builder
RUN apt-get update && apt-get install -y libclang-dev cmake python3-pip && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt
WORKDIR /app
COPY . .
# FR-69 P8 (D9/D13) — which pillars this image carries. `full` | `collab` |
# `remote` | `mesh` | `access`; every profile is the same server composed from
# fewer modules, and `/health` lists the ones it mounts. `SAAS=1` adds the
# hosted service's billing + newsletter module: the default HERE so the
# operator's manual prod build (no build args) keeps it; the self-host publish
# workflow passes `SAAS=0` and asserts the image does not mount it.
ARG PROFILE=full
ARG SAAS=1
# `derp-relay` rides along so the SAME image can run as the central
# coturn workers' `/stats` sidecar (stats follow-up): one image, two
# binaries, no second build+push pipeline. A few MB, and it keeps the
# stats producer byte-identical between the PoPs and the central fleet.
# Package-qualified features are what let one build line serve both
# packages: `derp-relay` has no features, so `--no-default-features` is
# inert for it, and `roomler-ai-api/profile-…` names exactly the one crate
# that composes the modules.
RUN cargo build --release -p roomler-ai-api -p derp-relay --no-default-features \
      --features "roomler-ai-api/profile-${PROFILE}$( [ "$SAAS" = "1" ] && printf ',roomler-ai-api/saas' )"

# --- Stage 2: Vue SPA build ---
FROM oven/bun:1 AS ui-builder
WORKDIR /app/ui
COPY ui/package.json ui/bun.lock ./
RUN bun install --frozen-lockfile
COPY ui/ .
RUN bun run build

# --- Stage 3: Runtime (nginx + Rust binary) ---
FROM debian:trixie-slim AS runtime

# ── OCI image metadata (FR-24) ──────────────────────────────────────────────
# Renders on GHCR / Docker Hub listing pages and is what `docker inspect`
# reports. `licenses` is AGPL-3.0-only because THIS IMAGE is the control
# plane (roomler-ai-api + derp-relay + the Vue bundle) — the MPL-2.0 half of
# the split ships as native agent binaries, never in this image. It must be
# updated in lockstep with LICENSE/LICENSING.md; a stale value here
# advertises the wrong terms to every registry that lists us.
ARG VERSION=dev
ARG GIT_SHA=unknown
LABEL org.opencontainers.image.title="Roomler" \
      org.opencontainers.image.description="Remote desktop in a browser tab + WireGuard mesh VPN. Self-hosted, end-to-end encrypted." \
      org.opencontainers.image.url="https://roomler.ai" \
      org.opencontainers.image.source="https://github.com/gjovanov/roomler-ai" \
      org.opencontainers.image.documentation="https://roomler.ai/docs/" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.vendor="G ROX EOOD" \
      org.opencontainers.image.authors="legal@roomler.ai" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${GIT_SHA}"
RUN apt-get update && apt-get install -y ca-certificates nginx && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/roomler-ai-api /usr/local/bin/
COPY --from=builder /app/target/release/derp-relay /usr/local/bin/
COPY --from=ui-builder /app/ui/dist /var/www/roomler-ai
COPY files/nginx-pod.conf /etc/nginx/conf.d/default.conf
# Operator-supplied GeoIP database for the user analytics. The directory
# always exists (README + .gitignore keep the licensed .mmdb out of git);
# the build host drops the file in before `docker build`. Absent ⇒ the
# analytics honestly report `country: unknown` — see files/geoip/README.
COPY files/geoip/ /usr/share/roomler/geoip/
# FR-20 P5 - unit costs for the metered relay/SFU resources. The binary
# resolves `config/relay-costs.toml` relative to its CWD, which is `/` here.
# Same contract as the GeoIP directory above: absent is a supported state and
# renders "not priced", never a fabricated 0.00 (which would also imply 100%
# margin). `ROOMLER__RELAY_COSTS__*` overrides it without an image rebuild.
COPY config/ /config/
RUN rm -f /etc/nginx/sites-enabled/default
RUN printf '#!/bin/sh\nnginx\nexec roomler-ai-api\n' > /entrypoint.sh && chmod +x /entrypoint.sh
EXPOSE 80
CMD ["/entrypoint.sh"]
