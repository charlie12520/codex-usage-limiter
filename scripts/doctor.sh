#!/usr/bin/env sh
set -u

STRICT=0
if [ "${1:-}" = "--strict" ]; then
  STRICT=1
fi

# The whisper/cpal dictation stack (which needed CMake) is gone; building now
# only needs Node and a stable Rust toolchain.
missing=""
if ! command -v cargo >/dev/null 2>&1; then
  missing="rust (cargo)"
fi

if [ -z "$missing" ]; then
  echo "Doctor: OK"
  exit 0
fi

echo "Doctor: missing dependencies: $missing"
echo "Install Rust from: https://rustup.rs/"

if [ "$STRICT" -eq 1 ]; then
  exit 1
fi

exit 0
