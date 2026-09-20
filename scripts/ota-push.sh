#!/usr/bin/env bash
#
# ota-push.sh -- send a firmware image to a traffic light over the network.
#
# Usually reached through cargo:
#
#   cargo run --release -- 192.168.1.110    build, convert and push
#   cargo run --release -- ota              ...to the remembered host
#
# but it also works on its own, which is how you push an image built somewhere
# else without an esp toolchain installed:
#
#   scripts/ota-push.sh --image app.bin 192.168.1.110
#   scripts/ota-push.sh --check 192.168.1.110
#
# Options:
#   --elf FILE       ELF to convert with `espflash save-image` (cargo passes this)
#   --image FILE     push this .bin as it is, skipping espflash
#   --check          just report what the device is running, then stop
#   --dry-run        build and check everything, send nothing
#   --self-test      run the internal assertions and stop (no device needed)
#   -y, --yes        do not ask for confirmation
#   --no-verify      do not wait for the device to come back
#   --allow-debug    permit pushing a debug build
#   --user NAME      portal username (default: admin, or $TLD_OTA_USER)
#   --port N         http port (default 80)
#   --timeout SEC    upload timeout (default 300)
#
# Environment: TLD_OTA_HOST, TLD_OTA_USER, TLD_OTA_NETRC, ESPFLASH.
#
# The password is never taken from the command line and never put on one: it
# comes from ~/.netrc or from a prompt, and reaches curl through a config file
# on stdin, so it cannot show up in `ps` or in your shell history.

set -euo pipefail

# Exit codes are the contract scripts/test-ota-push.sh asserts against.
E_USAGE=1       # bad arguments
E_IMAGE=2       # missing, empty, truncated or not an application image
E_TOOBIG=3      # will not fit the slot
E_UNREACHABLE=4 # no answer, or not one of ours
E_REJECTED=5    # the device said no
E_VERIFY=6      # pushed, but it did not come back as expected
E_DECLINED=7    # cancelled, or no tty and no --yes

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(dirname -- "$HERE")"
ESPFLASH="${ESPFLASH:-espflash}"

# The first byte of every esp-idf application image, and the app descriptor
# magic at offset 0x20. The firmware checks both; checking them here too means
# the obvious mistakes never reach the wire.
IMAGE_MAGIC="e9"
APPDESC_MAGIC="3254cdab"
MIN_IMAGE=262144
HARD_MAX_IMAGE=$((4 * 1024 * 1024))

# `[ -r /dev/tty ]` is not the question: the node exists and is readable even
# when the process has no controlling terminal, and the redirect then fails at
# use time. Actually try to open it.
have_tty() { { : >/dev/tty; } 2>/dev/null; }

step() { printf '==> %s\n' "$*" >&2; }
info() { printf '    %s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    local code="$1"
    shift
    printf 'error: %s\n' "$*" >&2
    exit "$code"
}

# ---------------------------------------------------------------- pure helpers
# Kept free of I/O and globals so --self-test can exercise them.

# "0x1F0000" | "1984K" | "2M" | "1048576" -> bytes
to_bytes() {
    local v="${1:-}" mult=1
    case "$v" in
    *[kK]) mult=1024 v="${v%?}" ;;
    *[mM]) mult=$((1024 * 1024)) v="${v%?}" ;;
    esac
    [ -n "$v" ] || return 1
    printf '%d' "$((v * mult))" 2>/dev/null || return 1
}

# Smallest app partition in an esp-idf partitions.csv, in bytes.
min_app_slot() {
    local csv="$1" name type size best="" bytes
    [ -f "$csv" ] || return 1
    while IFS=, read -r name type _ _ size _; do
        name="${name//[[:space:]]/}"
        type="${type//[[:space:]]/}"
        size="${size//[[:space:]]/}"
        case "$name" in '' | \#*) continue ;; esac
        [ "$type" = "app" ] || continue
        bytes="$(to_bytes "$size")" || continue
        if [ -z "$best" ] || [ "$bytes" -lt "$best" ]; then best="$bytes"; fi
    done <"$csv"
    [ -n "$best" ] || return 1
    printf '%s' "$best"
}

# -v throughout: without it od collapses repeated lines to a single "*", which
# silently truncates any run of identical bytes.
file_size() { wc -c <"$1" | tr -d '[:space:]'; }
first_byte() { od -v -An -N1 -tx1 <"$1" | tr -d '[:space:]'; }
appdesc_magic() { od -v -An -j32 -N4 -tx1 <"$1" | tr -d '[:space:]'; }

# esp_app_desc_t.app_elf_sha256, which esp-idf stamps into the image and the
# device reports back through /version. It is what distinguishes one build from
# another: the version string does not change while you are iterating.
# Descriptor at image offset 0x20, sha256 144 bytes into it.
image_elf_sha() { od -v -An -j176 -N32 -tx1 <"$1" | tr -d '[:space:]'; }

# Everything that can be known about an image without a device. Prints why on
# failure and returns non-zero; the caller turns that into an exit code.
validate_image() {
    local f="$1" max="$2" size
    [ -f "$f" ] || {
        printf 'no image at %s\n' "$f"
        return 1
    }
    size="$(file_size "$f")"
    [ "$size" -gt 0 ] || {
        printf '%s is empty\n' "$f"
        return 1
    }
    [ "$(first_byte "$f")" = "$IMAGE_MAGIC" ] || {
        printf '%s does not start with 0x%s, so it is not an esp32 application image\n' \
            "$f" "$IMAGE_MAGIC"
        return 1
    }
    [ "$size" -ge "$MIN_IMAGE" ] || {
        printf '%s is only %s bytes -- truncated build?\n' "$f" "$size"
        return 1
    }
    [ "$(appdesc_magic "$f")" = "$APPDESC_MAGIC" ] || {
        printf '%s has no application descriptor -- a bootloader or a merged image?\n' "$f"
        return 1
    }
    [ "$size" -le "$max" ] || {
        printf '%s is %s bytes and will not fit the %s byte slot\n' "$f" "$size" "$max"
        return 2
    }
    return 0
}

http_hint() {
    case "$1" in
    200 | 201 | 202 | 204) printf 'accepted' ;;
    400) printf 'the device rejected the image as malformed' ;;
    401) printf 'wrong password' ;;
    403) printf 'the device refused; has a portal password been set on it?' ;;
    404) printf 'no such endpoint -- is the firmware on the device older than this script?' ;;
    409) printf 'someone else is pushing to it right now' ;;
    411) printf 'the device wants a Content-Length' ;;
    413) printf 'the image is too large for the slot on the device' ;;
    500 | 503) printf 'the device failed while writing the image' ;;
    000) printf 'no http response at all' ;;
    *) printf 'unexpected http status' ;;
    esac
}

curl_hint() {
    case "$1" in
    6) printf 'could not resolve that host name' ;;
    7) printf 'could not connect -- is the light powered on and on the network?' ;;
    28) printf 'timed out' ;;
    52) printf 'the device closed the connection without answering' ;;
    56) printf 'the connection dropped mid-transfer' ;;
    *) printf 'curl failed' ;;
    esac
}

# Minimal field reader, so this does not need jq installed.
json_field() {
    printf '%s' "$1" |
        sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*"\{0,1\}\([^",}]*\).*/\1/p' |
        head -n 1
}

# ------------------------------------------------------------------- self test
self_test() {
    local tmp fails=0
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064 # expand tmp now, not at trap time
    trap "rm -rf '$tmp'" EXIT

    check() {
        if [ "$2" = "$3" ]; then
            info "ok   $1"
        else
            info "FAIL $1: wanted [$2], got [$3]"
            fails=$((fails + 1))
        fi
    }

    check "to_bytes hex" 2031616 "$(to_bytes 0x1F0000)"
    check "to_bytes K" 2031616 "$(to_bytes 1984K)"
    check "to_bytes M" 2097152 "$(to_bytes 2M)"
    check "to_bytes plain" 4096 "$(to_bytes 4096)"

    cat >"$tmp/parts.csv" <<'CSV'
# Name,   Type, SubType, Offset,   Size,     Flags
nvs,      data, nvs,     0x9000,   0x6000,
ota_0,    app,  ota_0,   0x20000,  0x1F0000,
ota_1,    app,  ota_1,   0x210000, 0x1F0000,
CSV
    check "min_app_slot skips data partitions" 2031616 "$(min_app_slot "$tmp/parts.csv")"

    # A plausible image, and each way of being implausible.
    make_image() { # path size first_byte appdesc
    {
        printf '%b' "$3"
        dd if=/dev/zero bs=1 count=31 2>/dev/null
        printf '%b' "$4"
        dd if=/dev/zero bs=1024 count="$2" 2>/dev/null
    } >"$1"
    }
    make_image "$tmp/good.bin" 512 '\351' '\062\124\315\253'
    make_image "$tmp/badmagic.bin" 512 '\052' '\062\124\315\253'
    make_image "$tmp/nodesc.bin" 512 '\351' '\336\255\276\357'
    make_image "$tmp/tiny.bin" 1 '\351' '\062\124\315\253'
    : >"$tmp/empty.bin"

    try() {
        validate_image "$1" "$2" >/dev/null 2>&1 && printf 'pass' || printf '%s' "$?"
    }
    check "accepts a real-looking image" pass "$(try "$tmp/good.bin" 2031616)"
    check "rejects a wrong magic byte" 1 "$(try "$tmp/badmagic.bin" 2031616)"
    check "rejects a missing descriptor" 1 "$(try "$tmp/nodesc.bin" 2031616)"
    check "rejects a truncated image" 1 "$(try "$tmp/tiny.bin" 2031616)"
    check "rejects an empty image" 1 "$(try "$tmp/empty.bin" 2031616)"
    check "rejects a missing image" 1 "$(try "$tmp/nope.bin" 2031616)"
    # A padded or merged image is flash-sized, not app-sized: its own exit code
    # so the caller can say something more useful than "bad image".
    check "flags an oversize image separately" 2 "$(try "$tmp/good.bin" 65536)"

    # The descriptor hash the device reports lives 176 bytes into the image.
    {
        dd if=/dev/zero bs=1 count=176 2>/dev/null
        printf '%b' '\001\043\105\147'
        dd if=/dev/zero bs=1 count=28 2>/dev/null
    } >"$tmp/sha.bin"
    check "image_elf_sha reads offset 176" \
        "01234567$(printf '00%.0s' $(seq 1 28))" "$(image_elf_sha "$tmp/sha.bin")"

    check "401 hint" 'wrong password' "$(http_hint 401)"
    check "409 hint" 'someone else is pushing to it right now' "$(http_hint 409)"
    check "curl 7 hint" 'could not connect -- is the light powered on and on the network?' \
        "$(curl_hint 7)"
    check "json_field" '0.3.0' "$(json_field '{"name":"x","version":"0.3.0"}' version)"
    check "json_field on a number" '4210' \
        "$(json_field '{"uptime_ms":4210}' uptime_ms)"

    [ "$fails" -eq 0 ] || die "$E_USAGE" "$fails self-test check(s) failed"
    step "self-test passed"
}

# --------------------------------------------------------------- argument parse
HOST=""
ELF=""
IMAGE=""
PORT=80
TIMEOUT=300
CHECK_ONLY=0
DRY_RUN=0
ASSUME_YES=0
DO_VERIFY=1
ALLOW_DEBUG=0
OTA_USER="${TLD_OTA_USER:-admin}"
NETRC="${TLD_OTA_NETRC:-$HOME/.netrc}"

while [ "$#" -gt 0 ]; do
    case "$1" in
    --elf)
        ELF="${2:?--elf needs a path}"
        shift 2
        ;;
    --image)
        IMAGE="${2:?--image needs a path}"
        shift 2
        ;;
    --user)
        OTA_USER="${2:?--user needs a name}"
        shift 2
        ;;
    --port)
        PORT="${2:?--port needs a number}"
        shift 2
        ;;
    --timeout)
        TIMEOUT="${2:?--timeout needs seconds}"
        shift 2
        ;;
    --check)
        CHECK_ONLY=1
        shift
        ;;
    --dry-run)
        DRY_RUN=1
        shift
        ;;
    --no-verify)
        DO_VERIFY=0
        shift
        ;;
    --allow-debug)
        ALLOW_DEBUG=1
        shift
        ;;
    -y | --yes)
        ASSUME_YES=1
        shift
        ;;
    --self-test)
        self_test
        exit 0
        ;;
    -h | --help)
        sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    -*) die "$E_USAGE" "unknown option: $1" ;;
    *)
        [ -z "$HOST" ] || die "$E_USAGE" "more than one host given: $HOST and $1"
        HOST="$1"
        shift
        ;;
    esac
done

# "ota" and "" both mean "the remembered host".
if [ -z "$HOST" ] || [ "$HOST" = "ota" ]; then
    HOST="${TLD_OTA_HOST:-}"
    if [ -z "$HOST" ] && [ -f "$ROOT/.ota-host" ]; then
        HOST="$(tr -d '[:space:]' <"$ROOT/.ota-host")"
    fi
    [ -n "$HOST" ] || die "$E_USAGE" \
        "no host. Give one (cargo run --release -- 192.168.1.110), set TLD_OTA_HOST, or write it to .ota-host"
fi
BASE="http://$HOST:$PORT"

command -v curl >/dev/null || die "$E_USAGE" "curl is not installed"

# ----------------------------------------------------------------- credentials
OTA_PASS=""

# Escape for curl's config-file syntax.
cfg_escape() {
    local v="$1"
    v="${v//\\/\\\\}"
    printf '%s' "${v//\"/\\\"}"
}

# Credentials go to curl on stdin, never in argv where `ps` would show them.
curl_config() {
    if [ -n "$OTA_PASS" ]; then
        printf 'user = "%s:%s"\n' "$(cfg_escape "$OTA_USER")" "$(cfg_escape "$OTA_PASS")"
    elif [ -f "$NETRC" ]; then
        printf 'netrc-file = "%s"\nnetrc-optional\n' "$NETRC"
    fi
}

prompt_for_password() {
    have_tty || die "$E_REJECTED" \
        "$HOST wants a password and there is no terminal to ask on.
  Add an entry to $NETRC (and chmod 600 it):
      machine $HOST login $OTA_USER password <the password>"
    printf 'password for %s@%s: ' "$OTA_USER" "$HOST" >/dev/tty
    IFS= read -rs OTA_PASS </dev/tty
    printf '\n' >/dev/tty
    [ -n "$OTA_PASS" ] || die "$E_DECLINED" "no password given"
}

HTTP_CODE=""
CURL_RC=0
BODY=""

device_get() {
    local path="$1" out
    out="$(mktemp)"
    set +e
    HTTP_CODE="$(curl_config | curl --silent --show-error --config - \
        --connect-timeout 5 --max-time 15 \
        --write-out '%{http_code}' --output "$out" "$BASE$path" 2>/dev/null)"
    CURL_RC=$?
    set -e
    BODY="$(cat "$out")"
    rm -f "$out"
}

# GET, asking for a password once if the device wants one.
device_get_authed() {
    device_get "$1"
    if [ "$HTTP_CODE" = "401" ] && [ -z "$OTA_PASS" ]; then
        prompt_for_password
        device_get "$1"
    fi
    if [ "$HTTP_CODE" = "401" ]; then
        die "$E_REJECTED" "$(http_hint 401). To stop being asked, add to $NETRC:
      machine $HOST login $OTA_USER password <the password>"
    fi
}

# ----------------------------------------------------------------------- check
if [ "$CHECK_ONLY" -eq 1 ]; then
    step "asking $BASE what it is running"
    device_get_authed "/version"
    [ "$CURL_RC" -eq 0 ] || die "$E_UNREACHABLE" "$(curl_hint "$CURL_RC")"
    [ "$HTTP_CODE" = "200" ] || die "$E_UNREACHABLE" "$HTTP_CODE: $(http_hint "$HTTP_CODE")"
    printf '%s\n' "$BODY"
    exit 0
fi

# ----------------------------------------------------------------- the image
[ -n "$ELF$IMAGE" ] || die "$E_USAGE" "give --elf or --image (cargo passes --elf)"
[ -z "$ELF" ] || [ -z "$IMAGE" ] || die "$E_USAGE" "--elf and --image are mutually exclusive"

MAX_IMAGE="$(min_app_slot "$ROOT/partitions.csv" || true)"
if [ -z "$MAX_IMAGE" ]; then
    warn "could not read a slot size from partitions.csv; falling back to a ${HARD_MAX_IMAGE} byte ceiling"
    MAX_IMAGE="$HARD_MAX_IMAGE"
fi

if [ -n "$ELF" ]; then
    [ -f "$ELF" ] || die "$E_IMAGE" "no such ELF: $ELF"
    case "$ELF" in
    */debug/*)
        [ "$ALLOW_DEBUG" -eq 1 ] || die "$E_IMAGE" \
            "that is a debug build, which is large and slow on this device.
  Use: cargo run --release -- $HOST
  Or pass --allow-debug if you mean it."
        warn "pushing a debug build"
        ;;
    esac
    command -v "$ESPFLASH" >/dev/null || die "$E_USAGE" \
        "espflash is not installed (cargo install espflash --locked)"
    step "converting the ELF to an application image"
    mkdir -p "$ROOT/target/ota"
    IMAGE="$ROOT/target/ota/$(basename "$ELF").bin"
    # Without --merge this writes exactly one file, the app image, to the path
    # given. (--skip-padding is not accepted here; it only applies to --merge.)
    "$ESPFLASH" save-image --chip esp32 --flash-size 4mb "$ELF" "$IMAGE" >/dev/null 2>&1 ||
        die "$E_IMAGE" "espflash save-image failed"
fi

step "checking the image"
set +e
WHY="$(validate_image "$IMAGE" "$MAX_IMAGE")"
RC=$?
set -e
case "$RC" in
0) ;;
2) die "$E_TOOBIG" "$WHY" ;;
*) die "$E_IMAGE" "$WHY" ;;
esac

SIZE="$(file_size "$IMAGE")"
SENT_SHA="$(image_elf_sha "$IMAGE")"
PCT=$((SIZE * 100 / MAX_IMAGE))
info "$IMAGE: $SIZE bytes, ${PCT}% of the slot"
[ "$PCT" -lt 90 ] || warn "the image fills ${PCT}% of the slot -- headroom is running out"

NEW_VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"

# -------------------------------------------------------------------- preflight
step "asking $BASE what it is running"
device_get_authed "/version"
if [ "$CURL_RC" -ne 0 ]; then
    die "$E_UNREACHABLE" "$(curl_hint "$CURL_RC") ($BASE/version).
  If the light is in setup mode it is an access point at 192.168.71.1 instead."
fi
case "$HTTP_CODE" in
200) ;;
404) die "$E_UNREACHABLE" "$HOST answered but has no /version: $(http_hint 404)" ;;
*) die "$E_UNREACHABLE" "$HTTP_CODE from /version: $(http_hint "$HTTP_CODE")" ;;
esac

DEV_NAME="$(json_field "$BODY" name)"
OLD_VERSION="$(json_field "$BODY" version)"
OLD_SHA="$(json_field "$BODY" elf_sha256)"
[ "$DEV_NAME" = "traffic-light-display" ] || die "$E_UNREACHABLE" \
    "$HOST calls itself '$DEV_NAME', not traffic-light-display. Refusing to reflash it."
info "running $OLD_VERSION from $(json_field "$BODY" partition)"

if [ "$DRY_RUN" -eq 1 ]; then
    step "dry run: would post $SIZE bytes to $BASE/ota"
    exit 0
fi

if [ "$ASSUME_YES" -eq 0 ]; then
    have_tty || die "$E_DECLINED" "not a terminal; pass --yes to push without confirming"
    printf 'replace the firmware on %s (%s -> %s)? [y/N] ' \
        "$HOST" "${OLD_VERSION:-?}" "${NEW_VERSION:-?}" >/dev/tty
    IFS= read -r reply </dev/tty
    case "$reply" in
    y | Y | yes | YES) ;;
    *) die "$E_DECLINED" "cancelled" ;;
    esac
fi

# ----------------------------------------------------------------- the upload
step "uploading to $BASE/ota"
RESP="$(mktemp)"
# shellcheck disable=SC2064 # expand RESP now, not at trap time
trap "rm -f '$RESP'" EXIT

# No --silent: that would take the progress bar with it. 'Expect:' suppresses
# curl's 100-continue handshake, which esp-idf's http server does not answer and
# which otherwise costs a second on every push.
set +e
OUT="$(curl_config | curl --show-error --progress-bar --config - \
    --request POST \
    --header 'Content-Type: application/octet-stream' \
    --header 'Expect:' \
    --header "X-OTA-Pusher: ${USER:-someone}@$(uname -n)" \
    --data-binary "@$IMAGE" \
    --connect-timeout 5 --max-time "$TIMEOUT" \
    --write-out '%{http_code} %{size_upload}' --output "$RESP" "$BASE/ota")"
CURL_RC=$?
set -e
HTTP_CODE="${OUT%% *}"
UPLOADED="${OUT##* }"
REASON="$(head -c 300 "$RESP" 2>/dev/null || true)"

if [ "$CURL_RC" -ne 0 ]; then
    case "$CURL_RC" in
    28 | 52 | 56)
        if [ "${UPLOADED:-0}" -ge "$SIZE" ]; then
            # The device reboots into the new image ~1.5s after answering, so a
            # dropped connection after a complete upload is the normal race,
            # not a failure. Let the verification step settle it.
            warn "the whole image went up but the device stopped answering; it has probably rebooted"
        else
            die "$E_UNREACHABLE" \
                "connection lost after ${UPLOADED}/${SIZE} bytes. Nothing was committed on the device."
        fi
        ;;
    *) die "$E_UNREACHABLE" "$(curl_hint "$CURL_RC") (curl exit $CURL_RC)" ;;
    esac
else
    case "$HTTP_CODE" in
    200 | 201 | 202 | 204) step "the device accepted it${REASON:+ ($REASON)}" ;;
    *) die "$E_REJECTED" "$HTTP_CODE: $(http_hint "$HTTP_CODE")${REASON:+ -- $REASON}" ;;
    esac
fi

if [ "$DO_VERIFY" -eq 0 ]; then
    step "pushed; not waiting for it to come back"
    exit 0
fi

# ----------------------------------------------------------------- verification
step "waiting for $HOST to come back"
DEADLINE=$(($(date +%s) + 120))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    sleep 3
    device_get "/version"
    if [ "$CURL_RC" -ne 0 ] || [ "$HTTP_CODE" != "200" ]; then
        printf '.' >&2
        continue
    fi
    printf '\n' >&2
    GOT_VERSION="$(json_field "$BODY" version)"
    GOT_SHA="$(json_field "$BODY" elf_sha256)"
    GOT_SLOT="$(json_field "$BODY" partition)"
    if [ -n "$SENT_SHA" ] && [ -n "$GOT_SHA" ]; then
        # The image hash settles it outright, and it is the only thing that
        # does: version strings do not change while you are iterating, and
        # pushing the same build twice on purpose is a legitimate thing to do.
        if [ "$GOT_SHA" = "$SENT_SHA" ]; then
            step "up on $GOT_VERSION from $GOT_SLOT (image hash matches)"
            exit 0
        fi
        if [ "$GOT_SHA" = "$OLD_SHA" ]; then
            die "$E_VERIFY" "$HOST came back running the image it had before.
  It rolled back -- the new one did not do what the old one was doing.
  Read the serial log with: espflash monitor"
        fi
        die "$E_VERIFY" "$HOST came back running an image that is neither the
  one that was sent nor the one it had. Read the serial log."
    fi
    if [ -n "$NEW_VERSION" ] && [ "$GOT_VERSION" != "$NEW_VERSION" ]; then
        die "$E_VERIFY" "$HOST came back running $GOT_VERSION, expected $NEW_VERSION."
    fi
    step "up on $GOT_VERSION from $GOT_SLOT"
    exit 0
done

printf '\n' >&2
die "$E_VERIFY" "$HOST did not answer within 120s.
  It may still be rebooting, or it may have fallen back to setup mode.
  Check with: scripts/ota-push.sh --check $HOST"
