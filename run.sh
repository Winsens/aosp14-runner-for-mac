#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${ROOT}"

BIN="./target/release/aosp14-runner"
if [[ ! -x "${BIN}" ]]; then
    echo "[run.sh] Binary not found, building with make host..."
    make host
elif ! codesign -d --entitlements - --xml "${BIN}" 2>/dev/null | grep -q com.apple.security.virtualization; then
    echo "[run.sh] Binary lacks the virtualization entitlement, re-signing with make host..."
    make host
fi

ADB_PORT="15555"
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    case "${args[$i]}" in
        --adb-port)
            if ((i + 1 < ${#args[@]})); then
                ADB_PORT="${args[$((i + 1))]}"
            fi
            ;;
        --adb-port=*)
            ADB_PORT="${args[$i]#--adb-port=}"
            ;;
    esac
done

ADB_HELPER_PID=""
cleanup() {
    if [[ -n "${ADB_HELPER_PID}" ]]; then
        kill "${ADB_HELPER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if command -v adb >/dev/null 2>&1; then
    (
        adb disconnect "127.0.0.1:${ADB_PORT}" >/dev/null 2>&1 || true
        for _ in $(seq 1 90); do
            sleep 1
            if adb connect "127.0.0.1:${ADB_PORT}" >/dev/null 2>&1; then
                if [[ "$(adb -s "127.0.0.1:${ADB_PORT}" get-state 2>/dev/null || true)" == "device" ]]; then
                    echo "[run.sh] ADB connected: 127.0.0.1:${ADB_PORT}"
                    exit 0
                fi
            fi
        done
        echo "[run.sh] Warning: ADB did not become ready on 127.0.0.1:${ADB_PORT} within 90s" >&2
    ) &
    ADB_HELPER_PID=$!
fi

"${BIN}" run "$@"
