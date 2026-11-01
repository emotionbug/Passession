#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec cargo --manifest-path "$SCRIPT_DIR/../Cargo.toml" run --package paseo-xtask -- dev daemon --watch "$@"
