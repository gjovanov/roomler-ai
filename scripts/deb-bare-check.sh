#!/usr/bin/env bash
# Install finished .deb packages into bare images, then start what they ship (#3204).
#
# A package can install cleanly and still not start, and only the real package
# on a bare image shows it:
#   - `apt-get install` exits 0 whichever provider it picks for a virtual
#     package. On Ubuntu 24.04 `libasound2` went to a stub without the symbols
#     the daemon binds at load, and every desktop image already had the real
#     library, so nothing noticed.
#   - A hand-written package list proves nothing about `depends`: the lane's
#     smoke step installed `libasound2t64` by name and passed.
#   - A Recommends can carry a library a Depends forgot. `libssl3` hid that way
#     on Debian 12, so this installs with --no-install-recommends: `dpkg -i`
#     plus a dependency fix, which is how install.sh runs, skips them too.
#
# Usage: scripts/deb-bare-check.sh [--image IMAGE]... [--platform P] [--install-sh SCRIPT] DEB...
#   --image       an image to check, repeatable. Default: every base we support.
#   --platform    docker's --platform, for an emulated arch on a dev box.
#   --install-sh  also drive this install.sh over the daemon's package, on an
#                 image with no package lists, as a fresh container has.
#
# Exits non-zero when any image fails. docs/installation.md has what each part
# of the daemon's `depends` is for.
set -euo pipefail

default_images=(ubuntu:22.04 ubuntu:24.04 ubuntu:26.04 debian:12 debian:13)

apt_retry=(-o Acquire::Retries=3)

# Everything a package ships must load and start, on the image it landed on.
start_package() {
    local pkg=$1 f out missing
    [ "$(dpkg-query -W -f='${db:Status-Abbrev}' "$pkg")" = "ii " ] || {
        echo "FAIL $pkg is not fully installed: $(dpkg-query -W -f='${db:Status-Abbrev}' "$pkg")"
        return 1
    }
    for f in $(dpkg -L "$pkg"); do
        [ -f "$f" ] && [ "$(head -c 4 "$f" | tail -c 3)" = ELF ] || continue
        # ldd's exit says nothing either way, so only its text is read. `not found`
        # also catches "version `GLIBC_2.39' not found".
        out=$(ldd "$f" 2>&1 || true)
        if missing=$(printf '%s\n' "$out" | grep 'not found'); then
            echo "FAIL $f: $missing"
            return 1
        fi
    done
    case "$pkg" in
        roomlerd)
            # Bound at load (BIND_NOW), so a library missing a symbol fails here.
            roomlerd --version
            roomler --version
            roomlerd encoder-smoke --encoder software >/tmp/smoke.log 2>&1 || {
                tail -20 /tmp/smoke.log
                echo "FAIL roomlerd encoder-smoke"
                return 1
            }
            ;;
        roomler-cli)
            roomler --version
            ;;
        # roomler-desktop is a GTK app and needs a display to get past main, so
        # the loader check above is all a bare image can ask of it.
    esac
}

# In the container: the packages through apt, Depends and nothing else.
in_image_apt() {
    local d
    export DEBIAN_FRONTEND=noninteractive
    apt-get "${apt_retry[@]}" update -qq
    apt-get "${apt_retry[@]}" install -y -qq --no-install-recommends "$@" >/tmp/apt.log 2>&1 || {
        tail -30 /tmp/apt.log
        echo "FAIL apt-get install"
        return 1
    }
    dpkg-query -W -f='  ${Package} ${Version}\n' 2>/dev/null | grep -E 'asound|oss4-salsa|libssl' || true
    for d in "$@"; do
        start_package "$(dpkg-deb -f "$d" Package)"
    done
}

# In the container: the real install.sh, against a stand-in for the release
# listing, on an image whose package lists are gone. curl and python3 are its
# needs and the stand-in's, and they come before the lists go.
in_image_install_sh() {
    local deb=$1 name url digest tries=0
    export DEBIAN_FRONTEND=noninteractive
    apt-get "${apt_retry[@]}" update -qq
    apt-get "${apt_retry[@]}" install -y -qq --no-install-recommends curl ca-certificates python3 >/dev/null 2>&1
    rm -rf /var/lib/apt/lists/*

    name="roomlerd-0.0.0-$(uname -m)-unknown-linux-gnu.deb"
    url=http://127.0.0.1:8099
    digest=$(sha256sum "$deb" | cut -d' ' -f1)
    mkdir -p /tmp/site/api/agent
    cp "$deb" "/tmp/site/$name"
    printf '{"assets":[{"name":"%s","browser_download_url":"%s/%s","digest":"sha256:%s"}]}' \
        "$name" "$url" "$name" "$digest" >/tmp/site/api/agent/latest-release
    (cd /tmp/site && exec python3 -m http.server 8099 --bind 127.0.0.1) >/dev/null 2>&1 &
    until curl -fs -o /dev/null "$url/api/agent/latest-release"; do
        tries=$((tries + 1))
        [ "$tries" -lt 50 ] || { echo "FAIL the stand-in never answered"; return 1; }
        sleep 0.2
    done

    sh /install.sh --role daemon --no-enroll --no-desktop --server "$url" >/tmp/install.log 2>&1 || {
        tail -30 /tmp/install.log
        echo "FAIL install.sh"
        return 1
    }
    start_package roomlerd
}

case "${1:-}" in
    --in-image-apt) shift; in_image_apt "$@"; exit ;;
    --in-image-install-sh) shift; in_image_install_sh "$@"; exit ;;
esac

# On the host: one container per check, in parallel, each log printed whole.
images=() platform=() install_sh=""
while [ $# -gt 0 ]; do
    case "$1" in
        --image) images+=("$2"); shift 2 ;;
        --platform) platform=(--platform "$2"); shift 2 ;;
        --install-sh) install_sh=$2; shift 2 ;;
        --*) echo "unknown option: $1" >&2; exit 2 ;;
        *) break ;;
    esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--image IMAGE]... [--platform P] [--install-sh SCRIPT] DEB..." >&2; exit 2; }
[ ${#images[@]} -gt 0 ] || images=("${default_images[@]}")

# Git Bash hands docker a Windows path and must leave the container's alone.
export MSYS_NO_PATHCONV=1
host_path() {
    local abs
    abs=$(cd "$(dirname "$1")" && printf '%s/%s' "$PWD" "$(basename "$1")")
    if command -v cygpath >/dev/null 2>&1; then cygpath -w "$abs"; else printf '%s' "$abs"; fi
}

stage=$(mktemp -d)
logs=$(mktemp -d)
trap 'rm -rf "$stage" "$logs"' EXIT
debs=() daemon=""
for d in "$@"; do
    cp "$d" "$stage/"
    debs+=("/debs/$(basename "$d")")
    case "$(basename "$d")" in roomlerd[-_]*) daemon="/debs/$(basename "$d")" ;; esac
done
[ -z "$install_sh" ] || [ -n "$daemon" ] || { echo "--install-sh needs the roomlerd package" >&2; exit 2; }

mounts=(-v "$(host_path "$stage/."):/debs:ro" -v "$(host_path "$0"):/check.sh:ro")
[ -z "$install_sh" ] || mounts+=(-v "$(host_path "$install_sh"):/install.sh:ro")

run() { # <log> <image> <args for this script in the container>
    local log=$1 image=$2 tries
    shift 2
    for tries in 1 2 3; do
        docker pull -q "${platform[@]}" "$image" >/dev/null 2>&1 && break
        sleep 5
    done
    docker run --rm "${platform[@]}" "${mounts[@]}" "$image" bash /check.sh "$@" >"$log" 2>&1
}

names=() pids=()
start() { # <what> <image> <args>
    local n=${#names[@]}
    names+=("$1 $2")
    run "$logs/$n" "$2" "${@:3}" &
    pids+=($!)
}
for image in "${images[@]}"; do
    start apt "$image" --in-image-apt "${debs[@]}"
    [ -z "$install_sh" ] || start install.sh "$image" --in-image-install-sh "$daemon"
done

summary=() failed=0
for i in "${!pids[@]}"; do
    if wait "${pids[$i]}"; then result=ok; else result=FAILED; failed=$((failed + 1)); fi
    echo "::group::${names[$i]}: $result"
    cat "$logs/$i"
    echo "::endgroup::"
    summary+=("$result: ${names[$i]}")
done

echo
printf '%s\n' "${summary[@]}"
if [ "$failed" -gt 0 ]; then
    echo "::error::$failed bare-image check(s) failed"
    exit 1
fi
