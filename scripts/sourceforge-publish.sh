#!/usr/bin/env bash
# sourceforge-publish.sh — every DECISION of the SourceForge publish step (FR-143).
#
# .github/workflows/sourceforge-publish.yml does the network half: it downloads a
# `setup-v<V>` release from GitHub, uploads it to SourceForge, sets the per-OS
# Download defaults and removes the versions that have aged out. What it uploads,
# which file becomes whose default and which folders it removes are decided here,
# as functions of their arguments and stdin alone:
#
#   valid-tag     <tag>                  exit 0 for a final setup release tag, 1 otherwise
#   newest                               tag names on stdin -> the highest final setup tag
#   default-for   <tag> <asset-name>     prints windows | mac | linux, or nothing
#   defaults-plan <tag> <dir>            prints "<os><TAB><file>" for the three defaults,
#                                        fails when the release lacks one of them
#   verify        <dir> <digests.tsv>    every file against GitHub's asset digest, and
#                                        every installer against its .sha256 sidecar
#   prune-plan    <keep> <current-tag>   folder names on stdin -> the folders to remove,
#                                        oldest first
#   listing-state <tag>                  what the listing serves for windows, mac and
#                                        linux on stdin -> current | behind | newer
#   reconcile-plan <tag> <folders> <listing>
#                                        what the reconcile does about the newest
#                                        release -> publish | verified | nothing
#   --selftest                           proves each of the above, offline
#
# ⚠️ This file makes no network call and must never gain one. `--selftest` runs on
# every pull request that touches it and on any developer's machine; a test of a
# script that can reach SourceForge is a test that one day deletes a release there.
# The workflow owns ssh, rsync and curl, and its `dry_run` input is how those are
# rehearsed.
#
# ⚠️ prune-plan removes only what it can NAME: a folder called exactly
# `setup-v<major>.<minor>.<patch>`. Anything else on SourceForge (a folder someone
# made by hand, another release family, a stray file) is left alone, because a
# retention job that guesses is how a listing loses the one file it needed.

set -euo pipefail

readonly TAG_RE='^setup-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
readonly HEX64_RE='^[0-9a-f]{64}$'
# A retention of one or two versions is a typo, not a policy: the Download
# default has to survive a release that is pulled back.
readonly KEEP_FLOOR=3

die() { echo "sourceforge-publish: $*" >&2; exit 2; }

valid_tag() { [[ "${1:-}" =~ $TAG_RE ]]; }

# Final setup tags from stdin, oldest first, by version and not by text: 0.4.9 is
# older than 0.4.10. Everything that is not such a tag is dropped.
sorted_tags() {
  local line name all=""
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    case "$line" in 'sftp>'*) continue ;; esac
    line="${line%/}"
    name="${line##*/}"
    valid_tag "$name" || continue
    all+="${name#setup-v}"$'\n'
  done
  printf '%s' "$all" | LC_ALL=C sort -u -t. -k1,1n -k2,2n -k3,3n | sed 's/^/setup-v/'
}

# The highest final setup tag among the names on stdin. Only that release may
# take the Download defaults, so a re-run of an older one cannot take them back.
newest() {
  local all
  all="$(sorted_tags)"
  [ -n "$all" ] || die "no final setup release tag in the list"
  printf '%s\n' "$all" | tail -n 1
}

# The one file that is an OS's Download default in version <v>. Exact names only:
# the Windows `.zip` holds the same EXE and the aarch64 tarball is a second Linux
# build, and neither may ever win the button by matching a looser pattern.
default_file() {
  case "$1" in
    windows) echo "roomler-setup-${2}-x86_64-pc-windows-msvc.exe" ;;
    mac)     echo "roomler-setup-${2}-universal-apple-darwin.tar.gz" ;;
    linux)   echo "roomler-setup-${2}-x86_64-unknown-linux-gnu.tar.gz" ;;
    *)       die "no such platform: '$1'" ;;
  esac
}

# Which OS's Download default an asset becomes: nothing for every other asset.
default_for() {
  local tag="$1" name="$2" v os
  valid_tag "$tag" || die "not a final setup release tag: '$tag'"
  v="${tag#setup-v}"
  for os in windows mac linux; do
    if [ "$name" = "$(default_file "$os" "$v")" ]; then echo "$os"; fi
  done
}

# The three defaults of a release, or a failure naming the one that is missing.
# A wizard release is every platform or none (release-setup.yml), so a release
# without one of these is broken, and moving two buttons to it would leave the
# third on an older version than its neighbours.
defaults_plan() {
  local tag="$1" dir="$2" v os file missing=0
  valid_tag "$tag" || die "not a final setup release tag: '$tag'"
  [ -d "$dir" ] || die "no such directory: '$dir'"
  v="${tag#setup-v}"
  for os in windows mac linux; do
    file="$(default_file "$os" "$v")"
    if [ -f "$dir/$file" ]; then
      printf '%s\t%s\n' "$os" "$file"
    else
      echo "sourceforge-publish: $tag has no $os installer ($file)" >&2
      missing=1
    fi
  done
  return "$missing"
}

sha256_of() { sha256sum -- "$1" | cut -d' ' -f1; }

# Proves the directory holds exactly the release GitHub holds.
#   digests.tsv: "<asset-name><TAB>sha256:<hex>", one line per asset, from the API.
# Three checks, each of which has to hold for every file:
#   1. the directory and the asset list name the same files, no more and no fewer;
#   2. each file hashes to GitHub's digest for that asset;
#   3. each installer hashes to the value in its own `.sha256` sidecar, which is
#      the number the release lane computed before it uploaded anything.
verify() {
  local dir="$1" digests="$2" bad=0 name want got side listed
  [ -d "$dir" ] || die "no such directory: '$dir'"
  [ -s "$digests" ] || die "the digest list is empty or missing: '$digests'"

  listed="$(cut -f1 "$digests" | tr -d '\r' | LC_ALL=C sort)"
  if [ "$(printf '%s\n' "$listed" | uniq -d | wc -l)" -ne 0 ]; then
    echo "verify: the digest list names an asset twice" >&2
    bad=1
  fi
  local present
  present="$(cd "$dir" && find . -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)"
  if [ "$listed" != "$present" ]; then
    echo "verify: the directory and the release's asset list differ:" >&2
    diff <(printf '%s\n' "$listed") <(printf '%s\n' "$present") | sed 's/^/  /' >&2 || true
    bad=1
  fi

  while IFS=$'\t' read -r name want; do
    name="${name%$'\r'}"; want="${want%$'\r'}"
    [ -n "$name" ] || continue
    case "$name" in */*|.*) echo "verify: refusing the asset name '$name'" >&2; bad=1; continue ;; esac
    want="${want#sha256:}"
    if ! [[ "$want" =~ $HEX64_RE ]]; then
      echo "verify: no usable digest from GitHub for $name" >&2; bad=1; continue
    fi
    [ -f "$dir/$name" ] || continue          # already reported by check 1
    got="$(sha256_of "$dir/$name")"
    if [ "$got" != "$want" ]; then
      echo "verify: $name does not match GitHub's digest" >&2; bad=1; continue
    fi
    case "$name" in
      *.sha256|*.asc) ;;
      *)
        if [ ! -f "$dir/$name.sha256" ]; then
          echo "verify: $name has no .sha256 sidecar" >&2; bad=1; continue
        fi
        side="$(tr -d '\r' < "$dir/$name.sha256" | awk 'NR==1 {print tolower($1)}')"
        if [ "$side" != "$got" ]; then
          echo "verify: $name does not match its .sha256 sidecar" >&2; bad=1; continue
        fi
        ;;
    esac
    echo "ok  $name"
  done < "$digests"

  return "$bad"
}

# Which folders retention removes. stdin: one name per line, as `sftp ls -1`
# prints them (a path prefix, a trailing slash, CRs and the batch echo are all
# tolerated). Prints the `setup-v*` folders that are neither among the newest
# <keep> nor the one just published, oldest first.
prune_plan() {
  local keep="$1" current="$2" all n
  [[ "$keep" =~ ^[0-9]+$ ]] || die "keep must be a whole number, got '$keep'"
  [ "$keep" -ge "$KEEP_FLOOR" ] || die "keep must be at least $KEEP_FLOOR, got $keep"
  valid_tag "$current" || die "not a final setup release tag: '$current'"

  all="$(sorted_tags)"

  # The folder just published has to be there. If it is not, the listing is not
  # the one this run wrote to, and nothing may be removed on its strength.
  if ! printf '%s\n' "$all" | grep -qxF -- "$current"; then
    die "$current is not in the listing: refusing to prune"
  fi

  n="$(printf '%s\n' "$all" | grep -c . || true)"
  [ "$n" -gt "$keep" ] || return 0
  printf '%s\n' "$all" | head -n "$((n - keep))" | grep -vxF -- "$current" || true
}

# Whether the listing's three Download buttons are this release's. stdin: the file
# best_release.json names for windows, for mac and for linux, in that order, one
# per line (`/setup-v0.4.211/roomler-setup-0.4.211-….exe`; an empty line for a
# platform that has no default). Prints one word:
#   current  all three are exactly this release's defaults
#   newer    at least one button serves a setup release NEWER than this one, so
#            this tag is not the newest SourceForge knows
#   behind   anything else: an older release, another file, or no default at all
# It refuses only what it cannot read: a tag that is not one, or not three lines.
listing_state() {
  local tag="$1" v os line folder i=0 state=current
  local -a served=()
  valid_tag "$tag" || die "not a final setup release tag: '$tag'"
  v="${tag#setup-v}"
  while IFS= read -r line || [ -n "$line" ]; do
    # Two statements on purpose. `served+=("${line%$'\r'}")` keeps the CR on
    # bash 5.2.37 (Git for Windows): inside an array assignment that shell does
    # not read `$'\r'` as a carriage return. The plain assignment does, on both.
    line="${line%$'\r'}"
    served+=("$line")
  done
  [ "${#served[@]}" -eq 3 ] || die "want three lines (windows, mac, linux), got ${#served[@]}"
  for os in windows mac linux; do
    line="${served[$i]}"
    i=$((i + 1))
    [ "$line" != "/$tag/$(default_file "$os" "$v")" ] || continue
    [ "$state" = newer ] || state=behind
    folder="${line#/}"
    folder="${folder%%/*}"
    if valid_tag "$folder" && [ "$folder" != "$tag" ] \
       && [ "$(printf '%s\n%s\n' "$folder" "$tag" | sorted_tags | tail -n 1)" = "$folder" ]; then
      state=newer
    fi
  done
  echo "$state"
}

# What the reconcile does about <tag>, the newest final setup release on GitHub,
# from what it could read of SourceForge. It never publishes anything itself: the
# answer `publish` means "send the dispatch this release should have sent".
#   <folders>  ok | unreadable   whether the file store's listing was read. When ok,
#                                stdin is that listing, as `sftp ls -1` prints it.
#   <listing>  current | behind | newer | unreadable
#                                listing-state's word for the three buttons, or
#                                unreadable when best_release.json gave none.
# Prints one word, and on a second line the reason in plain words:
#   publish   the release's folder is not on SourceForge, or the buttons were read
#             and do not serve it, or nothing at all could be read
#   verified  nothing to do, and the buttons were read to be this release's
#   nothing   nothing to do: the folder is there and the buttons could not be read.
#             A publish that failed half-way has an alert of its own.
# The file store decides, because it is the channel a publish needs anyway. The
# buttons only add what they can prove: `behind` sends, `current` verifies.
#
# It refuses when SourceForge carries a setup release NEWER than <tag>, as a folder
# or on a button. That is a release pulled back on GitHub. Sending <tag> again
# would move the buttons backwards, and that is a person's decision.
reconcile_plan() {
  local tag="$1" folders="$2" listing="$3" all="" top
  valid_tag "$tag" || die "not a final setup release tag: '$tag'"
  case "$folders" in
    ok|unreadable) ;;
    *) die "folders must be ok or unreadable, got '$folders'" ;;
  esac
  case "$listing" in
    current|behind|newer|unreadable) ;;
    *) die "listing must be current, behind, newer or unreadable, got '$listing'" ;;
  esac

  if [ "$folders" = ok ]; then
    all="$(sorted_tags)"
    top="$(printf '%s\n' "$all" | tail -n 1)"
    if [ -n "$top" ] && [ "$top" != "$tag" ] \
       && [ "$(printf '%s\n%s\n' "$top" "$tag" | sorted_tags | tail -n 1)" = "$top" ]; then
      die "SourceForge holds $top, which is newer than $tag: refusing"
    fi
  fi
  if [ "$listing" = newer ]; then
    die "a Download button serves a setup release newer than $tag: refusing"
  fi

  if [ "$folders" = ok ] && ! printf '%s\n' "$all" | grep -qxF -- "$tag"; then
    printf 'publish\n%s\n' "SourceForge has no folder for $tag"
  elif [ "$listing" = behind ]; then
    printf 'publish\n%s\n' "the Download buttons do not serve $tag"
  elif [ "$listing" = current ]; then
    printf 'verified\n%s\n' "the Download buttons serve $tag"
  elif [ "$folders" = ok ]; then
    printf 'nothing\n%s\n' "the folder for $tag is on SourceForge, and the buttons could not be read"
  else
    printf 'publish\n%s\n' "neither SourceForge's files nor its buttons could be read"
  fi
}

# ----------------------------------------------------------------------------
# --selftest
# ----------------------------------------------------------------------------
selftest() {
  local pass=0 fail=0 tmp out rc
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" EXIT

  ok()  { pass=$((pass + 1)); }
  bad() { fail=$((fail + 1)); echo "FAIL: $*" >&2; }
  eq()  { if [ "$2" = "$3" ]; then ok; else bad "$1: want '$3', got '$2'"; fi; }
  # Runs a subcommand in a child, so a `die` ends the child and not the selftest.
  # Through `bash`, so it does not depend on this file's execute bit.
  run() { out="$(bash "$0" "$@" 2>"$tmp/err")" && rc=0 || rc=$?; }

  # --- valid-tag ------------------------------------------------------------
  local t
  for t in setup-v0.4.211 setup-v1.0.0 setup-v10.20.30; do
    run valid-tag "$t"; eq "valid-tag $t" "$rc" 0
  done
  for t in '' setup-v0.4 setup-v0.4.211-rc.1 setup-v0.4.211.1 setup-v0.04.1 agent-v0.4.211 \
           'setup-v0.4.211 ' 'setup-v0.4.211;id' 'setup-v0.4.211/..' seed-cache v0.4.211; do
    run valid-tag "$t"; eq "valid-tag rejects '$t'" "$rc" 1
  done

  # --- newest ---------------------------------------------------------------
  out="$(printf '%s\n' setup-v0.4.9 setup-v0.4.211 setup-v0.4.10 agent-v0.4.999 setup-v0.4.212-rc.1 seed-cache \
        | bash "$0" newest)" && rc=0 || rc=$?
  eq "newest: by version, final tags only" "$out" setup-v0.4.211
  out="$(printf '%s\n' setup-v0.9.0 setup-v0.10.0 setup-v1.0.0 setup-v0.99.99 | bash "$0" newest)" && rc=0 || rc=$?
  eq "newest: across minor and major" "$out" setup-v1.0.0
  out="$(printf '%s\n' agent-v0.4.211 seed-cache | bash "$0" newest 2>"$tmp/err")" && rc=0 || rc=$?
  eq "newest refuses a list with no setup tag" "$rc" 2

  # --- default-for ----------------------------------------------------------
  run default-for setup-v0.4.211 roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe;      eq "windows default" "$out" windows
  run default-for setup-v0.4.211 roomler-setup-0.4.211-universal-apple-darwin.tar.gz;   eq "mac default" "$out" mac
  run default-for setup-v0.4.211 roomler-setup-0.4.211-x86_64-unknown-linux-gnu.tar.gz; eq "linux default" "$out" linux
  local n
  for n in roomler-setup-0.4.211-x86_64-pc-windows-msvc.zip \
           roomler-setup-0.4.211-aarch64-unknown-linux-gnu.tar.gz \
           roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe.sha256 \
           roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe.asc \
           roomler-setup-0.4.210-x86_64-pc-windows-msvc.exe \
           roomler-desktop-0.4.211-x86_64-pc-windows-msvc.exe \
           roomler-release-pubkey.asc; do
    run default-for setup-v0.4.211 "$n"; eq "no default for $n" "$out" ''
  done
  run default-for agent-v0.4.211 x; eq "default-for refuses a foreign tag" "$rc" 2

  # --- a release on disk, with its digest list --------------------------------
  local rel="$tmp/rel" dig="$tmp/digests.tsv" f
  mkdir -p "$rel"
  for f in roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe \
           roomler-setup-0.4.211-x86_64-pc-windows-msvc.zip \
           roomler-setup-0.4.211-universal-apple-darwin.tar.gz \
           roomler-setup-0.4.211-x86_64-unknown-linux-gnu.tar.gz \
           roomler-setup-0.4.211-aarch64-unknown-linux-gnu.tar.gz; do
    printf 'payload of %s\n' "$f" > "$rel/$f"
    printf '%s  %s\n' "$(sha256_of "$rel/$f")" "$f" > "$rel/$f.sha256"
    printf 'signature of %s\n' "$f" > "$rel/$f.asc"
  done
  printf 'the public key\n' > "$rel/roomler-release-pubkey.asc"
  write_digests() {
    : > "$dig"
    local g
    for g in "$rel"/*; do printf '%s\tsha256:%s\n' "${g##*/}" "$(sha256_of "$g")" >> "$dig"; done
  }
  write_digests

  # --- defaults-plan ----------------------------------------------------------
  run defaults-plan setup-v0.4.211 "$rel"
  eq "defaults-plan exit" "$rc" 0
  eq "defaults-plan lines" "$(printf '%s\n' "$out" | wc -l | tr -d ' ')" 3
  eq "defaults-plan windows" "$(printf '%s\n' "$out" | awk -F'\t' '$1=="windows"{print $2}')" \
     roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe
  eq "defaults-plan mac" "$(printf '%s\n' "$out" | awk -F'\t' '$1=="mac"{print $2}')" \
     roomler-setup-0.4.211-universal-apple-darwin.tar.gz
  eq "defaults-plan linux" "$(printf '%s\n' "$out" | awk -F'\t' '$1=="linux"{print $2}')" \
     roomler-setup-0.4.211-x86_64-unknown-linux-gnu.tar.gz
  mv "$rel/roomler-setup-0.4.211-universal-apple-darwin.tar.gz" "$tmp/held"
  run defaults-plan setup-v0.4.211 "$rel"; eq "defaults-plan fails without the mac installer" "$rc" 1
  mv "$tmp/held" "$rel/roomler-setup-0.4.211-universal-apple-darwin.tar.gz"
  run defaults-plan setup-v0.4.210 "$rel"; eq "defaults-plan fails for another version" "$rc" 1

  # --- verify -----------------------------------------------------------------
  run verify "$rel" "$dig"; eq "verify accepts an intact release" "$rc" 0
  eq "verify names every file" "$(printf '%s\n' "$out" | grep -c '^ok  ')" 16

  local exe="$rel/roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe"
  cp "$exe" "$tmp/exe.bak"; cp "$exe.sha256" "$tmp/side.bak"
  # Only GitHub's digest disagrees: the file and its sidecar were changed together,
  # which is what a swap on the way from GitHub would look like.
  printf 'tampered\n' >> "$exe"
  printf '%s  %s\n' "$(sha256_of "$exe")" "${exe##*/}" > "$exe.sha256"
  run verify "$rel" "$dig"; eq "verify refuses a file that differs from GitHub's digest" "$rc" 1
  # Only the sidecar disagrees: GitHub's digests are those of the files on disk.
  cp "$tmp/side.bak" "$exe.sha256"; write_digests
  run verify "$rel" "$dig"; eq "verify refuses a file that differs from its sidecar" "$rc" 1
  cp "$tmp/exe.bak" "$exe"; write_digests
  run verify "$rel" "$dig"; eq "verify accepts the restored release" "$rc" 0

  printf 'stray\n' > "$rel/extra.bin"
  run verify "$rel" "$dig"; eq "verify refuses a file the release does not list" "$rc" 1
  rm "$rel/extra.bin"

  mv "$rel/roomler-release-pubkey.asc" "$tmp/held"
  run verify "$rel" "$dig"; eq "verify refuses a missing asset" "$rc" 1
  mv "$tmp/held" "$rel/roomler-release-pubkey.asc"

  mv "$exe.sha256" "$tmp/held"; write_digests
  run verify "$rel" "$dig"; eq "verify refuses an installer with no sidecar" "$rc" 1
  mv "$tmp/held" "$exe.sha256"; write_digests

  sed -i 's/\tsha256:[0-9a-f]*$/\t/' "$dig"
  run verify "$rel" "$dig"; eq "verify refuses an asset list without digests" "$rc" 1
  write_digests
  printf '../escape\tsha256:%s\n' "$(sha256_of "$exe")" >> "$dig"
  run verify "$rel" "$dig"; eq "verify refuses a path in an asset name" "$rc" 1
  write_digests
  run verify "$rel" "$tmp/none.tsv"; eq "verify refuses a missing digest list" "$rc" 2

  # --- prune-plan ---------------------------------------------------------------
  plan() { out="$(printf '%s\n' "${@:3}" | bash "$0" prune-plan "$1" "$2" 2>"$tmp/err")" && rc=0 || rc=$?; }
  local names=() i
  for i in 199 200 201 202 203 204 205 206 207 208 209 210 211; do names+=("setup-v0.4.$i"); done

  plan 10 setup-v0.4.211 "${names[@]}"
  eq "prune: 13 folders, keep 10" "$(echo $out)" "setup-v0.4.199 setup-v0.4.200 setup-v0.4.201"
  plan 13 setup-v0.4.211 "${names[@]}"; eq "prune: exactly keep folders" "$out" ''
  plan 20 setup-v0.4.211 "${names[@]}"; eq "prune: fewer than keep" "$out" ''

  # Version order, not text order: 0.4.9 is older than 0.4.10 and 0.4.100.
  plan 3 setup-v0.4.100 setup-v0.4.100 setup-v0.4.9 setup-v0.4.10 setup-v0.4.99 setup-v0.10.0 setup-v0.4.8
  eq "prune: numeric order" "$(echo $out)" "setup-v0.4.8 setup-v0.4.9 setup-v0.4.10"

  # Everything that is not a setup release folder is left alone.
  plan 3 setup-v0.4.211 "${names[@]}" seed-cache agent-v0.4.211 tunnel-v0.4.192 vendored-ffmpeg-9.0.1 \
       setup-v0.4.5-rc.1 'setup-v0.4.5 copy' README.md setup-v
  eq "prune: only named setup folders" "$(echo $out | tr ' ' '\n' | grep -vc '^setup-v0\.4\.[0-9]*$')" 0
  eq "prune: count with strangers present" "$(echo $out | wc -w | tr -d ' ')" 10

  # The folder just published is never removed, even when it is not among the newest.
  plan 3 setup-v0.4.200 "${names[@]}"
  eq "prune: a republished old tag survives" "$(echo $out | tr ' ' '\n' | grep -cx 'setup-v0.4.200')" 0
  eq "prune: and the rest of the old ones go" "$(echo $out | wc -w | tr -d ' ')" 9

  # sftp's own output shape: an echo line, path prefixes, trailing slashes, CRs, duplicates.
  out="$(printf 'sftp> ls -1 /home/frs/project/x/\r\n/home/frs/project/x/setup-v0.4.1/\r\n/home/frs/project/x/setup-v0.4.2\r\n/home/frs/project/x/setup-v0.4.3\n/home/frs/project/x/setup-v0.4.4\nsetup-v0.4.4\n/home/frs/project/x/seed-cache\n' \
        | bash "$0" prune-plan 3 setup-v0.4.4)" && rc=0 || rc=$?
  eq "prune: sftp listing" "$out" setup-v0.4.1

  # Refusals: each of these would otherwise remove something on a wrong premise.
  plan 10 setup-v0.4.212 "${names[@]}"; eq "prune refuses when the current tag is not listed" "$rc" 2
  eq "prune prints nothing on that refusal" "$out" ''
  plan 10 setup-v0.4.211;               eq "prune refuses an empty listing" "$rc" 2
  plan 2 setup-v0.4.211 "${names[@]}";  eq "prune refuses a keep under the floor" "$rc" 2
  plan 0 setup-v0.4.211 "${names[@]}";  eq "prune refuses keep 0" "$rc" 2
  plan x setup-v0.4.211 "${names[@]}";  eq "prune refuses a keep that is not a number" "$rc" 2
  plan 10 seed-cache "${names[@]}";     eq "prune refuses a current tag of another family" "$rc" 2

  # --- listing-state ------------------------------------------------------------
  # stdin: what best_release.json names for windows, mac and linux, in that order.
  state() { out="$(printf '%s\n' "${@:2}" | bash "$0" listing-state "$1" 2>"$tmp/err")" && rc=0 || rc=$?; }
  local w211=/setup-v0.4.211/roomler-setup-0.4.211-x86_64-pc-windows-msvc.exe
  local m211=/setup-v0.4.211/roomler-setup-0.4.211-universal-apple-darwin.tar.gz
  local l211=/setup-v0.4.211/roomler-setup-0.4.211-x86_64-unknown-linux-gnu.tar.gz
  local w210=/setup-v0.4.210/roomler-setup-0.4.210-x86_64-pc-windows-msvc.exe
  local m210=/setup-v0.4.210/roomler-setup-0.4.210-universal-apple-darwin.tar.gz
  local l210=/setup-v0.4.210/roomler-setup-0.4.210-x86_64-unknown-linux-gnu.tar.gz

  state setup-v0.4.211 "$w211" "$m211" "$l211"; eq "listing-state: all three are this release's" "$out" current
  eq "listing-state: and that is exit 0" "$rc" 0
  state setup-v0.4.211 "$w210" "$m210" "$l210"; eq "listing-state: all three one release back" "$out" behind
  eq "listing-state: behind is exit 0 too" "$rc" 0
  state setup-v0.4.211 "$w210" "$m211" "$l211"; eq "listing-state: only windows back" "$out" behind
  state setup-v0.4.211 "$w211" "$m210" "$l211"; eq "listing-state: only mac back" "$out" behind
  state setup-v0.4.211 "$w211" "$m211" "$l210"; eq "listing-state: only linux back" "$out" behind
  state setup-v0.4.211 "$w211" "$m211" "";      eq "listing-state: a platform with no default" "$out" behind
  # The right folder is not enough: the button has to be the one default file.
  state setup-v0.4.211 /setup-v0.4.211/roomler-setup-0.4.211-x86_64-pc-windows-msvc.zip "$m211" "$l211"
  eq "listing-state: the zip is not the windows default" "$out" behind
  state setup-v0.4.211 "$w211" "$m211" /setup-v0.4.211/roomler-setup-0.4.211-aarch64-unknown-linux-gnu.tar.gz
  eq "listing-state: the aarch64 tarball is not the linux default" "$out" behind
  state setup-v0.4.211 "$w211" "$l211" "$m211"; eq "listing-state: the right files on the wrong platforms" "$out" behind
  state setup-v0.4.211 /agent-v0.4.211/roomler-desktop-0.4.211-x86_64-pc-windows-msvc.exe "$m211" "$l211"
  eq "listing-state: a file of another release family" "$out" behind
  out="$(printf '%s\r\n%s\r\n%s\r\n' "$w211" "$m211" "$l211" | bash "$0" listing-state setup-v0.4.211)" && rc=0 || rc=$?
  eq "listing-state: CR line ends" "$out" current
  # Version order, not text order: 0.4.9 is older than 0.4.10.
  state setup-v0.4.10 /setup-v0.4.9/roomler-setup-0.4.9-x86_64-pc-windows-msvc.exe \
        /setup-v0.4.10/roomler-setup-0.4.10-universal-apple-darwin.tar.gz \
        /setup-v0.4.10/roomler-setup-0.4.10-x86_64-unknown-linux-gnu.tar.gz
  eq "listing-state: 0.4.9 is behind 0.4.10" "$out" behind

  # Newer: a button already serves a later setup release than the tag asked about.
  state setup-v0.4.210 "$w211" "$m211" "$l211"; eq "listing-state: all three newer" "$out" newer
  eq "listing-state: newer is exit 0, a word and not a failure" "$rc" 0
  state setup-v0.4.210 "$w211" "$m210" "$l210"; eq "listing-state: only windows newer" "$out" newer
  state setup-v0.4.210 "$w210" "$m211" "$l210"; eq "listing-state: only mac newer" "$out" newer
  state setup-v0.4.210 "$w210" "$m210" "$l211"; eq "listing-state: only linux newer" "$out" newer
  # One newer and one older is still newer: the order of the lines must not matter.
  state setup-v0.4.210 "$w211" /setup-v0.4.209/roomler-setup-0.4.209-universal-apple-darwin.tar.gz "$l210"
  eq "listing-state: newer first, then older" "$out" newer
  state setup-v0.4.210 /setup-v0.4.209/roomler-setup-0.4.209-x86_64-pc-windows-msvc.exe "$m210" "$l211"
  eq "listing-state: older first, then newer" "$out" newer
  state setup-v0.4.9 /setup-v0.4.10/roomler-setup-0.4.10-x86_64-pc-windows-msvc.exe \
        /setup-v0.4.9/roomler-setup-0.4.9-universal-apple-darwin.tar.gz \
        /setup-v0.4.9/roomler-setup-0.4.9-x86_64-unknown-linux-gnu.tar.gz
  eq "listing-state: 0.4.10 is newer than 0.4.9" "$out" newer

  # Refusals: only what cannot be read.
  state setup-v0.4.211 "$w211" "$m211";                  eq "listing-state refuses two lines" "$rc" 2
  eq "listing-state prints nothing on a refusal" "$out" ''
  state setup-v0.4.211 "$w211" "$m211" "$l211" "$l211";  eq "listing-state refuses four lines" "$rc" 2
  state setup-v0.4.211;                                  eq "listing-state refuses one blank line" "$rc" 2
  out="$(bash "$0" listing-state setup-v0.4.211 < /dev/null 2>"$tmp/err")" && rc=0 || rc=$?
  eq "listing-state refuses an empty answer" "$rc" 2
  state seed-cache "$w211" "$m211" "$l211";              eq "listing-state refuses a tag of another family" "$rc" 2
  state seed-cache "" "" "";                             eq "listing-state refuses it whatever the listing serves" "$rc" 2
  state setup-v0.4.212-rc.1 "$w211" "$m211" "$l211";     eq "listing-state refuses a pre-release tag" "$rc" 2

  # --- reconcile-plan -----------------------------------------------------------
  # rplan <tag> <folders> <listing> [folder names...]: `out` is the plan word.
  rplan() {
    out="$(printf '%s\n' "${@:4}" | bash "$0" reconcile-plan "$1" "$2" "$3" 2>"$tmp/err")" && rc=0 || rc=$?
    why="$(printf '%s\n' "$out" | sed -n 2p)"
    out="$(printf '%s\n' "$out" | sed -n 1p)"
  }
  local why
  local held=(setup-v0.4.209 setup-v0.4.210 setup-v0.4.211)     # the release's folder is there
  local lacks=(setup-v0.4.209 setup-v0.4.210)                   # it is not

  # The file store decides.
  rplan setup-v0.4.211 ok current    "${lacks[@]}"; eq "reconcile: no folder, buttons current" "$out" publish
  rplan setup-v0.4.211 ok behind     "${lacks[@]}"; eq "reconcile: no folder, buttons behind" "$out" publish
  rplan setup-v0.4.211 ok unreadable "${lacks[@]}"; eq "reconcile: no folder, buttons unread" "$out" publish
  eq "reconcile: and it says why" "$why" "SourceForge has no folder for setup-v0.4.211"
  rplan setup-v0.4.211 ok unreadable;               eq "reconcile: an empty file store" "$out" publish
  rplan setup-v0.4.211 ok unreadable seed-cache agent-v0.4.211 README.md
  eq "reconcile: a file store with no setup folder" "$out" publish
  # With the folder there, the buttons add only what they can prove.
  rplan setup-v0.4.211 ok current    "${held[@]}"; eq "reconcile: folder there, buttons current" "$out" verified
  rplan setup-v0.4.211 ok behind     "${held[@]}"; eq "reconcile: folder there, buttons behind" "$out" publish
  eq "reconcile: behind says why" "$why" "the Download buttons do not serve setup-v0.4.211"
  rplan setup-v0.4.211 ok unreadable "${held[@]}"; eq "reconcile: folder there, buttons unread" "$out" nothing
  eq "reconcile: nothing is exit 0" "$rc" 0
  # Without the file store, the buttons decide; without either, it sends.
  rplan setup-v0.4.211 unreadable current;    eq "reconcile: files unread, buttons current" "$out" verified
  rplan setup-v0.4.211 unreadable behind;     eq "reconcile: files unread, buttons behind" "$out" publish
  rplan setup-v0.4.211 unreadable unreadable; eq "reconcile: nothing could be read" "$out" publish
  # stdin is not the file store when it could not be read: whatever is on it is ignored.
  rplan setup-v0.4.211 unreadable current setup-v0.4.212 setup-v0.4.213
  eq "reconcile: an unread file store is not consulted" "$out" verified
  # sftp's own output shape, and version order.
  out="$(printf 'sftp> ls -1 /home/frs/project/x/\r\n/home/frs/project/x/setup-v0.4.9/\r\n/home/frs/project/x/setup-v0.4.10\r\n' \
        | bash "$0" reconcile-plan setup-v0.4.10 ok current | sed -n 1p)" && rc=0 || rc=$?
  eq "reconcile: sftp listing, 0.4.10 after 0.4.9" "$out" verified

  # A release pulled back on GitHub: SourceForge is ahead, and nothing is sent.
  rplan setup-v0.4.210 ok current    "${held[@]}"; eq "reconcile refuses when a newer folder is there" "$rc" 2
  eq "reconcile prints no plan on that refusal" "$out" ''
  rplan setup-v0.4.210 ok behind     "${held[@]}"; eq "reconcile refuses it with the buttons behind too" "$rc" 2
  rplan setup-v0.4.210 ok unreadable "${held[@]}"; eq "reconcile refuses it with the buttons unread too" "$rc" 2
  rplan setup-v0.4.9 ok current setup-v0.4.9 setup-v0.4.10
  eq "reconcile refuses 0.4.10 as newer than 0.4.9" "$rc" 2
  rplan setup-v0.4.211 ok newer "${held[@]}";      eq "reconcile refuses a newer button" "$rc" 2
  rplan setup-v0.4.211 unreadable newer;           eq "reconcile refuses a newer button with the files unread" "$rc" 2
  # A newer folder of another family, or a pre-release one, is not a newer setup release.
  rplan setup-v0.4.211 ok current "${held[@]}" agent-v0.4.999 setup-v0.4.212-rc.1 tunnel-v9.9.9
  eq "reconcile: strangers are not newer releases" "$out" verified
  # Arguments it cannot read.
  rplan seed-cache ok current "${held[@]}";          eq "reconcile refuses a tag of another family" "$rc" 2
  rplan seed-cache unreadable current;               eq "reconcile refuses it with nothing else to trip on" "$rc" 2
  rplan setup-v0.4.212-rc.1 unreadable current;      eq "reconcile refuses a pre-release tag" "$rc" 2
  rplan setup-v0.4.211 yes current "${held[@]}";     eq "reconcile refuses an unknown folders word" "$rc" 2
  rplan setup-v0.4.211 ok stale "${held[@]}";        eq "reconcile refuses an unknown listing word" "$rc" 2
  rplan setup-v0.4.211 ok "" "${held[@]}";           eq "reconcile refuses an empty listing word" "$rc" 2

  if [ "$fail" -ne 0 ]; then
    echo "sourceforge-publish selftest: $fail of $((pass + fail)) checks FAILED" >&2
    return 1
  fi
  echo "sourceforge-publish selftest: $pass checks passed"
}

case "${1:-}" in
  valid-tag)     [ $# -eq 2 ] || die "usage: valid-tag <tag>";                  valid_tag "$2" ;;
  newest)        [ $# -eq 1 ] || die "usage: newest  (tag names on stdin)";     newest ;;
  default-for)   [ $# -eq 3 ] || die "usage: default-for <tag> <asset-name>";   default_for "$2" "$3" ;;
  defaults-plan) [ $# -eq 3 ] || die "usage: defaults-plan <tag> <dir>";        defaults_plan "$2" "$3" ;;
  verify)        [ $# -eq 3 ] || die "usage: verify <dir> <digests.tsv>";       verify "$2" "$3" ;;
  prune-plan)    [ $# -eq 3 ] || die "usage: prune-plan <keep> <current-tag>";  prune_plan "$2" "$3" ;;
  listing-state) [ $# -eq 2 ] || die "usage: listing-state <tag>  (three file names on stdin)"; listing_state "$2" ;;
  reconcile-plan)
    [ $# -eq 4 ] || die "usage: reconcile-plan <tag> <ok|unreadable> <current|behind|newer|unreadable>  (folder names on stdin)"
    reconcile_plan "$2" "$3" "$4" ;;
  --selftest)    selftest ;;
  *) die "usage: $0 valid-tag|newest|default-for|defaults-plan|verify|prune-plan|listing-state|reconcile-plan|--selftest (see the header)" ;;
esac
