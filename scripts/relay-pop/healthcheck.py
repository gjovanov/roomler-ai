#!/usr/bin/env python3
"""Relay-PoP health probe — run from mars cron (or anywhere) against every PoP.

Per region, one check per transport variant the server advertises for a PoP
(`expand_turn_url` with the PoP caps `{"tls_443_udp": false}`):

  stun/udp  host:<base>       what the agents' nearest-region probes measure
  stun/udp  host:443          the iptables DNAT (corp firewalls pass UDP/443 as QUIC)
  stun/tcp  host:<base>       `turn:…?transport=tcp`
  stun/tls  host:5349         `turns:…:5349?transport=tcp` (coturn's own TLS listener)
  stun/tls  host:443          `turns:…:443?transport=tcp` — the nginx SNI passthrough
  alloc/udp host:443          full TURN Allocate (REST auth) — needs TURN_SECRET_FILE
  alloc/tls host:443          same over TLS — needs TURN_SECRET_FILE
  derp      https://host/healthz

The two `alloc` rows are the only checks that catch static-auth-secret drift
(the fleet-wide-401 class) and relay-address/external-ip misconfig (the
webrtc-rs `attribute not found` class); without a secret they print SKIP —
never silently absent. Prints one line per check; exits non-zero if ANY
fails, so a cron wrapper can alert (the e2e-nightly issue-lane pattern).

Usage:
  [TURN_SECRET_FILE=/home/gjovanov/coturn.auth] \
  healthcheck.py REGION=coturn-host:port,derp-host [REGION=...]...
e.g.
  TURN_SECRET_FILE=~/coturn.auth healthcheck.py \
      us-east=coturn-us-east.roomler.ai:3478,derp-us-east.roomler.ai \
      eu-central=coturn.roomler.ai:3478,
(the trailing comma / empty derp host = region without a DERP relay;
TURN_SECRET works too when a file is impractical)
"""

import base64
import hashlib
import hmac
import os
import socket
import ssl
import struct
import sys
import time
import urllib.request

STUN_MAGIC = 0x2112A442
TIMEOUT_S = 3.0

BINDING_REQ = 0x0001
ALLOCATE_REQ = 0x0003
REFRESH_REQ = 0x0004
ATTR_USERNAME = 0x0006
ATTR_MESSAGE_INTEGRITY = 0x0008
ATTR_ERROR_CODE = 0x0009
ATTR_LIFETIME = 0x000D
ATTR_REALM = 0x0014
ATTR_NONCE = 0x0015
ATTR_XOR_RELAYED_ADDRESS = 0x0016
ATTR_REQUESTED_TRANSPORT = 0x0019


def _attr(atype: int, value: bytes) -> bytes:
    return struct.pack(">HH", atype, len(value)) + value + b"\x00" * (-len(value) % 4)


def _msg(mtype: int, txn: bytes, attrs: bytes = b"", integrity_key: bytes | None = None) -> bytes:
    if integrity_key is None:
        return struct.pack(">HHI", mtype, len(attrs), STUN_MAGIC) + txn + attrs
    # RFC 5389 §15.4: the header length counts the MESSAGE-INTEGRITY attribute
    # (24 bytes) while the HMAC input stops just before it.
    hdr = struct.pack(">HHI", mtype, len(attrs) + 24, STUN_MAGIC) + txn
    mac = hmac.new(integrity_key, hdr + attrs, hashlib.sha1).digest()
    return hdr + attrs + _attr(ATTR_MESSAGE_INTEGRITY, mac)


def _parse_attrs(body: bytes) -> dict[int, bytes]:
    out: dict[int, bytes] = {}
    i = 0
    while i + 4 <= len(body):
        atype, alen = struct.unpack_from(">HH", body, i)
        out.setdefault(atype, body[i + 4 : i + 4 + alen])
        i += 4 + alen + (-alen % 4)
    return out


def _err_code(attrs: dict[int, bytes]) -> int | None:
    v = attrs.get(ATTR_ERROR_CODE)
    if v is None or len(v) < 4:
        return None
    return (v[2] & 0x07) * 100 + v[3]


def _xor_addr(value: bytes, txn: bytes) -> str:
    fam, port = struct.unpack_from(">xBH", value, 0)
    port ^= STUN_MAGIC >> 16
    if fam == 0x01:
        addr = struct.unpack_from(">I", value, 4)[0] ^ STUN_MAGIC
        return f"{socket.inet_ntoa(struct.pack('>I', addr))}:{port}"
    mask = struct.pack(">I", STUN_MAGIC) + txn
    raw = bytes(b ^ m for b, m in zip(value[4:20], mask))
    return f"[{socket.inet_ntop(socket.AF_INET6, raw)}]:{port}"


class Dgram:
    """UDP transport: one socket, replies matched by transaction id."""

    def __init__(self, host: str, port: int):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.settimeout(TIMEOUT_S)
        self.dest = (host, port)

    def send(self, data: bytes) -> None:
        self.sock.sendto(data, self.dest)

    def recv_msg(self, txn: bytes) -> tuple[int, bytes]:
        while True:
            data, _addr = self.sock.recvfrom(4096)
            if len(data) >= 20 and data[8:20] == txn:
                (mtype, mlen) = struct.unpack_from(">HH", data, 0)
                return mtype, data[20 : 20 + mlen]

    def close(self) -> None:
        self.sock.close()


class Stream:
    """TCP / TLS transport: framed STUN messages on one connection.

    `tls_sni` also turns on full certificate + hostname verification, so a
    TLS check failing here means the corp-escape path is really broken (SNI
    map regression, expired cert, wrong cert behind the nginx passthrough).
    """

    def __init__(self, host: str, port: int, tls_sni: str | None = None):
        raw = socket.create_connection((host, port), timeout=TIMEOUT_S)
        raw.settimeout(TIMEOUT_S)
        if tls_sni is not None:
            ctx = ssl.create_default_context()
            self.sock: socket.socket = ctx.wrap_socket(raw, server_hostname=tls_sni)
            self.sock.settimeout(TIMEOUT_S)
        else:
            self.sock = raw

    def send(self, data: bytes) -> None:
        self.sock.sendall(data)

    def _read_exact(self, n: int) -> bytes:
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError("connection closed mid-message")
            buf += chunk
        return buf

    def recv_msg(self, txn: bytes) -> tuple[int, bytes]:
        while True:
            hdr = self._read_exact(20)
            mtype, mlen = struct.unpack_from(">HH", hdr, 0)
            body = self._read_exact(mlen)
            if hdr[8:20] == txn:
                return mtype, body

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def stun_rtt_ms(conn) -> float:
    txn = os.urandom(12)
    t0 = time.monotonic()
    conn.send(_msg(BINDING_REQ, txn))
    mtype, _body = conn.recv_msg(txn)
    if mtype != 0x0101:
        raise RuntimeError(f"binding response type 0x{mtype:04x}")
    return (time.monotonic() - t0) * 1000.0


def rest_creds(secret: str) -> tuple[str, str]:
    """coturn REST creds (draft-uberti-behave-turn-rest) — the exact recipe
    `turn_creds.rs::issue_with_ttl` uses server-side."""
    username = f"{int(time.time()) + 600}:healthcheck"
    digest = hmac.new(secret.encode(), username.encode(), hashlib.sha1).digest()
    return username, base64.b64encode(digest).decode()


def turn_allocate(conn, username: str, password: str) -> tuple[float, str]:
    """Allocate → 401 challenge → authenticated Allocate → relay address,
    then a best-effort Refresh(lifetime=0) so probes don't pile up
    allocations. Returns (rtt-to-success, relayed transport address)."""
    transport = _attr(ATTR_REQUESTED_TRANSPORT, struct.pack(">B3x", 17))
    txn = os.urandom(12)
    t0 = time.monotonic()
    conn.send(_msg(ALLOCATE_REQ, txn, transport))
    mtype, body = conn.recv_msg(txn)
    attrs = _parse_attrs(body)
    if mtype != 0x0113 or _err_code(attrs) != 401:
        raise RuntimeError(f"expected 401 challenge, got 0x{mtype:04x} err={_err_code(attrs)}")
    realm, nonce = attrs.get(ATTR_REALM), attrs.get(ATTR_NONCE)
    if realm is None or nonce is None:
        raise RuntimeError("401 challenge missing REALM/NONCE")
    key = hashlib.md5(username.encode() + b":" + realm + b":" + password.encode()).digest()
    auth = _attr(ATTR_USERNAME, username.encode()) + _attr(ATTR_REALM, realm) + _attr(ATTR_NONCE, nonce)
    txn2 = os.urandom(12)
    conn.send(_msg(ALLOCATE_REQ, txn2, transport + auth, integrity_key=key))
    mtype, body = conn.recv_msg(txn2)
    attrs = _parse_attrs(body)
    if mtype != 0x0103:
        code = _err_code(attrs)
        hint = " — static-auth-secret drift?" if code == 401 else ""
        raise RuntimeError(f"allocate rejected 0x{mtype:04x} err={code}{hint}")
    relay = attrs.get(ATTR_XOR_RELAYED_ADDRESS)
    if relay is None:
        raise RuntimeError("success response without XOR-RELAYED-ADDRESS")
    rtt = (time.monotonic() - t0) * 1000.0
    try:
        txn3 = os.urandom(12)
        release = _attr(ATTR_LIFETIME, struct.pack(">I", 0)) + auth
        conn.send(_msg(REFRESH_REQ, txn3, release, integrity_key=key))
        conn.recv_msg(txn3)
    except Exception:  # noqa: BLE001 — release is best-effort; expiry covers it
        pass
    return rtt, _xor_addr(relay, txn2)


def derp_healthz(host: str) -> bool:
    url = f"https://{host}/healthz"
    ctx = ssl.create_default_context()
    with urllib.request.urlopen(url, timeout=TIMEOUT_S, context=ctx) as r:
        return r.status == 200 and r.read().strip() == b"ok"


def load_secret() -> str | None:
    path = os.environ.get("TURN_SECRET_FILE")
    if path:
        with open(os.path.expanduser(path), encoding="ascii") as f:
            return f.read().strip()
    env = os.environ.get("TURN_SECRET")
    return env.strip() if env else None


def main() -> int:
    failures = 0
    secret = load_secret()

    def run(region: str, label: str, target: str, fn):
        nonlocal failures
        try:
            detail = fn()
            print(f"OK   {region:16s} {label:9s} {target} {detail}".rstrip())
        except Exception as e:  # noqa: BLE001 — any failure is a FAIL line
            print(f"FAIL {region:16s} {label:9s} {target} ({e})")
            failures += 1

    def stun_via(mk):
        def go():
            conn = mk()
            try:
                return f"{stun_rtt_ms(conn):.0f} ms"
            finally:
                conn.close()

        return go

    def alloc_via(mk):
        def go():
            user, pw = rest_creds(secret)
            conn = mk()
            try:
                rtt, relay = turn_allocate(conn, user, pw)
                return f"relay={relay} {rtt:.0f} ms"
            finally:
                conn.close()

        return go

    for spec in sys.argv[1:]:
        region, _, rest = spec.partition("=")
        coturn, _, derp = rest.partition(",")
        host, _, port_s = coturn.partition(":")
        port = int(port_s or "3478")

        run(region, "stun/udp", f"{host}:{port}", stun_via(lambda: Dgram(host, port)))
        if port != 443:
            run(region, "stun/udp", f"{host}:443", stun_via(lambda: Dgram(host, 443)))
        run(region, "stun/tcp", f"{host}:{port}", stun_via(lambda: Stream(host, port)))
        run(region, "stun/tls", f"{host}:5349", stun_via(lambda: Stream(host, 5349, tls_sni=host)))
        run(region, "stun/tls", f"{host}:443", stun_via(lambda: Stream(host, 443, tls_sni=host)))
        if secret:
            run(region, "alloc/udp", f"{host}:443", alloc_via(lambda: Dgram(host, 443)))
            run(region, "alloc/tls", f"{host}:443", alloc_via(lambda: Stream(host, 443, tls_sni=host)))
        else:
            print(f"SKIP {region:16s} alloc/udp+tls {host}:443 (TURN_SECRET_FILE unset)")
        if derp:

            def derp_check(h=derp):
                if not derp_healthz(h):
                    raise RuntimeError("healthz not ok")
                return ""

            run(region, "derp", derp, derp_check)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
