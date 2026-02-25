# --- Stage 1: Rust build ---
FROM rust:1.88-bookworm AS builder
RUN apt-get update && apt-get install -y libclang-dev cmake python3-pip && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt
WORKDIR /app
COPY . .
RUN cargo build --release --bin roomler2-api

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
COPY --from=builder /app/target/release/roomler2-api /usr/local/bin/
COPY --from=ui-builder /app/ui/dist /var/www/roomler2
COPY files/nginx-pod.conf /etc/nginx/conf.d/default.conf
RUN rm -f /etc/nginx/sites-enabled/default
RUN printf '#!/bin/sh\nnginx\nexec roomler2-api\n' > /entrypoint.sh && chmod +x /entrypoint.sh
EXPOSE 80
CMD ["/entrypoint.sh"]
