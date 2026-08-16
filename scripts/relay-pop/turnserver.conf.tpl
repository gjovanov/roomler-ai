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
# Bind every allocation's relay socket to this address EXPLICITLY. Without it
# coturn derives the relay bind from the listener the CLIENT arrived on — a
# TLS/443 client proxied in by the SNI-split nginx arrives from 127.0.0.1, so
# its relay socket silently binds loopback while the ADVERTISED relay address
# stays public: the Allocate succeeds and every relayed byte then vanishes.
# Field-diagnosed 2026-08-17 on eu-north (tcpdump: relayed frames leaving with
# src=127.0.0.1). The relay-echo row in healthcheck.py guards this class.
relay-ip=${RELAY_IP}
# A PoP relays media only — no multicast peers, no cli. (Loopback peers are
# deny-by-default in modern coturn; the old `no-loopback-peers` option is gone
# and only triggers a "Bad configuration format" boot warning.)
no-multicast-peers
no-cli
# Allocation/session gauges for the derp-relay /stats endpoint (the API's
# load-aware routing input). Binds :9641 on all interfaces — provision.sh
# firewalls it to loopback; only derp-relay's localhost scrape reaches it.
prometheus
log-file=stdout
simple-log
