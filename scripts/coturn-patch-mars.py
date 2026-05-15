"""One-off patch: rewrite the :443 block in /usr/local/bin/coturn-iptables.sh
on mars so UDP/443 routes to coturn:3478 (plain TURN) instead of :5349
(DTLS/TLS). webrtc-rs does NOT implement DTLS-over-UDP (upstream issue
webrtc-rs/webrtc#690, closed NOT_PLANNED), so the existing UDP/443 -> :5349
rule produces zero usable candidates for the roomler-agent. Plain TURN/UDP
on :443 is the corporate-firewall bypass.

TCP/443 stays at :5349 for browsers' TURNS/TCP path.
"""
import pathlib, re, sys

p = pathlib.Path("/usr/local/bin/coturn-iptables.sh")
src = p.read_text(encoding="utf-8")
pattern = re.compile(
    r"# TURNS-over-:443 only on SECONDARY_IP \(PRIMARY_IP :443 is owned by host nginx\)\.\n"
    r"# Redirect to coturn.*?\nfor proto in tcp udp; do\n"
    r"  add_dnat \"\$SECONDARY_IP\" \"\$proto\" 443 5349\ndone",
    re.DOTALL,
)
replacement = (
    "# :443 on SECONDARY_IP (PRIMARY :443 = host nginx). TCP/443 -> coturn:5349\n"
    "# (TLS, browser TURNS/TCP path). UDP/443 -> coturn:3478 (plain TURN) -\n"
    "# webrtc-rs does NOT implement DTLS-over-UDP (upstream #690 NOT_PLANNED),\n"
    "# so plain TURN/UDP on :443 is the corporate-firewall bypass for the agent.\n"
    "add_dnat \"$SECONDARY_IP\" \"tcp\" 443 5349\n"
    "add_dnat \"$SECONDARY_IP\" \"udp\" 443 3478"
)
new, n = pattern.subn(replacement, src)
if n != 1:
    sys.exit(f"FAIL: expected 1 match, got {n}")
p.write_text(new, encoding="utf-8")
print(f"mars: patched OK ({n} replacement)")
