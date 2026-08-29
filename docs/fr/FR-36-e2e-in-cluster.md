# FR-36 — The e2e suite runs outside the cluster, so it cannot test media (and keeps breaking on the seam)

**Issue:** [#928](https://github.com/gjovanov/roomler-ai/issues/928)
**Status:** proposed — diagnosis measured 2026-08-29, no code yet

## Goal

Make the browser lane able to exercise a **call**: two participants, real
mediasoup RTP, screen share. Today every conference spec fails, the call
surface has no automated coverage at all, and the workarounds for running
outside the cluster are themselves a recurring source of breakage.

## Why it fails today — measured, not assumed

The suite runs a Playwright container on the build host and reaches the stack
through `kubectl port-forward`. That forwards **one TCP port**. Media does not
use it.

```
e2e roomler2 pod:  podIP 10.244.3.196   node k8s-worker-3 (10.10.30.11)
                   hostNetwork: <unset>          ← unlike prod, which sets it
mars -> 10.10.30.11 :  reachable, dev wg0 src 10.10.0.1
mars -> 10.244.3.196:  via 94.130.141.65 dev eth0   ← i.e. out the DEFAULT ROUTE.
                                                      No route to the pod network.
```

So the browser cannot send a single RTP packet to the SFU: the pod's address
is on the CNI network, which the build host has no route to. The failure is
**not** the mediasoup RTC range being blocked by a firewall — nothing was ever
going to arrive. ⚠️ I assumed DNAT was needed and it is not; the browser is
simply in the wrong network.

The e2e configmap also sets no `ROOMLER__MEDIASOUP__ANNOUNCED_IP` at all (only
`NUM_WORKERS: 1`), so whatever it announces, no one outside the cluster can
route to it.

### The seam bites in other ways too

- **2026-08-29**: every WebSocket handshake in the suite was refused with 403
  for ~a week, because cookie auth checks `Origin` against `frontend_url` and
  the port-forward origin (`127.0.0.1:18080`) is not the stack's own. Patched
  by pointing `frontend_url` at the forward — a workaround for the seam.
- `scripts/e2e-nightly.sh` carries ~60 lines of **self-healing port-forward
  supervisor** because a bare `kubectl port-forward` both dies and, worse,
  stays alive while silently forwarding nothing.

Every one of those disappears if the browser runs inside the cluster.

## Key design — move the browser, not the packets

Run the suite as a **Job in the `roomler-ai-e2e` namespace**:

- the browser reaches `http://roomler2` (the Service) directly — same origin as
  `frontend_url`, so the cookie/WS origin check passes with no override, and my
  2026-08-29 `frontend_url` patch is **reverted** as part of this;
- RTP goes pod-to-pod on the CNI network, which is what a real call does;
- Mailpit is an in-cluster Service, so email specs keep working;
- no port-forwards at all, so the supervisor and its failure modes go away.

Getting the specs in: an `initContainer` that clones the repo at the tag under
test into an `emptyDir`, then the Playwright container runs from it. Nothing is
baked into an image, so a spec change needs no image build.

⚠️ **Do NOT instead give the e2e pod `hostNetwork: true`.** It would make the
node IP reachable from mars and look like a fix, but it binds host ports on a
shared worker and drags in the announced-IP mapping prod needs — trading a test
seam for a production-shaped hazard on a node that also runs real workloads.

## Phases

| # | phase | kill switch |
|---|---|---|
| P1 | Job manifest + initContainer clone; prove one conference spec passes in-cluster | the existing host-side path stays until this is green |
| P2 | Point `scripts/e2e-nightly.sh` at the Job; delete the port-forward supervisor and revert the `frontend_url` override | revert the script; the overlay change is one line |
| P3 | Re-triage `scripts/e2e-expected-failures.txt` — the conference entries exist only because of this, and must not silently stay | — |

## Acceptance criteria

- [ ] A two-participant conference spec passes **in-cluster**, with real RTP —
      not skipped, not stubbed
- [ ] The suite runs with **zero** `kubectl port-forward` invocations
- [ ] `ROOMLER__APP__FRONTEND_URL` in the e2e overlay is back to the in-cluster
      service name, and the WS still authenticates
- [ ] `e2e-expected-failures.txt` loses the conference entries; anything still
      failing is either fixed or re-justified in writing
- [ ] The nightly writes its usual `LATEST` line from the new path

## Open decisions

- **Where does the Job's result go?** The nightly currently parses stdout on
  the build host. Simplest is `kubectl logs --follow` on the Job; the parsing
  is unchanged.
- **rc-vp9-444** needs an agent, not a browser — it stays an expected failure
  and should say so explicitly rather than riding along with the conference
  entries.

## Out of scope

- Prod's media path (already field-proven; see FR-4).
- Running this in GitHub Actions — it needs the cluster.

## Field-verification log

- (pending)
