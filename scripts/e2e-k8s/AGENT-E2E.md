# agent-e2e harness — bring-up runbook

Phase 1 of the agent + tunnel test-pipeline expansion. Adds a Linux
agent Pod to the existing `roomler-ai-e2e` overlay so Phase 2's
browser-driven remote-control specs can pick an enrolled agent
without operator setup.

## What ships in this chunk (1B)

- **`Dockerfile.agent-e2e`** — multi-stage build of `roomler-agent`
  with `--features synthetic-frame-source,openh264-encoder,clipboard`.
  Runtime is debian:trixie-slim + curl + ca-certificates + libssl3 +
  libgcc-s1 (~80 MiB final image).
- **`scripts/e2e-k8s/overlay-template/`**:
  - `job-agent-e2e-seed.yaml` — one-shot Job creating the e2e admin
    user + tenant. `ttlSecondsAfterFinished: 0` so re-apply replaces.
  - `secret-agent-e2e-bootstrap.yaml` — admin creds + tenant_id
    consumed by the StatefulSet. **Ships with a placeholder
    `tenant_id: 000…0` that MUST be rebaked after the first seed.**
  - `service-agent-e2e.yaml` — headless Service for predictable Pod
    DNS (`agent-e2e-0.agent-e2e.roomler-ai-e2e.svc.cluster.local`).
  - `statefulset-agent-e2e.yaml` — 2-replica StatefulSet of the
    synthetic-frame agents.
  - `kustomization.yaml` — extended to include the 4 new files plus
    the new image rewrite (`gjovanov/roomler-agent-e2e` →
    `<internal-registry>/roomler-agent-e2e:latest`).

## Bring-up steps (run from the build host)

### 1. Build + push the agent-e2e image

```bash
: "${BUILD_HOST:=<your-build-host>}"
: "${REGISTRY:=<internal-registry>}"
: "${REPO:=$HOME/roomler-ai}"

ssh "$BUILD_HOST"
cd "$REPO" && git pull
docker build -f Dockerfile.agent-e2e -t "$REGISTRY/roomler-agent-e2e:latest" .
docker push "$REGISTRY/roomler-agent-e2e:latest"
```

Image size sanity-check: ~80–100 MiB. If it's >200 MiB something
pulled in scrap-capture transitively — re-check the
`--no-default-features` + feature list in `Dockerfile.agent-e2e`.

### 2. Apply the overlay (placeholder tenant_id phase)

```bash
: "${DEPLOY_REPO:=$HOME/roomler-ai-deploy}"
# Ensure the overlay template is in the deploy repo (idempotent):
bash "$REPO/scripts/e2e-k8s/install-overlay.sh"
kubectl apply -k "$DEPLOY_REPO/k8s/overlays/e2e/"
```

The agent Pods will come up but **loop on `tenant not found`** —
the placeholder `tenant_id: 000…0` in the Secret doesn't match the
real tenant the seed Job creates. That's expected for this first
apply.

### 3. Read the live tenant_id from the seed Job logs

```bash
kubectl -n roomler-ai-e2e logs job/agent-e2e-seed | grep 'tenant_id='
# → [seed] tenant_id=<24-hex-chars>
```

### 4. Rebake the Secret + re-apply

Edit `secret-agent-e2e-bootstrap.yaml` in the overlay-template dir,
paste the real `tenant_id`, then:

```bash
bash "$REPO/scripts/e2e-k8s/install-overlay.sh"  # re-copies into deploy repo
kubectl apply -k "$DEPLOY_REPO/k8s/overlays/e2e/"
kubectl -n roomler-ai-e2e rollout restart statefulset/agent-e2e
```

### 5. Verify the agents are online

```bash
# Wait for both Pods to be Ready:
kubectl -n roomler-ai-e2e wait --for=condition=Ready --timeout=120s \
  pod -l app=agent-e2e

# Check the agent records exist in the API:
TOKEN=$(curl -fsS -X POST http://roomler2.roomler-ai-e2e/api/auth/login \
  -H 'content-type: application/json' \
  -d '{"email":"agent-e2e-admin@roomler.local","password":"agent-e2e-bootstrap-pw-2026"}' \
  | jq -r .access_token)
curl -fsS http://roomler2.roomler-ai-e2e/api/tenant/<tenant_id>/agent \
  -H "authorization: Bearer $TOKEN" | jq '.[] | {name, machine_name, is_online}'
# → two entries, both is_online: true
```

## What does NOT ship in this chunk

- **Chunk 1C — Rust integration test driver** (`crates/tests/agent_e2e/`)
  that drives `rc:session.request` from a controller-role WS client
  and asserts ICE-connected + ≥1 SCTP exchange. Comes next iteration.
- **`scripts/e2e-k8s.sh` extension** that orchestrates the agent
  bring-up automatically (so an operator can run a single command to
  spin up API + agents + Playwright). Comes with Chunk 1C.
- **Phase 2** (browser-driven remote smoke), **Phase 3** (file-DC),
  **Phase 5** (tunnel) — all depend on Chunk 1C being done.

## Known gotchas

- **Pod DNS name vs `machine_name`**: the agent's `rc:agent.hello`
  payload includes `machine_name` = `$HOSTNAME` (the Pod's hostname,
  e.g. `agent-e2e-0`). Tests resolve a Pod's current `agent_id` via
  `GET /api/tenant/<tid>/agent?name=agent-e2e-0` because `agent_id`
  is regenerated on every `enroll` call (each Pod restart mints a
  new id; deterministic `machine_id` derivation would carry across
  restarts but isn't wired here — Pods are ephemeral).
- **Resource limits**: 500m CPU per agent Pod is enough for synthetic
  frames at 15 fps + openh264 encode. If you bump fps in the
  synthetic backend, raise the CPU limit accordingly.
- **No input feature**: `--features enigo-input` isn't compiled in,
  so any test that drives mouse/keyboard against the agent will get
  silently dropped. Phase 2 covers input via an alternative image
  (Xvfb sidecar) or a Windows agent.
