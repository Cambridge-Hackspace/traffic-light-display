#!/usr/bin/env bash
#
# Cargo `runner` for the xtensa-esp32-espidf target. Cargo invokes it as
#
#   cargo-runner.sh <path-to-the-linked-elf> [whatever followed `--`]
#
# so the dispatch is:
#
#   cargo run --release                    -> serial flash + monitor, as always
#   cargo run --release -- 192.168.1.110   -> push over the network
#   cargo run --release -- ota             -> push to the remembered default host
#
# Everything after the host is forwarded to ota-push.sh.

set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname -- "$here")"

elf="${1:?internal error: cargo did not pass a binary path}"
shift

if [ "$#" -gt 0 ]; then
    if [ ! -x "$here/ota-push.sh" ]; then
        printf 'error: %s is missing, so there is no way to push over the network.\n' \
            "$here/ota-push.sh" >&2
        printf '       Run `cargo run --release` with no arguments to flash over USB.\n' >&2
        exit 1
    fi
    exec "$here/ota-push.sh" --elf "$elf" "$@"
fi

# ---------------------------------------------------------------- serial path
# Three of these flags are load-bearing and none of them is a default:
#
#   --partition-table  espflash otherwise writes its own generated single-app
#                      table, which has no ota_0/ota_1 and no otadata, and then
#                      nothing on the device can ever do an OTA update.
#   --bootloader       espflash otherwise writes its own bundled bootloader,
#                      built with stock config. Rollback is a *bootloader*
#                      state machine (NEW -> PENDING_VERIFY -> ABORTED), so with
#                      the stock one CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE is a
#                      no-op and nothing warns you until the day it matters.
#   --erase-parts      after an OTA the boot pointer names ota_1, but a serial
#                      flash writes ota_0. Without clearing otadata the device
#                      would come back up running the *old* image it just
#                      replaced. Erasing it takes the bootloader's "no factory
#                      image, trying OTA 0" path.
#
# esp-idf-sys drops bootloader.bin next to the ELF, so the profile directory
# never has to be named here.
out="$(dirname -- "$elf")"

args=(flash --monitor --flash-size 4mb --partition-table "$root/partitions.csv")

if [ -f "$out/bootloader.bin" ]; then
    args+=(--bootloader "$out/bootloader.bin")
else
    printf 'warning: %s not found; espflash will write its own bootloader and\n' \
        "$out/bootloader.bin" >&2
    printf '         firmware rollback will silently not work.\n' >&2
fi

exec espflash "${args[@]}" --erase-parts otadata "$elf"
