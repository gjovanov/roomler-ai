#!/usr/bin/env bash
# `xcrun stapler <staple|validate> <path>`, retried.
#
# Stapling fetches the notarisation ticket from Apple's CloudKit
# (api.apple-cloudkit.com, the `gk.ticket-delivery` container), and validating
# reads it back from there. Since 2026-10-12 that connection has dropped
# mid-request on GitHub's macOS runners, AFTER Apple had answered `Accepted`:
# `-1005 "The network connection was lost"`, exit 68, with no incident on
# Apple's status page. It failed five of nine release attempts over two days,
# split between `staple` and `validate`, and a whole-job re-run repeats the
# 25-minute build and a fresh notarisation to get past one network blip.
#
# This retries the ticket step alone. Every failure is retried, not only exit
# 68: a deterministic one costs about three more minutes, and each attempt's
# own message stays in the log above the verdict.
#
# Usage: stapler-retry.sh <staple|validate> <path>
# (STAPLER_TRIES, default 5; STAPLER_RETRY_DELAY_S, default 20: the n-th wait is n times it)
set -uo pipefail
verb="${1:?usage: stapler-retry.sh <staple|validate> <path>}"
target="${2:?usage: stapler-retry.sh <staple|validate> <path>}"
case "$verb" in staple|validate) ;; *) echo "::error::stapler-retry.sh: unknown verb '$verb'"; exit 2 ;; esac
tries="${STAPLER_TRIES:-5}"
delay="${STAPLER_RETRY_DELAY_S:-20}"
rc=1
for i in $(seq 1 "$tries"); do
  xcrun stapler "$verb" "$target"
  rc=$?
  [ "$rc" -eq 0 ] && exit 0
  if [ "$i" -lt "$tries" ]; then
    echo "::warning::xcrun stapler $verb failed (exit $rc), attempt $i of $tries; retrying in $((i * delay)) s"
    sleep $((i * delay))
  fi
done
echo "::error::xcrun stapler $verb failed $tries times (last exit $rc)"
exit "$rc"
