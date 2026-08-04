#!/usr/bin/env python3
"""Relay-PoP health probe — run from mars cron (or anywhere) against every PoP.

Per region: a real STUN Binding round-trip against the coturn UDP port
(what the agents' nearest-region probes measure) + the DERP relay's
/healthz over TLS. Prints one line per check; exits non-zero if ANY fails,
so a cron wrapper can alert (the e2e-nightly issue-lane pattern).

Usage:
  healthcheck.py REGION=coturn-host:port,derp-host [REGION=...]...
e.g.
  healthcheck.py us-east=coturn-us-east.roomler.ai:3478,derp-us-east.roomler.ai \
                 eu-central=coturn.roomler.ai:3478,
(the trailing comma / empty derp host = region without a DERP relay)
"""

import os
import socket
import ssl
import struct
import sys
import time
import urllib.request

STUN_MAGIC = 0x2112A442
TIMEOUT_S = 3.0


def stun_rtt_ms(host: str, port: int) -> float:
    txn = os.urandom(12)
    req = struct.pack(">HHI", 0x0001, 0, STUN_MAGIC) + txn
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.settimeout(TIMEOUT_S)
        t0 = time.monotonic()
        s.sendto(req, (host, port))
        while True:
            data, addr = s.recvfrom(2048)
            if len(data) >= 20 and data[0:2] == b"\x01\x01" and data[8:20] == txn:
                return (time.monotonic() - t0) * 1000.0


def derp_healthz(host: str) -> bool:
    url = f"https://{host}/healthz"
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(url, timeout=TIMEOUT_S, context=ctx) as r:
        return r.status == 200 and r.read().strip() == b"ok"


def main() -> int:
    failures = 0
    for spec in sys.argv[1:]:
        region, _, rest = spec.partition("=")
        coturn, _, derp = rest.partition(",")
        host, _, port_s = coturn.partition(":")
        port = int(port_s or "3478")
        try:
            rtt = stun_rtt_ms(host, port)
            print(f"OK   {region:16s} stun {host}:{port} {rtt:.0f} ms")
        except Exception as e:  # noqa: BLE001 — any failure is a FAIL line
            print(f"FAIL {region:16s} stun {host}:{port} ({e})")
            failures += 1
        if derp:
            try:
                assert derp_healthz(derp)
                print(f"OK   {region:16s} derp {derp}")
            except Exception as e:  # noqa: BLE001
                print(f"FAIL {region:16s} derp {derp} ({e})")
                failures += 1
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
