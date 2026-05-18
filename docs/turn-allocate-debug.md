# TURN Allocate "attribute not found" — investigation notes

Field repro 2026-05-17 from CLK00017265 / PC55331 agent logs:

```
WARN [controlled]: could not get server reflexive address udp4 turn:coturn.roomler.ai:443?transport=udp: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 turn:coturn.roomler.ai:3478?transport=udp: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 turn:coturn.roomler.ai:3478?transport=tcp: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 turns:coturn.roomler.ai:443?transport=udp: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 turns:coturn.roomler.ai:443?transport=tcp: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 stun:stun.l.google.com:19302: deadline has elapsed
WARN [controlled]: could not get server reflexive address udp4 turns:coturn.roomler.ai:5349?transport=tcp: deadline has elapsed
...
WARN [controlled]: TURN allocate failed addr=coturn.roomler.ai:3478 ... err=attribute not found
WARN [controlled]: TURN allocate failed addr=coturn.roomler.ai:443 ... err=attribute not found
```

Two distinct sub-issues, traced 2026-05-18.

## Sub-issue 1 — STUN `srflx` timeouts (network-side)

All seven `srflx` (server-reflexive) gather attempts time out — including
`stun:stun.l.google.com:19302`. **This points at outbound UDP being
blocked entirely on the host's network**, not a roomler-side bug. The
host is a corporate Windows endpoint where outbound UDP/3478, UDP/443
and likely all-UDP-on-arbitrary-ports are firewalled. Workaround for
the field: TCP/TLS relay path (`turns:443?transport=tcp`) via the
rc.32 vendor patch.

Not actionable from agent code; documented for completeness.

## Sub-issue 2 — TURN allocate "attribute not found" (client-side)

This is the diagnostically-rich case. Investigation 2026-05-18:

### Coturn server is configured correctly

Live config of `coturn-worker-{1,2,3}` (k8s ns `coturn`, coturn 4.11.0):

```
listening-port=3478
tls-listening-port=5349
alt-tls-listening-port=443
realm=roomler.ai
fingerprint
lt-cred-mech
use-auth-secret
static-auth-secret=<32-byte hex>
```

`use-auth-secret` enables coturn's REST-API (HMAC-of-shared-secret)
auth flow, which matches the backend's `TurnConfig::issue` username
format (`<unix_expiry>:<user_id>`) + HMAC-SHA1 password derivation.

### Coturn responds correctly to a known-good probe

In-cluster probe with `turnutils_uclient` against `127.0.0.1:3478`
using REST-API credentials matching the production format:

```
EXPIRY=$(($(date +%s)+600))
USER=$EXPIRY:probe
SECRET=<static-auth-secret>
PASS=$(echo -n $USER | openssl dgst -binary -sha1 -hmac $SECRET | base64)
turnutils_uclient -p 3478 -y -u $USER -w $PASS -n 1 -c -L 127.0.0.1 127.0.0.1
```

Coturn verbose log (filtered):

```
session <id>: realm <roomler.ai> user <>: incoming packet message processed, error 401: Unauthorized
session <id>: new, realm=<roomler.ai>, username=<…:probe>, lifetime=777
session <id>: realm <roomler.ai> user <…:probe>: incoming packet ALLOCATE processed, success
session <id>: refreshed, realm=<roomler.ai>, username=<…:probe>, lifetime=777
```

The full anonymous-Allocate → 401+NONCE+REALM → authenticated-retry →
200+XOR-RELAYED-ADDRESS+LIFETIME round-trip works.

### The failing agent sessions show NO incoming-packet processing

From the SAME coturn instance's logs, sessions from the agent's IP
show only:

```
session <id>: TCP socket closed remotely <agent-ip>:<port>
session <id>: usage: realm=<roomler.ai>, username=<>, rp=0, rb=0, sp=0, sb=0
session <id>: closed (2nd stage), user <> realm <roomler.ai> origin <>, ... reason: TCP connection closed by client (callback)
```

NO `incoming packet message processed` line. The TCP connection
opens, then closes — coturn never logs that it processed an
Allocate request from these sessions.

This is consistent with one of two scenarios:

**(a)** The webrtc-rs `turn-0.9.0` client TCP-connects to coturn,
   sends an anonymous Allocate, reads coturn's response, fails
   `Nonce::get_from_as(&res, ATTR_NONCE)?` in
   `turn-0.9.0::client::ClientInternal::allocate()` at line 540,
   bubbles up as `"attribute not found"`, and closes the connection.
   Coturn DID process the Allocate request (would log "processed")
   but **may not be visible at the log level being captured**.

**(b)** A corporate middlebox between CLK00017265 and coturn
   intercepts the STUN bytes and either (b1) modifies them in
   flight, stripping NONCE/REALM from the response, or (b2) drops
   the response entirely so the client times out — but the bare
   `attribute not found` rules (b2) out.

### Why my in-pod probe works and the agent's wide-area one doesn't

Same coturn pod, same credentials format, different protocol-level
outcome. The hypothesis-(b1) corporate-middlebox theory is the
working theory. STUN messages over TCP/3478 on port 3478 are a
relatively rare protocol; some inspection/DLP appliances filter the
attribute stream.

## Diagnostic patch shipped in rc.40

`crates/vendored/webrtc-ice/src/agent/agent_gather.rs:902` extended
the failure log with `scheme + transport + username` fields. Next
field repro will tell us:

- Which URL flavour failed (TURN/UDP vs TURNS/TCP)
- Whether the failure correlates with a specific transport-blocking
  middlebox pattern

If the next sample shows the same "attribute not found" pattern AND
multiple URL flavours fail identically, the theory shifts to
hypothesis (a) — a stun-0.7.0 decoder corner case worth filing
upstream / vendoring + patching.

## Next-step options (ranked by cost / value)

1. **Packet capture on CLK00017265** via Wireshark / `pktmon` for
   `tcp.port == 3478` during a fresh agent connection. Compare the
   STUN bytes coturn sent vs what webrtc-rs saw — confirms or
   refutes middlebox interference. ~30 min, definitive answer.
2. **Add `roomler-agent turn-probe` CLI subcommand** that does a
   manual TURN Allocate + dumps full STUN message exchange at debug
   level. Repeatable on-host diagnostic without a packet capture.
   ~2-3 hours.
3. **Vendor `turn-0.9.0`** + patch `client::ClientInternal::allocate()`
   to log raw response bytes when NONCE extraction fails. Heaviest
   path; probably overkill before we have packet-capture evidence
   that the bug is webrtc-rs-side. ~1 day.

(1) is the right next step — needs a person at CLK00017265 to run
`pktmon` for 30 sec while the agent reconnects. Not actionable from
this session.

## Not in scope for rc.40

- coturn config change: NONE NEEDED. The `no-tlsv1` / `no-tlsv1_1`
  warnings in coturn's startup are cosmetic; those flags were
  removed in coturn 4.6+ but are silently ignored, not breaking
  anything.
- Backend TURN URL generation: NONE NEEDED. The 6-URL emission in
  `crates/api/src/state.rs::build_turn_config` matches what
  webrtc-rs (with our vendor patches) can handle: UDP/Turn +
  TCP/Turns.
