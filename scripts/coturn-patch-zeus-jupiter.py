"""One-off patch: rewrite the :443 block in /usr/local/bin/coturn-iptables.sh
on zeus + jupiter so UDP/443 routes to coturn:3478 (plain TURN) instead of
:5349 (DTLS/TLS). See coturn-patch-mars.py for the full rationale.

zeus/jupiter use a different script layout than mars (literal iptables lines
instead of an add_dnat() helper loop), so the patch is a simple string
substitution on the two UDP/443 lines.
"""
import pathlib, sys

p = pathlib.Path("/usr/local/bin/coturn-iptables.sh")
src = p.read_text(encoding="utf-8")
old = (
    'iptables -t nat -A "$DNAT_CHAIN" -d "$PUBLIC_IP" -p udp --dport 443 '
    '-j DNAT --to-destination "$DEST_IP":5349'
)
new_dnat = (
    'iptables -t nat -A "$DNAT_CHAIN" -d "$PUBLIC_IP" -p udp --dport 443 '
    '-j DNAT --to-destination "$DEST_IP":3478'
)
old_out = (
    'iptables -t nat -A "$OUTPUT_DNAT_CHAIN" -d "$PUBLIC_IP" -p udp --dport 443 '
    '-j DNAT --to-destination "$DEST_IP":5349'
)
new_out = (
    'iptables -t nat -A "$OUTPUT_DNAT_CHAIN" -d "$PUBLIC_IP" -p udp --dport 443 '
    '-j DNAT --to-destination "$DEST_IP":3478'
)

replaced = 0
if old in src:
    src = src.replace(old, new_dnat)
    replaced += 1
if old_out in src:
    src = src.replace(old_out, new_out)
    replaced += 1

if replaced != 2:
    sys.exit(f"FAIL: expected 2 substitutions, made {replaced}")
p.write_text(src, encoding="utf-8")
print(f"patched OK ({replaced} substitutions)")
