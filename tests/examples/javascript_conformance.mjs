// Conformance fixture for the JavaScript backend.
//
// The Rust test generates a module from `commands.defs`, drops this file next
// to it, and runs it. Everything here is a claim about the *wire format*: a
// hand-written byte string on one side, the decoded value on the other. Bit
// offsets are worked out from SPEC.md §6 by hand rather than read back out of
// the generated module, so a bug in the emitter's layout arithmetic cannot
// agree with itself into a passing test.
//
// The byte strings are the very same ones `c_conformance.c` and
// `python_conformance.py` assert — that is the point of §14: several backends,
// one wire format.
//
// `defgenRound` and `DefgenBits` are module-private in the generated file (real
// consumers never see them), so — as in the Java and Swift fixtures — the
// rounding table those other fixtures check directly is exercised here through
// `temperatureToRaw`, which is the only way in from outside.
//
// Exit status is the number of failures.

import * as m from "./commands.mjs";

let failures = 0;

/**
 * @param {boolean} cond
 * @param {string} what
 */
function check(cond, what) {
  if (!cond) {
    console.log(`FAIL: ${what}`);
    failures++;
  }
}

/** @param {Uint8Array} data */
function hex(data) {
  return Array.from(data, (b) => b.toString(16).padStart(2, "0")).join(" ");
}

/**
 * @param {Uint8Array} got
 * @param {number[]} want
 * @param {string} what
 */
function checkBytes(got, want, what) {
  const wanted = new Uint8Array(want);
  if (got.length === wanted.length && got.every((b, i) => b === wanted[i])) {
    return;
  }
  console.log(`FAIL: ${what}\n  want: ${hex(wanted)}\n  got:  ${hex(got)}`);
  failures++;
}

/**
 * @param {Function} errorClass
 * @param {() => unknown} fn
 * @param {string} what
 */
function checkThrows(errorClass, fn, what) {
  try {
    fn();
  } catch (e) {
    if (e instanceof errorClass) return;
    const name = e instanceof Error ? e.name : typeof e;
    console.log(`FAIL: ${what}: threw ${name} (${e}), want ${errorClass.name}`);
    failures++;
    return;
  }
  console.log(`FAIL: ${what}: did not throw ${errorClass.name}`);
  failures++;
}

// ---------------------------------------------------------------------------
// Struct: bit packing, nesting, scaled fields, reserved bits (§6)
// ---------------------------------------------------------------------------
//
// Status is 64 little-endian bits, packed LSB-first in declaration order:
//
//   [0..4)   active_profile u4
//   [4..8)   volume         u4  (alias of u4)
//   [8..12)  mode           u4  (open enum)
//   [12..13) muted          bool
//   [13..21) battery        u8  (scaled, 0.02)
//   [21..45) orientation    3 x i8
//   [45..60) padding        u15
//   [60..64) reserved flags u4

function testStatus() {
  check(m.Status.SIZE === 8, "Status.SIZE === 8");

  const s = new m.Status({
    activeProfile: 0x3,
    volume: 0xa,
    mode: m.HearingMode.Cinema, // 3
    muted: true,
    battery: 2.0, // raw 100 = 0x64
    orientation: new m.Orientation({ x: -1, y: 2, z: -128 }),
    flags: 0x5,
  });

  // With battery raw = 0x64 and orientation = (0xff, 0x02, 0x80), the fields
  // straddle byte boundaries — which is the point of the exercise:
  //
  //   byte 0  bits  0..8   profile 3 | volume 0xa << 4              -> 0xa3
  //   byte 1  bits  8..16  mode 3 | muted << 4 | battery[0..3] << 5 -> 0x93
  //   byte 2  bits 16..24  battery[3..8] | x[0..3] << 5             -> 0xec
  //   byte 3  bits 24..32  x[3..8] | y[0..3] << 5                   -> 0x5f
  //   byte 4  bits 32..40  y[3..8] | z[0..3] << 5                   -> 0x00
  //   byte 5  bits 40..48  z[3..8] | padding                        -> 0x10
  //   byte 6  bits 48..56  padding                                  -> 0x00
  //   byte 7  bits 56..64  padding | flags 5 << 4                   -> 0x50
  const buf = s.encode();
  check(buf instanceof Uint8Array, "encode returns a Uint8Array");
  check(buf.length === 8, "Status encodes to 8 bytes");
  checkBytes(buf, [0xa3, 0x93, 0xec, 0x5f, 0x00, 0x10, 0x00, 0x50], "Status bytes");

  const back = m.Status.decode(buf);
  check(back.activeProfile === 0x3, "Status.activeProfile round-trips");
  check(back.volume === 0xa, "Status.volume round-trips");
  check(back.mode === m.HearingMode.Cinema, "Status.mode round-trips");
  check(back.muted === true, "Status.muted round-trips");
  check(1.99 < back.battery && back.battery < 2.01, "Status.battery round-trips");
  check(back.orientation.x === -1, "Orientation.x is sign-extended, not 255");
  check(back.orientation.y === 2, "Orientation.y round-trips");
  check(back.orientation.z === -128, "Orientation.z round-trips");
  check(back.flags === 0x5, "reserved bits round-trip (§6.2)");

  // A value too wide for its field is a hard error, never a truncation.
  s.activeProfile = 16;
  checkThrows(m.DefgenRangeError, () => s.encode(), "u4 overflow is a range error");
  s.activeProfile = -1;
  checkThrows(m.DefgenRangeError, () => s.encode(), "u4 underflow is a range error");
  // A `number` is a double, so a non-integer reaches an integer field as an
  // ordinary value rather than as a type error. It is still not a `u4`.
  s.activeProfile = 1.5;
  checkThrows(m.DefgenRangeError, () => s.encode(), "a fractional u4 is a range error");
  s.activeProfile = Number.NaN;
  checkThrows(m.DefgenRangeError, () => s.encode(), "and so is NaN");
  s.activeProfile = 3;

  // Wrong lengths are rejected.
  checkThrows(m.DefgenLengthError, () => m.Status.decode(buf.subarray(0, 7)), "short buffer");
  checkThrows(
    m.DefgenLengthError,
    () => m.Status.decode(new Uint8Array(9)),
    "long buffer",
  );

  // Every error is a DefgenError, so one `catch` clause catches the lot.
  check(new m.DefgenLengthError("x") instanceof m.DefgenError, "DefgenLengthError is a DefgenError");
  check(new m.DefgenRangeError("x") instanceof m.DefgenError, "DefgenRangeError is a DefgenError");
  check(new m.DefgenRangeError("x") instanceof Error, "and an Error");
}

function testOpenEnum() {
  // An open enum keeps an unrecognized wire value rather than failing (§5).
  check(m.HearingMode.Stereo === 1, "HearingMode.Stereo === 1");
  check(Object.isFrozen(m.HearingMode), "the enum object is frozen");

  const buf = new Uint8Array(8);
  buf[1] = 0x09; // mode = 9, undeclared
  const s = m.Status.decode(buf);
  check(s.mode instanceof m.HearingModeUnknown, "unknown mode decodes to the else variant");
  check(s.mode.raw === 9, "keeping the wire value it came from");
  check(typeof s.mode !== "number", "and never to a declared variant");

  // and re-encodes to exactly the bytes it came from.
  checkBytes(s.encode(), Array.from(buf), "unknown enum value re-encodes verbatim");
}

// ---------------------------------------------------------------------------
// Byte order (§8)
// ---------------------------------------------------------------------------

function testEndianness() {
  // LegacySerial carries #[endian(big)]: its one value reads MSB-first.
  const s = new m.LegacySerial({ serial: 0x01020304 });
  const buf = s.encode();
  checkBytes(buf, [0x01, 0x02, 0x03, 0x04], "LegacySerial is big-endian");
  check(m.LegacySerial.decode(buf).serial === 0x01020304, "LegacySerial round-trips");

  // while the file default is little-endian.
  const status = new m.Status({ activeProfile: 1 }).encode();
  check(status[0] === 0x01, "Status is little-endian: low bits land in byte 0");
  check(status[7] === 0x00, "Status is little-endian: byte 7 is untouched");
}

// A big-endian container with more than one field: they stay in declaration
// order, first field in the first byte, and only the direction each multi-byte
// value reads in changes (§8). The nested Orientation is flattened into this
// container and picks up its byte order.
function testBigEndianRecord() {
  const r = new m.LegacyReading({
    id: 0x11,
    value: 0x2233,
    orientation: new m.Orientation({ x: 1, y: -2, z: 3 }),
    crc: 0x4455,
  });
  const buf = r.encode();
  checkBytes(
    buf,
    [0x11, 0x22, 0x33, 0x01, 0xfe, 0x03, 0x44, 0x55],
    "LegacyReading keeps its fields in declaration order",
  );
  const back = m.LegacyReading.decode(buf);
  check(back.id === 0x11 && back.value === 0x2233 && back.crc === 0x4455, "LegacyReading scalars");
  check(
    back.orientation.x === 1 && back.orientation.y === -2 && back.orientation.z === 3,
    "LegacyReading's nested struct round-trips",
  );
}

// A fixed array and a variable-length tail in one big-endian container: both
// keep their elements in declaration order, with byte order applying inside an
// element and never across elements (§8).
function testBigEndianSequences() {
  const log = new m.LegacyLog({
    key: [0xde, 0xad, 0xbe, 0xef],
    samples: [1.0, -2.0], // raw 100 = 0x0064, raw -200 = 0xff38
  });
  const buf = log.encode();
  checkBytes(
    buf,
    [0xde, 0xad, 0xbe, 0xef, 0x00, 0x64, 0xff, 0x38],
    "LegacyLog keeps its array and tail elements in order",
  );
  const back = m.LegacyLog.decode(buf);
  checkBytes(Uint8Array.from(back.key), [0xde, 0xad, 0xbe, 0xef], "LegacyLog's fixed array");
  check(m.temperatureToRaw(back.samples[0]) === 100, "LegacyLog sample 0");
  check(m.temperatureToRaw(back.samples[1]) === -200, "LegacyLog sample 1");

  // An empty tail is just the prefix (§6.3).
  const empty = new m.LegacyLog({ key: [0xde, 0xad, 0xbe, 0xef], samples: [] });
  checkBytes(empty.encode(), [0xde, 0xad, 0xbe, 0xef], "LegacyLog with no samples");
}

// ---------------------------------------------------------------------------
// Arrays (§6.1) and scaled types (§4)
// ---------------------------------------------------------------------------

function testTemperatureLog() {
  const log = new m.TemperatureLog({
    samples: [
      21.5, // raw  2150 = 0x0866
      -0.01, // raw    -1 = 0xffff
      0.0,
      327.67, // raw 32767, the i16 maximum
    ],
  });

  const buf = log.encode();
  checkBytes(buf, [0x66, 0x08, 0xff, 0xff, 0x00, 0x00, 0xff, 0x7f], "TemperatureLog bytes");

  const back = m.TemperatureLog.decode(buf);
  check(21.49 < back.samples[0] && back.samples[0] < 21.51, "samples[0] round-trips");
  check(
    m.temperatureToRaw(back.samples[1]) === -1,
    "samples[1] is sign-extended exactly once, i.e. raw -1 and not -65537",
  );
  check(back.samples[3] > 327.66, "samples[3] round-trips");

  // The raw integer stays reachable, so a round trip need not go through
  // floating point at all (§4).
  check(m.temperatureToRaw(21.5) === 2150, "temperatureToRaw");
  check(m.temperatureFromRaw(2150) > 21.49, "temperatureFromRaw");

  // Rounding decides the raw unit: truncation would send both of these to 2151,
  // and rounding half to even — or `Math.round`, which rounds half *up* — would
  // disagree with the C backend on the ties below.
  check(m.temperatureToRaw(21.514) === 2151, "rounds down below the half");
  check(m.temperatureToRaw(21.516) === 2152, "and up above it");
  check(m.temperatureToRaw(-21.514) === -2151, "symmetrically below zero");
  check(m.temperatureToRaw(-21.516) === -2152, "on both sides");
  check(m.temperatureToRaw(0.005) === 1, "a tie rounds away from zero");
  check(m.temperatureToRaw(-0.005) === -1, "on both sides of it");
  check(Math.round(-0.5) === -0 && m.temperatureToRaw(-0.005) === -1, "unlike Math.round");

  // Out of the raw type's range is an error, not a wrap.
  checkThrows(m.DefgenRangeError, () => m.temperatureToRaw(1000.0), "raw overflow");
  checkThrows(m.DefgenRangeError, () => m.temperatureToRaw(-1000.0), "raw underflow");
  checkThrows(m.DefgenRangeError, () => m.temperatureToRaw(Number.NaN), "NaN is rejected");
  checkThrows(m.DefgenRangeError, () => m.temperatureToRaw(Infinity), "inf is rejected");
  checkThrows(m.DefgenRangeError, () => m.temperatureToRaw(-Infinity), "and -inf");

  // A fixed array carries exactly its declared count, always (§6.1).
  checkThrows(
    m.DefgenRangeError,
    () => new m.TemperatureLog({ samples: [0.0, 0.0] }).encode(),
    "a short array fails to encode",
  );
  const fresh = new m.TemperatureLog();
  check(fresh.samples.length === 4, "the default array is full-length");
  check(
    new m.TemperatureLog().samples !== new m.TemperatureLog().samples,
    "and not shared between instances",
  );
}

// ---------------------------------------------------------------------------
// Tagged unions (§7)
// ---------------------------------------------------------------------------

function testCommand() {
  check(m.Command.SIZE === 8, "Command.SIZE === 8");
  check(m.CommandSetVolume.ID === 0x0001, "SetVolume's wire id");
  check(m.CommandTriggerFactoryReset.ID === 0xffff, "TriggerFactoryReset's wire id");

  // A known variant: 16-bit tag in the low bits, payload above it.
  let buf = new m.CommandSetVolume({ volume: 7 }).encode();
  checkBytes(buf, [0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00], "SetVolume bytes");

  const back = m.Command.decode(buf);
  check(back instanceof m.CommandSetVolume, "decodes to the SetVolume class");
  check(back.volume === 7, "SetVolume.volume round-trips");

  // A variant with no payload leaves the whole payload region zero.
  checkBytes(
    new m.CommandTriggerFactoryReset().encode(),
    [0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    "TriggerFactoryReset bytes",
  );

  // A nested struct inside a variant is flattened into the payload region.
  buf = new m.CommandSetOrientationOffset({
    offset: new m.Orientation({ x: 1, y: 2, z: 3 }),
  }).encode();
  checkBytes(buf, [0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00], "SetOrientationOffset bytes");
  const offset = m.Command.decode(buf);
  check(offset instanceof m.CommandSetOrientationOffset, "decodes to the right variant");
  check(offset.offset.z === 3, "the nested struct round-trips");

  // Every variant is a Command, so one annotation covers them all (§7).
  for (const variant of [
    new m.CommandSetVolume(),
    new m.CommandSetMute(),
    new m.CommandSetMode(),
    new m.CommandSetOrientationOffset(),
    new m.CommandTriggerFactoryReset(),
    new m.CommandUnknown(),
  ]) {
    check(variant instanceof m.Command, `${variant.constructor.name} is a Command`);
  }

  // and the base itself is not inhabited: there is no `Command` that is not
  // one of its variants (§7).
  checkThrows(m.DefgenError, () => new m.Command(), "the base class is abstract");

  // A variant's own encode is the one it inherits, and dispatches on itself.
  checkBytes(
    new m.CommandSetMute({ muted: true }).encode(),
    [0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
    "SetMute bytes",
  );
}

function testCommandUnknown() {
  // An unrecognized id decodes to the `else` variant, keeping both the id and
  // the undecoded payload, and never silently becomes a known variant (§7).
  const buf = new Uint8Array(8);
  buf[0] = 0x34;
  buf[1] = 0x12; // id = 0x1234, undeclared
  buf[2] = 0xab;

  const back = m.Command.decode(buf);
  check(back instanceof m.CommandUnknown, "an unknown id decodes to the else variant");
  check(back.id === 0x1234, "the unknown id is kept");
  // The payload is 48 bits, past what a `number` holds exactly, so it is a
  // `bigint` (§2).
  check(back.raw === 0xabn, "and so is the undecoded payload");

  // and re-encodes to exactly the bytes it came from.
  checkBytes(back.encode(), Array.from(buf), "an unknown command re-encodes verbatim");
}

// ---------------------------------------------------------------------------
// Variable-length values (§6.3)
// ---------------------------------------------------------------------------

function testDiagnosticLabel() {
  check(m.DiagnosticLabel.FIXED_SIZE === 1, "DiagnosticLabel.FIXED_SIZE");
  check(m.DiagnosticLabel.MAX_SIZE === 25, "DiagnosticLabel.MAX_SIZE");

  const d = new m.DiagnosticLabel({ severity: 3, label: "hi" });

  // The encoding is exactly prefix + actual tail — never padded to max.
  check(d.encodedSize() === 3, "encodedSize is prefix + tail");
  const buf = d.encode();
  check(buf.length === 3, "and so is the encoding itself");
  checkBytes(buf, [0x03, 0x68, 0x69], "DiagnosticLabel bytes");

  const back = m.DiagnosticLabel.decode(buf);
  check(back.severity === 3, "severity round-trips");
  check(back.label === "hi", "the tail round-trips");

  // An empty tail is legal: the value is just its fixed prefix.
  const empty = new m.DiagnosticLabel({ severity: 3, label: "" }).encode();
  checkBytes(empty, [0x03], "an empty tail is just the prefix");
  check(m.DiagnosticLabel.decode(empty).label === "", "and decodes back to empty");

  // Over `max` fails on encode, and a too-long buffer fails on decode.
  checkThrows(
    m.DefgenRangeError,
    () => new m.DiagnosticLabel({ severity: 0, label: "x".repeat(25) }).encode(),
    "a tail over `max` fails to encode",
  );
  checkThrows(
    m.DefgenLengthError,
    () => m.DiagnosticLabel.decode(new Uint8Array(26)),
    "a buffer over MAX_SIZE fails to decode",
  );
  checkThrows(
    m.DefgenLengthError,
    () => m.DiagnosticLabel.decode(new Uint8Array(0)),
    "an empty buffer too",
  );

  // `max` bounds bytes, not characters: 12 two-byte characters fit, 13 do not.
  check(
    new m.DiagnosticLabel({ severity: 0, label: "é".repeat(12) }).encode().length === 25,
    "12 x é fits",
  );
  checkThrows(
    m.DefgenRangeError,
    () => new m.DiagnosticLabel({ severity: 0, label: "é".repeat(13) }).encode(),
    "13 x é does not",
  );
}

function testOwnerName() {
  // An alias of a variable-length type binds straight to a characteristic.
  check(m.OWNER_NAME_FIXED_SIZE === 0, "OWNER_NAME_FIXED_SIZE");
  check(m.OWNER_NAME_MAX_SIZE === 32, "OWNER_NAME_MAX_SIZE");

  let buf = m.encodeOwnerName("Ada");
  checkBytes(buf, [0x41, 0x64, 0x61], "OwnerName bytes");
  check(m.decodeOwnerName(buf) === "Ada", "OwnerName round-trips");

  // Multi-byte UTF-8 survives, and the byte count is what `max` bounds.
  buf = m.encodeOwnerName("é");
  check(buf.length === 2, "é is two bytes on the wire");
  check(m.decodeOwnerName(buf) === "é", "and round-trips");
  checkThrows(
    m.DefgenRangeError,
    () => m.encodeOwnerName("é".repeat(17)),
    "17 x é exceeds max: 32",
  );

  // Malformed UTF-8 fails rather than being replaced (§6.3).
  for (const [bad, why] of [
    [[0xff], "a byte no encoding starts with"],
    [[0xc3], "a lead byte with no continuation"],
    [[0xc0, 0x80], "an overlong encoding of U+0000"],
    [[0xed, 0xa0, 0x80], "a surrogate, U+D800"],
  ]) {
    checkThrows(
      m.DefgenUtf8Error,
      () => m.decodeOwnerName(new Uint8Array(bad)),
      `rejects ${why}`,
    );
  }

  // A lone surrogate has no UTF-8 encoding, and `TextEncoder` would quietly
  // substitute U+FFFD for it — the same replacement decoding refuses to do.
  checkThrows(
    m.DefgenUtf8Error,
    () => m.encodeOwnerName("a\ud800b"),
    "rejects an unpaired surrogate on encode",
  );
  check(m.encodeOwnerName("a😀b").length === 6, "while a surrogate *pair* is ordinary text");

  // Over `max` is a decode error too.
  checkThrows(
    m.DefgenLengthError,
    () => m.decodeOwnerName(new Uint8Array(33)),
    "a 33-byte buffer",
  );
}

// ---------------------------------------------------------------------------

function testMetadata() {
  check(
    m.HEARING_AID_CONTROL_UUID === "7d8f0000-3c1a-4e8a-9b5a-000000000000",
    "service UUID",
  );
  check(
    m.HEARING_AID_CONTROL_STATUS_CHAR_UUID === "7d8f0001-3c1a-4e8a-9b5a-000000000000",
    "characteristic UUID",
  );

  check(m.SERVICES.length === 1, "SERVICES lists every service");
  const service = m.SERVICES[0];
  check(service === m.HEARING_AID_CONTROL, "by the same object the module exports");
  check(service.name === "HearingAidControl", "the service keeps its schema name");
  check(service.characteristics.length === 8, "with all eight characteristics, in source order");
  check(Object.isFrozen(service), "and is frozen");

  const statusChar = service.characteristics[0];
  check(statusChar.name === "StatusChar", "characteristics are in source order");
  check(
    statusChar.properties === (m.GattProperty.READ | m.GattProperty.NOTIFY),
    "properties are a flag set",
  );
  check(
    service.characteristics[1].properties ===
      (m.GattProperty.WRITE | m.GattProperty.WRITE_WITHOUT_RESPONSE),
    "write properties",
  );
  check(
    (statusChar.properties & m.GattProperty.READ) !== 0,
    "and support a membership test",
  );
}

function testConstants() {
  check(m.MAX_WRITE_LENGTH === 32, "MAX_WRITE_LENGTH");
  check(m.MIN_RATED_TEMPERATURE === -40, "MIN_RATED_TEMPERATURE");
}

function main() {
  testStatus();
  testOpenEnum();
  testEndianness();
  testBigEndianRecord();
  testBigEndianSequences();
  testTemperatureLog();
  testCommand();
  testCommandUnknown();
  testDiagnosticLabel();
  testOwnerName();
  testMetadata();
  testConstants();

  if (failures === 0) {
    console.log("ok");
  }
  return failures;
}

process.exit(main());
