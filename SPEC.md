# defgen IDL Specification (v1)

`defgen` compiles a `.defs` schema into matching C, Java, Kotlin, Python, and
Swift code for reading and writing the fixed-width binary values stored in
BLE GATT characteristics.

This document specifies the language: its grammar-level rules, what is and
isn't allowed, and the contract generated code must satisfy. It does not
specify the compiler's internals.

The worked example is `tests/examples/commands.defs` — every feature below
has a corresponding line in that file.

## 0. Design philosophy

- **Explicit over implicit.** Wire-critical facts (discriminant values,
  byte order, unknown-value handling) are either stated in the schema or
  governed by one documented default — never left to each backend to
  decide on its own.
- **Fail loud by default, opt into leniency.** Decoding an unrecognized
  enum value or command id is a hard error unless the schema explicitly
  declares an `else` fallback. Silent misinterpretation of malformed
  data is worse than a thrown exception.
- **Exact layouts, checked at compile time.** Every container's declared
  bit width must be exactly accounted for by its fields. There is no
  implicit trailing padding at the struct level — if the bits don't add
  up, it's a compile error, not a runtime surprise.
- **Fixed-width by default; variable-length only at the very end.** A
  container is exactly-sized unless it deliberately opts into one
  trailing variable-length field (§6.3). Every offset before that point
  is always compile-time-known — only the final field's length depends
  on what's actually on the wire, and that length is never itself stored
  in the payload (see §6.3, §10 on ATT_MTU).

## 1. Lexical structure

- Files are UTF-8, `.defs` extension.
- `//` line comments. `///` doc comments immediately preceding a
  declaration are propagated into generated code as native doc comments
  (Javadoc, KDoc, Python docstring, Swift `///`, Doxygen).
- Identifiers: `[A-Za-z_][A-Za-z0-9_]*`. Type names conventionally
  `PascalCase`, field names `snake_case`; the compiler does not enforce
  casing, but each backend's naming-convention conversion (§12) assumes it.
- Trailing commas are permitted in every comma-separated list.

### 1.1 File header

A file may optionally open with a header section — the file-level `endian`
pragma (§8), declared at most once — followed by a line containing only
`---`, which separates the header from the type/service declarations that
make up the rest of the file:

```
endian: little;

---

alias Volume = u4;
...
```

The header and its `---` are optional as a unit: a file with neither just
starts straight into its declarations, and the default byte order
(little-endian) applies. A file that does declare `endian` must still follow
it with `---` before any declaration. Nothing but `endian` may appear above
the `---`; nothing appears below it except type and service declarations.
This keeps the one global, file-wide setting visually separated from the
declarations it applies to, rather than mixed in among them.

### 1.2 Attributes

`#[attribute(args)]` on its own line, directly above a `struct` or
tagged-union `enum` declaration (below any `///` doc comment, matching the
doc-comment-then-attribute ordering convention), attaches a compile-time
modifier to that declaration:

```
/// Legacy characteristic kept for backwards compatibility.
#[endian(big)]
struct LegacySerial: u32 {
    serial: u32,
}
```

This is a general mechanism, not special-cased syntax per modifier — it
exists so future per-declaration options don't each need their own bolted-
on syntax. In v1 the only recognized attribute is `endian(little|big)`
(§8); an unrecognized attribute name is a compile error.

`plain` enums, `alias`, `scaled` and `const` declarations don't have their own
byte order (§8 — only root containers do) and so cannot carry
`#[endian(...)]`; doing so is a compile error.

## 2. Primitive types

| Type | Meaning |
|---|---|
| `uN` | unsigned integer, exactly `N` bits, `1 <= N <= 128` |
| `iN` | two's-complement signed integer, exactly `N` bits, `2 <= N <= 128` |
| `bool` | sugar for `u1`; `0`/`1` decode to `false`/`true` |

There is no raw floating-point wire type — `f32`/`f64` exist only as the
decoded, in-memory representation a `scaled` declaration (§4) produces,
never as something a field can be directly backed by on the wire. This
sidesteps IEEE-754 endianness and precision questions that real devices
don't deal with either.

A field's on-wire width is always exactly the bit width of its declared
type. There is no truncating/reinterpreting a wider type into a narrower
field slot.

The 128-bit ceiling is a limit on **values**: a `uN`/`iN` field is decoded
into a native integer, and 128 bits is the widest any target language has
one for. Bit counts that are never held as a single value are not bound by
it — a container's declared width (§6) and a run of `padding` (§6.2) may go
up to 4096 bits (512 bytes, the largest value ATT can carry). So
`struct Blob: u2048 { data: u8[256] }` is fine, while `data: u2048` as a
field is not.

`N` is not restricted to native widths: `u4`, `u9` and `u12` are ordinary
types, because a bit-packed payload is exactly where such widths turn up
(the Bluetooth SIG's own characteristics use 12-, 24- and 48-bit
integers). The decoded representation is pinned down so backends cannot
drift apart on it:

- a `uN`/`iN` value is carried in the **smallest native integer that holds
  `N` bits** — 8, 16, 32, 64 or 128 — signed for `iN`, unsigned for `uN`;
- an `iN` narrower than its carrier is **sign-extended from bit `N-1`** on
  decode, so `i12` `0xFFF` decodes to `-1`, not `4095`;
- on encode a value outside `[0, 2^N - 1]` (or `[-2^(N-1), 2^(N-1) - 1]`)
  is a **hard error**, never a silent truncation — the same rule as a
  `scaled` value's range check (§4);
- in a language with no unsigned types, a backend may widen the carrier or
  wrap it, whatever is idiomatic (§12) — but only the in-memory type
  changes, never the `N` bits on the wire.

There is also no fixed-width `string`. `string` exists only in its
variable-length form, `string(max: N)` — see §6.3.

## 3. Aliases

```
alias Volume = u4;
alias OwnerName = string(max: 32);
```

`alias Name = Type;` gives any field-legal type — a primitive, or a
variable-length `string`/array (§6.3) — a domain name. It is purely a
compile-time convenience: it generates no runtime type of its own, and
carries no conversion or unit metadata — that's what `scaled` (§4) is for.
`scaled` itself may only wrap `uN`/`iN`, never a variable-length type.

### 3.1 Constants

```
const MaxRetries: u8 = 5;
const MinTemperature: i16 = -40;
```

`const Name: uN|iN = <literal>;` declares a named integer value with no wire
presence of its own — unlike everything else in this section, it is never a
field's type, an alias target, or a characteristic binding. It exists purely
so a schema can hand generated code a shared magic number (a retry count, a
protocol version, a sensible default) once, instead of each language's
project re-declaring it by hand and risking drift between them — the same
concern §0 raises about hand-written codecs, applied to plain values instead
of wire layout.

- The declared type is always `uN` or `iN` (§2); `bool`, a `scaled` type, an
  `enum`, or any other declared name may not back a `const`.
- The literal may be negative only for an `iN` constant; `uN` has no sign.
  Either way, the value must fit the declared type's range exactly as a field
  value would (§2, §11) — encode-time range checking has no meaning here,
  since a constant is never encoded, but the same bound still applies: a value
  a backend could never actually hold in the type is a compile error, not a
  silent truncation.
- A `const` is not a type: it cannot be named as a field's type, an array
  element, an alias target, or a characteristic binding. It exists only to be
  read back by the code a project writes around the generated module.
- Each backend emits it as that language's closest idiom for a named,
  immutable integer value (§12).

## 4. Scaled types

```
scaled Temperature: i16 as f32 (scale: 0.01);
scaled BatteryVoltage: u8 as f32 (scale: 0.02, offset: 0.0);
```

`scaled Name: RawType as PhysicalType (scale: <float>[, offset: <float>]);`
declares a fixed-point physical value: `RawType` is the `uN`/`iN` wire
type, `PhysicalType` (`f32` or `f64`) is the type generated code exposes
to callers, and the two are related by `physical = raw * scale + offset`
(`offset` defaults to `0`).

This is a dedicated top-level declaration, not something attachable to an
arbitrary field — a schema either names the conversion once, with both
types spelled out, or doesn't have one at all. There is no inline
`field: uN scale(...)` form. `RawType` must be `uN`/`iN`; `scale` may
never be attached to `bool` or an `enum`.

Generated code exposes both representations:

- decode: `physical: PhysicalType = raw * scale + offset`.
- encode: `raw: RawType = round((physical - offset) / scale)`, then
  range-checked against `RawType`'s bit width — out-of-range input is a
  hard error, not silent wraparound/truncation.
- the underlying raw integer stays reachable too (backend-specific
  naming, e.g. a `_raw`-suffixed accessor), for callers that want to
  round-trip a value without floating-point rounding at all.

A `scaled` name is used as a field/array-element type exactly like an
alias (e.g. `samples: Temperature[4]`, §6.1). There is no unit annotation
in the schema — nothing downstream of the schema (codegen, wire format)
ever reads a unit, so it would only be documentation; put that in a `///`
doc comment on the `scaled` declaration instead.

## 5. Enums (closed value sets)

```
enum HearingMode: u4 {
    Default = 0,
    Stereo = 1,
    Mono = 2,
    Cinema = 3,
    else Unknown,
}
```

- `enum Name: uN { ... }` declares a set of named values backed by an
  `N`-bit unsigned wire value.
- Variants are numbered by declaration order starting at 0 unless given an
  explicit `= <int>`. Mixing implicit and explicit numbering in one enum is
  allowed; the compiler advances the implicit counter from the last
  explicit value it saw.
- **`else` decoding policy:** if the last arm is `else <VariantName>`, the
  enum is *open* — decoding synthesizes a variant of that name carrying a
  fixed field `raw: uN`, for any wire value that doesn't match a declared
  variant, and decoding never fails on this enum. If there is no `else`
  arm, the enum is *closed* — decoding an unmatched value is a hard decode
  error (surfaced per backend as an exception/`Result::Err`/equivalent).
- It is a compile error for two variants to share a numeric value, and for
  any declared value to fall outside `[0, 2^N - 1]`.

## 6. Structs (fixed-width records)

```
struct Orientation: u24 {
    x: i8,
    y: i8,
    z: i8,
}
```

- `struct Name: uN { field, field, ... }` declares a record backed by
  exactly `N` bits. `N` here is a bit count, not a primitive type, so it is
  not capped at 128 the way a field's `uN` is (§2) — a fixed struct may be
  up to 4096 bits (512 bytes) wide. A `#[endian(...)]` attribute (§1.2, §8)
  may precede the declaration to override the file's default byte order for
  this struct.
- **Exact-fit rule:** the sum of every field's bit width must equal `N`
  exactly. This is a compile error, not a warning — under- or
  over-specifying a struct's own container is almost always a bug.
- A field's type may be: a primitive, an alias, a `scaled` type, an enum,
  another previously-declared struct (nesting), or any of those followed
  by `[count]` for a fixed-size array (§6.1).
- **`struct Name { field, field, ... }` — no `: uN` at all** is the other
  declaration form: a *variable-length* struct, whose last field is a
  `string`, a variable-length array, or another variable-length struct.
  See §6.3; a struct either declares an exact bit width and is fully
  fixed, or omits it and ends in exactly one variable-length field —
  nothing in between.
- Fields are packed in declaration order with no implicit gaps, starting at
  the **front** of the container — the first field always occupies the
  container's first bits, and so its first byte. Declaration order is wire
  order, whatever the byte order.
- **Bit order follows byte order** (§8), and is not separately
  configurable. In a little-endian container fields fill from bit 0, the
  least significant bit, so the first field's own bit 0 is the container's
  bit 0; in a big-endian one they fill from the most significant bit down,
  so the first field's most significant bit is the container's. Both come
  to the same thing: pack the container as one integer, then write that
  integer out in the container's byte order. This is the one convention
  that gets a bit-packed register and a byte-oriented record right at the
  same time — a little-endian `struct S: u16 { a: u4, b: u12 }` is the
  `b << 4 | a` that a little-endian device writes, and a big-endian
  `struct S: u32 { serial: u8[4] }` is the four bytes in the order they
  were declared, rather than reversed.
- Nested structs/enums are flattened into the parent's bit sequence; a
  nested type's own `#[endian(...)]` attribute, if any, only takes effect
  when that type is serialized as a root value (bound to a characteristic,
  or otherwise encoded/decoded on its own) — see §8.
- **Byte-crossing diagnostic (non-fatal):** a named or `reserved` field that
  starts mid-byte and spans into the following byte — i.e. it neither
  starts at a byte boundary nor stays within one byte — is legal (there is
  no alignment requirement) but produces a non-fatal diagnostic, the same
  category as the MTU note (§10). This is a real pattern, not just a
  mistake — e.g. BLE's own `Appearance` characteristic packs a 10-bit and a
  6-bit value into one 16-bit container, so one of the two necessarily
  crosses the boundary — but it is unintentional often enough (a
  miscounted field width, most commonly) to be worth flagging. `padding`
  is exempt, since it carries no value and so has no bug signal either way.

### 6.1 Fixed-size arrays

`field: Type[N]` is `N` back-to-back repetitions of `Type`, occupying
`N * bitwidth(Type)` bits, with no length prefix and no terminator — the
count is always known at compile time from the schema alone. Element type
may be a primitive, alias, `scaled` type, enum, or struct (array-of-struct).

Arrays are always fixed-count. There is no variable-length array in v1
(see §14).

### 6.2 Padding and reserved bits

Three distinct declarations cover the cases that "just call it padding"
usually conflates:

| Declaration | Encode | Decode | Exposed to caller? |
|---|---|---|---|
| `padding: uN,` | always writes zero | ignored, not checked | no |
| `padding: uN = 0,` | always writes zero | **error if non-zero** | no |
| `reserved name: uN,` | writes back whatever was captured on decode | captured verbatim | yes, read-only |

- `padding` is for bits you don't care about. The bare form is maximally
  lenient (matches devices that put arbitrary garbage in unused bits); the
  `= 0` form is for layouts where non-zero really does indicate a bug or a
  version mismatch worth failing loudly on.
- `reserved <name>: uN` is for bits you must preserve but don't yet
  understand — the classic "some firmware fields must be echoed back
  unchanged to avoid breaking a future extension" case. The generated
  struct carries the raw value as a read-only field so a decode-then-relay
  round trip doesn't clobber it.
- An unnamed field is always `padding`; `reserved` fields are always named.

An **unmatched tagged-union variant's unused trailing payload bits**
(§7) behave exactly like bare `padding` — silently zero-filled on encode,
unchecked on decode — without needing to be spelled out per variant. If a
variant's author wants strict zero-validation on its unused tail, they
can write it explicitly as a trailing `padding: uN = 0` field.

### 6.3 Variable-length fields

```
alias OwnerName = string(max: 32);

struct DiagnosticLabel {
    severity: u8,
    label: string(max: 24),
}
```

Two variable-length field types exist:

| Type | Meaning |
|---|---|
| `string(max: N)` | UTF-8 text, at most `N` bytes on the wire (`N` bounds bytes, not codepoints) |
| `Type[max: N]` | between 0 and `N` back-to-back repetitions of `Type` |

- Either may only appear as the **last field** of a struct, and a struct
  may contain **at most one** variable-length-contributing field —
  whether that's a `string`, a `Type[max: N]`, or a nested struct that is
  itself variable-length (§6 above; variable-ness of a nested struct
  propagates to whatever contains it, always still only in trailing
  position). Putting one anywhere else, or having more than one, is a
  compile error.
- A struct containing such a field **omits its `: uN` width** entirely
  (§6) — there is no single bit count to declare. The compiler still
  requires the *fixed* fields preceding it to sum to a whole number of
  bytes (a multiple of 8 bits): the actual runtime length is always
  computed as `buffer_length - fixed_prefix_bytes`, which only makes
  sense at a byte boundary. Sub-byte fields (e.g. two `u4`s) are still
  fine as long as their combined width is byte-aligned by the time the
  variable field starts.
- `Type` in `Type[max: N]` must itself have a byte-multiple width (a
  primitive `u8`/`u16`/.../`u128`, an alias of one, or a struct/enum with
  a byte-multiple declared width) — decoding divides the remaining bytes
  by the element's byte width to recover the element count, which only
  works evenly if elements are whole bytes; a decode where that division
  has a remainder is a hard error (the data doesn't correspond to a whole
  number of elements).
- **There is no length prefix in the payload.** The actual length is
  whatever the GATT transport itself delivers — for a write, exactly the
  bytes the client sent; for a read/notify, exactly the bytes the
  peripheral chose to return. This matches how BLE already works (ATT
  frames a value's length for you) and avoids a schema-declared length
  ever disagreeing with the bytes actually on the wire. Concretely:
  encode produces a buffer of exactly `fixed_prefix_bytes +
  actual_variable_bytes` (never padded out to `max`); decode's input is
  that same variably-sized buffer, and the variable field's length is
  derived from `buffer.len()`, not read out of it.
- **Validation, on both sides:** it is a hard error to encode more than
  `max` elements/bytes, and a hard decode error if the received buffer
  implies more than `max`. For `string`, decode additionally validates
  the bytes are well-formed UTF-8 and fails otherwise — silently
  replacing invalid bytes would be exactly the kind of per-SDK-drift this
  spec otherwise avoids (§0).
- `alias` may name a `string(max: N)` or `Type[max: N]` (§3) so a
  characteristic can bind directly to one without a wrapping struct —
  the common case of "one characteristic is just a name" doesn't need
  any boilerplate.
- Because a variable-length struct's exact size isn't known until
  runtime, the compiler tracks its *maximum* possible size
  (`fixed_prefix_bytes + max_variable_bytes`) for the purposes of the
  MTU diagnostic (§10) and for backends that need to size a receive
  buffer up front.
- **Out of scope for v1:** a variable-length field inside a tagged-union
  variant. Tagged unions remain fully fixed-width (§7) — model a
  variable-length value (like a name) as its own characteristic instead
  of as one command among several. See §14.

## 7. Tagged unions

```
enum Command(id: u16): u64 {
    SetVolume(0x0001) { volume: Volume }
    TriggerFactoryReset(0xffff)
    else Unknown
}
```

- `enum Name(tag: uT): uN { Variant(hex_id) [{ fields }] ... [else Name] }`
  declares a C-union-like value: a `T`-bit discriminant occupying the
  low `T` bits of an `N`-bit container, followed by a `(N - T)`-bit
  payload region whose interpretation depends on the discriminant.
- Every variant's numeric id is **mandatory and explicit** (unlike plain
  enums, §5) — these are cross-firmware-version wire contracts, and
  auto-numbering them would silently reshuffle wire values if a variant
  is ever reordered.
- A variant's field list is packed the same way a struct's fields are
  (§6), but its total width need only be `<= N - T`; unused trailing bits
  are implicit padding (§6.2). A variant with no braces (like
  `TriggerFactoryReset` above) uses zero payload bits.
- It is a compile error for a variant's field width to exceed `N - T`, or
  for two variants to share a numeric id.
- **`else` decoding policy**, same idea as §5: if the last arm is
  `else <VariantName>`, the union is open, and decoding an unrecognized id
  synthesizes a variant of that name carrying `{ id: uT, raw: u(N-T) }`
  instead of failing. Without an `else` arm, an unrecognized id is a hard
  decode error. Because `raw` is an ordinary value, `N - T` must be between
  1 and 128 bits (§2): a union with a container wider than that has to be
  closed.
- Recommended (not enforced) evolution practice: never remove or renumber
  an existing variant; add new ones instead. Combined with an `else`
  fallback, this lets an older SDK talk to newer firmware (and vice versa)
  without either side crashing on an id it doesn't recognize yet. The same
  applies to plain-enum values (§5).

## 8. Endianness

- `endian: little;` or `endian: big;` in the file header (§1.1) sets the
  default byte order for every *root* container (see below) that doesn't
  override it. It may be declared at most once; if the header is omitted
  entirely, the default is little-endian, the common case for BLE.
- Any struct or tagged-union enum may override it locally with the
  `#[endian(...)]` attribute (§1.2):
  ```
  #[endian(big)]
  struct LegacySerial: u32 {
      serial: u32,
  }
  ```
- **What byte order does.** A root container is packed as a single integer
  and written out in its byte order — most significant byte first if it is
  big-endian, least significant first if little. Field *positions* do not
  depend on byte order: the first field always occupies the container's
  first byte, because bit order follows byte order too (§6). What byte
  order changes is the direction each multi-byte value reads in. So

  ```
  struct Reading: u32 { id: u8, value: u16, crc: u8 }
  ```

  is `id`, then `value`, then `crc` on the wire under either setting, with
  `value` little-endian in a little-endian container and big-endian in a
  big-endian one — the same layout its datasheet would state.
- **Endianness is a root-container property only.** A container is "root"
  if it is ever bound directly to a characteristic (§10) or otherwise
  encoded/decoded on its own, as opposed to only ever appearing nested
  inside another container's fields. It is a compile error to write
  `#[endian(...)]` on a declaration that is *only* ever used as a nested
  field — bit-packing flattens nested types into the parent's single
  contiguous bit sequence first, and byte order is applied exactly once,
  to that flattened sequence, by the root container's setting. A type used
  both nested and standalone must pick one root endianness, applied
  whenever it's the root.
- **Arrays and variable-length tails.** An array's elements are always in
  declaration order — `xs: u8[4]` is `xs[0]` first, in the container's
  first byte — and byte order applies within each element, not across
  them. A variable-length struct (§6.3) is its fixed prefix followed by
  its tail: the prefix is one container in the sense above, and each tail
  element is packed as its own byte-multiple container under the same byte
  order. A `string` is UTF-8 bytes in order and byte order never touches
  it. This makes `Type[N]` and `Type[max: N]` lay the same elements out
  the same way, which is the point.
- There is intentionally no per-field byte-swap override inside an
  otherwise-consistent container. Genuinely mixed-endianness-within-one-
  container devices are rare enough that v1 treats this as out of scope;
  model such a field as its own root-level type with its own characteristic
  binding, or as a separate encode/decode step outside generated code (see
  §14).

## 9. Nested structs

Covered in §6: any previously-declared `struct` name may be used as a
field's type, and its layout is inlined bit-for-bit at that position.
Forward references are not allowed — a struct must be declared before
anything that embeds it.

## 10. GATT-layer metadata

```
service HearingAidControl(uuid: "7d8f0000-...") {
    characteristic StatusChar(
        uuid: "7d8f0001-...",
        properties: [read, notify],
    ): Status;
}
```

- `service Name(uuid: "...") { characteristic ... ; ... }` groups
  characteristic bindings under a GATT service UUID.
- `characteristic Name(uuid: "...", properties: [prop, ...]): Type;` binds
  a previously-declared struct, tagged-union, or alias type as a
  characteristic's value (a variable-length type, §6.3, is bindable too —
  `OwnerName` in the example is bound directly with no wrapping struct).
  `properties` is drawn from the standard GATT set: `read`, `write`,
  `write_without_response`, `notify`, `indicate`.
- Binding is additive metadata, not part of a type's definition — a type
  never needs a `service`/`characteristic` to exist (e.g. `Orientation` in
  the example is only ever used nested) and the same type may back more
  than one characteristic.
- A bound type is what the transport actually carries, so its width (its
  fixed prefix, for a variable-length type) must be a whole number of
  bytes: ATT frames a value in bytes, and a 4-bit characteristic has no
  meaning. Sub-byte types are still free to appear *inside* a bound
  container. UUIDs are written in hex, in any of the three GATT forms —
  16-bit (`"180a"`), 32-bit (`"0000180a"`) or 128-bit
  (`"0000180a-0000-1000-8000-00805f9b34fb"`).
- The compiler knows each characteristic's exact encoded byte length (or,
  for a variable-length type, its maximum, §6.3) and emits a non-fatal
  diagnostic (not an error — MTU is negotiable at runtime) when that
  size exceeds 20 bytes, the default ATT payload size without MTU
  negotiation.
- Anything beyond UUID + properties + value type (connection setup,
  subscription management, actual GATT client/server API shape) is
  backend-specific and out of scope for the schema itself — each backend
  decides what idiomatic wrapper, if any, to generate around the
  underlying platform BLE library (CoreBluetooth, `BluetoothGatt`, BlueZ,
  `bleak`, ...).

## 11. Compile-time errors (non-exhaustive checklist)

- Struct field widths not summing to exactly the declared container width.
- Tagged-union variant field width exceeding `container_width - tag_width`,
  or a discriminant wider than the container it lives in.
- Duplicate plain-enum values, or duplicate tagged-union ids, within one
  enum.
- A declared enum/tagged-union value outside its backing type's range —
  including one the implicit counter (§5) walked into.
- An `enum` or tagged union declaring no variants at all, or an open
  tagged union whose payload region is not a legal width for the `raw`
  field its fallback variant carries — zero bits, or more than 128 (§7).
- A container width or `padding` run above 4096 bits, or a *value* (a
  field, `reserved` run, enum backing type or discriminant) above 128 (§2).
- A `scaled` declaration's `RawType` not being `uN`/`iN`.
- A `const` declared with a type other than `uN`/`iN`, a negative literal on
  a `uN` constant, or a literal that does not fit the declared type's range
  (§3.1).
- A `const` name used as a field's type, an array element, an alias target, or
  a characteristic binding (§3.1) — a constant has no wire representation to
  bind.
- An unrecognized `#[...]` attribute name, or `#[endian(...)]` on a
  declaration that is only ever used as a nested field.
- A variable-length field (`string`/`Type[max: N]`/a variable-length
  nested struct) anywhere but the last field of a struct; more than one
  such field in one struct; or one appearing anywhere in a tagged-union
  variant (§6.3, §7).
- A struct declaring `: uN` while also containing a variable-length
  field, or omitting `: uN` while *not* ending in one.
- The fixed fields preceding a variable-length field not summing to a
  multiple of 8 bits, or a `Type[max: N]` element type not having a
  byte-multiple width.
- `max: N`, or a fixed array's `[N]`, that is not a positive integer.
- Reference to an undeclared type, or a struct referencing itself
  (directly or transitively) as a field.
- `endian` declared more than once, or appearing below the `---` separator
  (§1.1) instead of in the file header.
- Duplicate field names within one struct or variant; duplicate variant
  names within one enum or tagged union; duplicate
  characteristic/service names within a file; two declarations sharing a
  name, or a declaration named after a built-in type (`u8`, `bool`, ...).
- A characteristic binding something other than a `struct`, tagged-union
  `enum` or `alias`, binding a type that is not a whole number of bytes,
  listing one GATT property twice, or carrying a UUID that is not one of
  the three hex forms (§10); two characteristics of one service, or two
  services in one file, sharing a UUID.

## 12. Codegen contract (per backend: C, Java, JavaScript, Kotlin, Python, Swift)

Each backend must, at minimum:

- Emit a `const` (§3.1) as that language's idiom for a named, immutable
  integer value, holding the exact declared value — no codec, since a
  constant is never encoded or decoded.
- Emit an encode function/method and a decode function/method per root
  type (struct, or tagged-union), operating on a fixed-size byte buffer
  of exactly that type's byte length.
- Carry each `uN`/`iN` value in the smallest native integer that holds `N`
  bits, sign-extending `iN` on decode and range-checking on encode, as
  §2 spells out.
- Represent an open enum's/union's unknown case as a distinct variant
  (e.g. a sum-type case, sealed-class subtype, or tagged struct) carrying
  the synthesized `raw` (and `id`, for unions) field(s) — never silently
  coerced into one of the known cases.
- Make a closed enum's/union's decode operation fallible (exception,
  `Result`, `Optional`/`nil`, per the language's idiom) on an unmatched
  value.
- Convert identifier casing to the target language's convention
  (`snake_case` fields, for instance, become `camelCase` properties in
  Kotlin/Swift/Java) without changing the schema's own naming rules.
- Propagate `///` doc comments to the native doc-comment form.
- Represent a `string`/`Type[max: N]` field as the target language's
  native string/array type where one exists (Java/JavaScript/Kotlin/Swift/
  Python all have one); reject, at encode time, a value longer than `max`. Validate
  UTF-8 on `string` decode and fail (exception/`Result`/equivalent) on
  malformed input rather than substituting replacement characters. C has
  no native growable string or array, so the C backend represents a
  variable-length field as a fixed-capacity buffer of `max` elements plus
  an explicit length, never a dynamically-allocated one.

Exact API shape (builder vs. constructor, mutability, module layout) is a
per-backend decision and is intentionally not pinned down here.

## 13. Conformance testing (recommended, not part of the schema)

Because six independently-implemented backends must agree on every bit
of layout, byte order, and unknown-value handling, the recommended
workflow — once codegen exists — is a set of golden fixtures: hex byte
strings paired with the expected decoded value (and vice versa for
encode), generated once from the schema and run against all six
backends in CI. This is what catches a Kotlin/Swift bit-ordering
disagreement in a test run instead of in the field. This document does
not specify the fixture format; it's a follow-up once the compiler
exists.

## 14. Explicitly out of scope for v1

- Variable-length fields anywhere but the trailing position of a struct —
  in particular, inside a tagged-union variant (§6.3, §7).
- Length-prefixed encoding of any kind: a variable-length field's actual
  length always comes from the transport/buffer, never from a length
  value stored in the payload itself (§6.3).
- A minimum-length constraint on a variable-length field (only a `max`).
- Raw floating-point wire types (only `scaled` fixed-point, §4).
- Per-field byte-swap overrides inside an otherwise-consistent container
  (§8).
- GATT descriptors, security/permission metadata, encryption/signing.
- Any schema-versioning or schema-diffing/migration mechanism: v1 has no
  version pragma and no generated version constant at all — evolution
  discipline (never renumbering a value, adding `else` fallbacks) is a
  recommendation for schema authors, not something the tooling tracks.
- Bit order as an axis separate from byte order: a container's bit order
  always follows its byte order (§6, §8), and neither can be set per field.
