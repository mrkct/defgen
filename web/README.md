# The defgen playground

A page that runs `defgen` in the browser: write a schema, pick a backend, get
the same file the CLI would have written — <https://mrkct.github.io/defgen/>.

Nothing is uploaded. The compiler is compiled to WebAssembly and runs in the
tab, so a schema never leaves the machine it was typed on.

## Why this works at all

`defgen`'s front end and its backends are a pure function from a string to a
string: no files are read, no clock is consulted, no environment is inspected.
Only `src/main.rs` touches the outside world. So the library compiles for
`wasm32-unknown-unknown` unchanged, and what this directory adds is a boundary
around it, not a second implementation.

That is worth keeping true. `web/check.mjs` generates every example schema
with every backend twice — once through the wasm module and once by running
the binary — and fails if a single byte differs, diagnostics included. CI runs
it on every pull request.

## Layout

| Path | What it is |
|------|------------|
| `wasm/` | A `cdylib` wrapping the compiler in a handful of `extern "C"` entry points. Its own workspace, so it stays out of `cargo test` at the root. |
| `site/` | The page: hand-written HTML, CSS and ES modules. No bundler, no dependencies, no build step of its own. |
| `site/defgen.js` | The JavaScript half of the wasm ABI. |
| `site/app.js` | The UI — editor, options, result panels. |
| `build.sh` | Builds the module into `site/`, which is then a complete static site. |
| `check.mjs` | The wasm-against-CLI conformance check described above. |

## Building it

```sh
rustup target add wasm32-unknown-unknown   # once
./web/build.sh --serve                     # http://localhost:8000
```

`build.sh` writes two files into `site/` that are deliberately not committed:
`defgen.wasm`, and `examples/hearing-aid.defs` — a copy of the spec's worked
example at `tests/examples/commands.defs`, so the playground offers it without
keeping a second copy that could drift from the original.

A file server is not optional even for a local look: a page opened as
`file://` is not allowed to instantiate WebAssembly by URL.

To run the conformance check:

```sh
cargo build --release && ./web/build.sh && node web/check.mjs
```

## The wasm boundary

Four exports, documented in full in `wasm/src/lib.rs`:

| Export | Purpose |
|--------|---------|
| `defgen_alloc(len) -> ptr` | Space for an argument, or for a result. |
| `defgen_free(ptr, len)` | Releases either. |
| `defgen_backends() -> ptr` | The backend registry, as JSON. |
| `defgen_compile(src, backend, stem, flags) -> ptr` | One compilation, as JSON. |

Strings cross as `(pointer, length)` pairs of UTF-8 bytes; results come back
as a little-endian `u32` length followed by that many bytes of UTF-8, because
a wasm function returns one value. Both sides free with the length they
allocated.

There is no `wasm-bindgen` in that list, and that is the point: the interface
is two strings in and one string out, so the whole boundary is about eighty
lines of Rust and forty of JavaScript, and building the site needs nothing
beyond a Rust toolchain with the `wasm32-unknown-unknown` target — no CLI to
install, no generated glue to keep in step with the compiler that produced it.

## Deployment

`.github/workflows/pages.yml` builds `site/` and publishes it to GitHub Pages
on every push to `master`. The Pages source has to be set to *GitHub Actions*
once, under Settings → Pages; the workflow asks for that itself, but a
repository whose token cannot change the setting needs it done by hand.
