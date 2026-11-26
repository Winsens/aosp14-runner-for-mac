#!/bin/sh
# Cargo runner (see .cargo/config.toml): ad-hoc sign the freshly built binary
# with the Virtualization.framework entitlement, then exec it.
set -e
here="$(cd "$(dirname "$0")/.." && pwd)"
bin="$1"
shift
if ! out="$(codesign --force --sign - --entitlements "$here/entitlements.plist" "$bin" 2>&1)"; then
    echo "codesign failed: $out" >&2
    exit 1
fi
exec "$bin" "$@"
