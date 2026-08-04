# coturn config for a relay PoP — rendered by provision.sh from pop.env.
# Serves: TURN/UDP+TCP on 3478, TURNS on 5349 (and, via the host's SNI
# router, TURNS behind coturn-${REGION} on :443). UDP/443 arrives via the
# iptables DNAT provision.sh installs (it preserves the client source addr,
# which nothing user-space can).
listening-port=3478
tls-listening-port=5349
fingerprint
use-auth-secret
static-auth-secret=${TURN_SHARED_SECRET}
realm=${COTURN_HOST}
# LE cert (SAN covers coturn-${REGION} + derp-${REGION}); deployed by acme.sh.
cert=/certs/fullchain.pem
pkey=/certs/key.pem
min-port=${TURN_MIN_PORT}
max-port=${TURN_MAX_PORT}
# NAT'd providers: external-ip maps the relayed addresses correctly.
${EXTERNAL_IP_LINE}
# A PoP relays media only — no loopback/multicast peers, no cli.
no-loopback-peers
no-multicast-peers
no-cli
log-file=stdout
simple-log
