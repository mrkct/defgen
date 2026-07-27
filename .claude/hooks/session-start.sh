#!/bin/bash
# Warms up the two things a fresh Claude Code on the web container doesn't
# have that this repo's own build/test/CI need:
#
#   - the wasm32-unknown-unknown Rust target, required by web/build.sh and
#     the "playground (wasm)" CI job, but not installed by default;
#   - the cargo build cache for both the root crate and the standalone
#     web/wasm crate, so the first real `cargo build`/`cargo test`/
#     `./web/build.sh` of the session doesn't pay full compile time.
#
# Everything else CI needs (a stable Rust toolchain with rustfmt/clippy,
# Node, a JDK for the Java backend's conformance test) is already present in
# this environment. Only runs in Claude Code on the web, per the skill's
# convention — a local `claude` session on a developer's own machine
# presumably already has its Rust toolchain set up the way it wants it.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "$CLAUDE_PROJECT_DIR"

rustup target add wasm32-unknown-unknown

cargo build
cargo build --manifest-path web/wasm/Cargo.toml --target wasm32-unknown-unknown
