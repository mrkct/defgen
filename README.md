# defgen

`defgen` compiles a single `.defs` schema into matching, type-safe codec code
for reading and writing the fixed-width binary values carried over BLE GATT
characteristics — C, Java, JavaScript, Kotlin, Python and Swift, all
generated from the same source of truth.

Firmware and its client apps agree on a wire format; they rarely agree on how
it's implemented. Hand-written encode/decode logic for the same bit-packed
struct, tagged union, or scaled sensor value tends to drift across languages —
an offset changes on one platform and not the others, a new enum case is
handled on iOS but forgotten on Android. `defgen` removes the drift: you
describe the layout once, and every backend is generated from the same
checked model, so they can only ever agree with each other.

**[Try it in the browser](https://mrkct.github.io/defgen/)** — the compiler
runs as WebAssembly, so you can write a schema and read the generated code
for all six languages without installing anything, and without a schema
leaving your machine. If the schema binds a GATT service, its Device tab can
also connect to a real BLE peripheral with Web Bluetooth and read/write/notify
its characteristics live, through that same generated code. See
[`web/`](web/) for how it is built.

See [`SPEC.md`](SPEC.md) for the full language specification and
[`GRAMMAR.ebnf`](GRAMMAR.ebnf) for its grammar. The worked example referenced
throughout the spec lives at
[`tests/examples/commands.defs`](tests/examples/commands.defs).

## Supported backends

| Backend    | `--backend`  | Output |
|------------|--------------|--------|
| C          | `c`          | a single self-contained C99 header |
| Python     | `python`     | a single self-contained, type-hinted module (Python 3.10+) |
| JavaScript | `javascript` | a single self-contained, JSDoc-typed ES module (ES2022+) |
| Java       | `java`       | a single self-contained Java file (Java 17+) |
| Kotlin     | `kotlin`     | a single self-contained Kotlin file (JVM, Kotlin 1.9+) |
| Swift      | `swift`      | a single self-contained Swift file (Swift 6+) |

Every backend generates from the same checked model, so bit offsets, enum
values, byte order and variable-length handling are identical across all six
— see §13 of `SPEC.md` for the cross-backend conformance guarantee.

## What a schema looks like

```defs
endian: little;

---

/// Bulb brightness, 0-100.
alias Brightness = u8;

enum PowerState: u1 {
    Off = 0,
    On = 1,
}

/// State pushed via BLE notify.
struct LightState: u16 {
    brightness: Brightness,
    power: PowerState,
    reserved bits: u7,
}

service Light(uuid: "0000ffe0-0000-1000-8000-00805f9b34fb") {
    characteristic StateChar(
        uuid: "0000ffe1-0000-1000-8000-00805f9b34fb",
        properties: [read, notify],
    ): LightState;
}
```

An optional `endian` header, then type declarations (aliases, enums, structs,
scaled values) and `service`/`characteristic` bindings — everything needed to
derive an exact bit layout at compile time.

## Installation

Requires a stable Rust toolchain.

```sh
git clone https://github.com/mrkct/defgen.git
cd defgen
cargo install --path .
```

This installs a `defgen` binary. Alternatively, build it in place with
`cargo build --release` (binary at `target/release/defgen`), or grab a
prebuilt binary from the [releases page](https://github.com/mrkct/defgen/releases).

## Usage

```sh
defgen <SCHEMA.defs> --backend <c|python|javascript|java|kotlin|swift> [-o <PATH>]
```

```sh
# Write light.py next to the schema
defgen light.defs --backend python -o light.py

# Or print to stdout
defgen light.defs --backend c
```

`-o`/`--out` takes a file path for single-file backends, or a directory when
a backend emits more than one file. Omit it to print the generated code to
standard output.

Two flags help while writing a schema (`--backend` is still required, though
its value is otherwise unused for these):

- `--ast` — dump the parsed syntax tree instead of generating code.
- `--model` — dump the checked model (resolved layouts, offsets, enum
  values) instead of generating code.

Both parse and type errors are reported with source spans; a schema with
errors produces no output.

## Using the generated code

Every backend generates the same shape of API: a class (or struct) per
schema type, with `encode`/`decode` and a `SIZE`/`FIXED_SIZE`/`MAX_SIZE`
constant, plus one exception hierarchy for anything that can go wrong on the
wire. In Python, generated from the schema above:

```python
import light

state = light.LightState(power=light.PowerState.ON, brightness=42)
data = state.encode()
print(data)  # b'*\x01' — exactly LightState.SIZE (2) bytes

decoded = light.LightState.decode(data)
assert decoded.power is light.PowerState.ON
assert decoded.brightness == 42
```

Values that don't fit their field, or buffers of the wrong length, raise
rather than silently truncating or misreading:

```python
state.brightness = 1000
try:
    state.encode()
except light.DefgenRangeError as e:
    print(e)  # LightState.brightness: 1000 does not fit in u8
```

Every generated error type is a subclass of a single `DefgenError`, so one
`except` clause catches anything the codec can raise. The other backends
mirror this: exceptions in Java, JavaScript and Kotlin, a thrown `DefgenError`
in Swift, and `errno`-style return codes in C, all reporting the same
categories of failure (range, length, malformed UTF-8, non-zero validated
padding).

`enum` types with an `else` arm decode unrecognized wire values to a distinct
"unknown" case that carries the raw value, instead of failing — see §5 and §7
of `SPEC.md`.

## Development

```sh
cargo test              # unit tests plus generated-code conformance tests
cargo fmt --all          # formatting (checked in CI)
cargo clippy --all-targets --all-features -- -D warnings
```

The conformance tests generate code with each backend from
`tests/examples/commands.defs`, then run each generated module's own test
suite (`tests/examples/*_conformance.*`) against the same hand-computed wire
bytes, so a bug that makes one backend disagree with another fails the build.

The browser playground lives in [`web/`](web/) and is built separately:

```sh
rustup target add wasm32-unknown-unknown   # once
./web/build.sh --serve                     # http://localhost:8000
node web/check.mjs                         # wasm output == CLI output
```
