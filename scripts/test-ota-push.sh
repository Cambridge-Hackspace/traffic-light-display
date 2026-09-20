#!/usr/bin/env bash
#
# End-to-end tests for ota-push.sh against a stub device.
#
# ota-push.sh --self-test covers the pure helpers. This covers the part that
# talks: which exit code each kind of refusal produces, and that a push which
# comes back as the wrong image is reported as a failure rather than a success.
# Needs bash, curl and python3; no hardware, no esp toolchain, no network.

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PUSH="$HERE/ota-push.sh"
PORT="${TEST_PORT:-8099}"
HOST=127.0.0.1

TMP="$(mktemp -d)"
STUB_PID=""
cleanup() {
    [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

fails=0
check() { # check <name> <expected-exit> <cmd...>
    local name="$1" want="$2" got
    shift 2
    set +e
    "$@" >"$TMP/out" 2>&1
    got=$?
    set -e
    if [ "$got" = "$want" ]; then
        printf 'ok   %s\n' "$name"
    else
        printf 'FAIL %s: wanted exit %s, got %s\n' "$name" "$want" "$got"
        sed 's/^/       /' "$TMP/out"
        fails=$((fails + 1))
    fi
}

# --- fixtures: a plausible image, and the ways of being implausible ----------
make_image() { # path kilobytes first_byte appdesc_bytes
    {
        printf '%b' "$3"
        dd if=/dev/zero bs=1 count=31 2>/dev/null
        printf '%b' "$4"
        dd if=/dev/zero bs=1024 count="$2" 2>/dev/null
    } >"$1"
}
GOOD_DESC='\062\124\315\253'
make_image "$TMP/good.bin" 512 '\351' "$GOOD_DESC"
make_image "$TMP/badmagic.bin" 512 '\052' "$GOOD_DESC"
make_image "$TMP/tiny.bin" 1 '\351' "$GOOD_DESC"

# --- the stub device --------------------------------------------------------
cat >"$TMP/stub.py" <<'PY'
import os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

STATUS = int(os.environ.get("STUB_STATUS", "200"))
NAME = os.environ.get("STUB_NAME", "traffic-light-display")
BEFORE = os.environ.get("STUB_VERSION", "0.0.1")
AFTER = os.environ.get("STUB_VERSION_AFTER", BEFORE)
SHA_BEFORE = os.environ.get("STUB_SHA", "aa" * 32)
# A stub that "rolls back" keeps reporting the image it already had, however
# many times you push to it.
ROLLBACK = os.environ.get("STUB_ROLLBACK", "")
NEED_AUTH = os.environ.get("STUB_AUTH", "")

state = {"pushed": False, "sha": SHA_BEFORE}

class H(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        raw = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(raw)))
        if code == 401:
            self.send_header("WWW-Authenticate", 'Basic realm="stub"')
        self.end_headers()
        self.wfile.write(raw)

    def _authed(self):
        if not NEED_AUTH:
            return True
        return self.headers.get("Authorization") == NEED_AUTH

    def do_GET(self):
        if not self._authed():
            return self._send(401, "no")
        version = AFTER if state["pushed"] else BEFORE
        slot = "ota_1" if state["pushed"] else "ota_0"
        self._send(200, '{"name":"%s","version":"%s","partition":"%s",'
                        '"elf_sha256":"%s","uptime_ms":1200}'
                        % (NAME, version, slot, state["sha"]))

    def do_POST(self):
        if not self._authed():
            return self._send(401, "no")
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if STATUS in (200, 202):
            state["pushed"] = True
            if not ROLLBACK:
                # Report back the hash esp-idf would have found in the image
                # that was just written: descriptor at 0x20, sha 144 bytes in.
                state["sha"] = body[176:208].hex()
        self._send(STATUS, "ok", "text/plain")

    def log_message(self, *a):
        pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY

start_stub() {
    [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null
    python3 "$TMP/stub.py" "$PORT" &
    STUB_PID=$!
    for _ in $(seq 1 50); do
        if curl -s -o /dev/null "http://$HOST:$PORT/version"; then return; fi
        sleep 0.1
    done
    echo "stub server did not start" >&2
    exit 1
}

push() { "$PUSH" --image "$1" --port "$PORT" --no-verify --yes "${@:2}" "$HOST"; }

# --- image checks happen before anything is sent ----------------------------
# No stub running yet, which is the point: these must not need one.
check "a wrong magic byte is refused"   2 push "$TMP/badmagic.bin"
check "a truncated image is refused"    2 push "$TMP/tiny.bin"
check "a missing image is refused"      2 push "$TMP/nope.bin"
check "an unreachable device"           4 "$PUSH" --image "$TMP/good.bin" \
    --port 1 --yes --no-verify "$HOST"

# --- the happy path ---------------------------------------------------------
export STUB_VERSION=0.0.1 STUB_VERSION_AFTER=0.0.1
start_stub
check "a good image is accepted"        0 push "$TMP/good.bin"
check "dry run sends nothing"           0 "$PUSH" --image "$TMP/good.bin" \
    --port "$PORT" --dry-run "$HOST"
check "--check reports what is running" 0 "$PUSH" --check --port "$PORT" "$HOST"

# --- refusing to push to something that is not ours -------------------------
kill "$STUB_PID" 2>/dev/null; STUB_PID=""
STUB_NAME="some-other-device" start_stub
check "refuses a device that is not ours" 4 push "$TMP/good.bin"

# --- the device says no -----------------------------------------------------
for status in 400 409 413 500; do
    kill "$STUB_PID" 2>/dev/null; STUB_PID=""
    STUB_STATUS="$status" start_stub
    check "http $status is reported as a refusal" 5 push "$TMP/good.bin"
done

# --- authentication ---------------------------------------------------------
kill "$STUB_PID" 2>/dev/null; STUB_PID=""
# base64("admin:hunter2")
STUB_AUTH="Basic YWRtaW46aHVudGVyMg==" start_stub
cat >"$TMP/netrc" <<EOF
machine $HOST login admin password hunter2
EOF
chmod 600 "$TMP/netrc"
check "the right password gets in" 0 env TLD_OTA_NETRC="$TMP/netrc" \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" --yes --no-verify "$HOST"
cat >"$TMP/wrong-netrc" <<EOF
machine $HOST login admin password wrong
EOF
chmod 600 "$TMP/wrong-netrc"
# No tty in CI, so a wrong password cannot be re-prompted: it must fail rather
# than hang waiting for input.
check "a wrong password is rejected, not retried forever" 5 \
    env TLD_OTA_NETRC="$TMP/wrong-netrc" \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" --yes --no-verify "$HOST" </dev/null

# --- verification ------------------------------------------------------------
kill "$STUB_PID" 2>/dev/null; STUB_PID=""
start_stub
check "a device running what was sent is a success" 0 \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" --yes "$HOST"

# Pushing a build the device is already running is a legitimate thing to do --
# recovering a device, or re-confirming one -- and must not be read as a
# rollback just because nothing changed.
kill "$STUB_PID" 2>/dev/null; STUB_PID=""
STUB_SHA="$(od -v -An -j176 -N32 -tx1 <"$TMP/good.bin" | tr -d '[:space:]')" start_stub
check "re-pushing the image it already runs is fine" 0 \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" --yes "$HOST"

# A device that keeps reporting the image it had is what a rollback looks like
# from out here, and it must not be called a success.
kill "$STUB_PID" 2>/dev/null; STUB_PID=""
STUB_ROLLBACK=1 start_stub
check "a rollback is reported as a failure" 6 \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" --yes "$HOST"

# --- refusing to run unattended without --yes -------------------------------
check "no tty and no --yes is a refusal" 7 \
    "$PUSH" --image "$TMP/good.bin" --port "$PORT" "$HOST" </dev/null

if [ "$fails" -ne 0 ]; then
    printf '\n%s test(s) failed\n' "$fails"
    exit 1
fi
printf '\nall ota-push tests passed\n'
