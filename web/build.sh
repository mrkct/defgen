#!/usr/bin/env bash
# Builds the playground's wasm module into site/, which is then a complete
# static site: no bundler, no npm, nothing to serve it but a file server.
#
#   ./web/build.sh          build
#   ./web/build.sh --serve  build, then serve it on http://localhost:8000
#
# Needs the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
set -euo pipefail

web="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="${CARGO_TARGET_DIR:-$web/wasm/target}"

cargo build \
    --manifest-path "$web/wasm/Cargo.toml" \
    --target wasm32-unknown-unknown \
    --release \
    --locked

cp "$target/wasm32-unknown-unknown/release/defgen_wasm.wasm" "$web/site/defgen.wasm"
echo "built $web/site/defgen.wasm ($(wc -c <"$web/site/defgen.wasm") bytes)"

# The playground's third example is the spec's worked example. Copying it is
# what keeps it from becoming a second copy that drifts from the original.
cp "$web/../tests/examples/commands.defs" "$web/site/examples/hearing-aid.defs"

if [[ "${1:-}" == "--serve" ]]; then
    echo "serving http://localhost:8000 — ctrl-c to stop"
    # A file:// page cannot instantiate wasm by URL, so a server is not
    # optional even for a local look.
    python3 -m http.server 8000 --directory "$web/site"
fi
