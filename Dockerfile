# --- Stage 1: Rust build ---
FROM rust:1.88-bookworm AS builder
RUN apt-get update && apt-get install -y libclang-dev cmake python3-pip && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt
WORKDIR /app
COPY . .
# `derp-relay` rides along so the SAME image can run as the central
# coturn workers' `/stats` sidecar (stats follow-up): one image, two
# binaries, no second build+push pipeline. A few MB, and it keeps the
# stats producer byte-identical between the PoPs and the central fleet.
RUN cargo build --release --bin roomler-ai-api --bin derp-relay

# --- Stage 2: Vue SPA build ---
FROM oven/bun:1 AS ui-builder
WORKDIR /app/ui
COPY ui/package.json ui/bun.lock ./
RUN bun install --frozen-lockfile
COPY ui/ .
RUN bun run build

# --- Stage 3: Runtime (nginx + Rust binary) ---
FROM debian:trixie-slim AS runtime
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
RUN rm -f /etc/nginx/sites-enabled/default
RUN printf '#!/bin/sh\nnginx\nexec roomler-ai-api\n' > /entrypoint.sh && chmod +x /entrypoint.sh
EXPOSE 80
CMD ["/entrypoint.sh"]
