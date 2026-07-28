// Conformance fixture for the Swift backend.
//
// The Rust test generates `commands.swift` from `commands.defs`, drops this
// file next to it, compiles both together and runs the result. Everything
// here is a claim about the *wire format*: a hand-written byte string on one
// side, the decoded value on the other. Bit offsets are worked out from
// SPEC.md §6 by hand rather than read back out of the generated file, so a
// bug in the emitter's layout arithmetic cannot agree with itself into a
// passing test.
//
// The byte strings are the very same ones `c_conformance.c`,
// `python_conformance.py` and `kotlin_conformance.kt` assert — that is the
// point of §14: several backends, one wire format. `defgenRound` is private
// to the generated file, so the rounding table those other fixtures check
// directly is exercised here through `temperatureToRaw`/`temperatureFromRaw`
// instead — the only public door onto it a Swift caller has.
//
// Exit status is the number of failures.

#if canImport(Glibc)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

var failures = 0

func check(_ cond: Bool, _ what: String) {
    if !cond {
        print("FAIL: \(what)")
        failures += 1
    }
}

func hex(_ bytes: [UInt8]) -> String {
    bytes.map { b in b < 16 ? "0" + String(b, radix: 16) : String(b, radix: 16) }.joined(separator: " ")
}

func checkBytes(_ got: [UInt8], _ want: [UInt8], _ what: String) {
    if got == want { return }
    print("FAIL: \(what)")
    print("  want: \(hex(want))")
    print("  got:  \(hex(got))")
    failures += 1
}

func isLength(_ e: DefgenError) -> Bool { if case .length = e { return true }; return false }
func isRange(_ e: DefgenError) -> Bool { if case .range = e { return true }; return false }
func isUnknownValue(_ e: DefgenError) -> Bool { if case .unknownValue = e { return true }; return false }
func isPadding(_ e: DefgenError) -> Bool { if case .padding = e { return true }; return false }
func isUtf8(_ e: DefgenError) -> Bool { if case .utf8 = e { return true }; return false }

func checkThrows(_ what: String, _ matches: (DefgenError) -> Bool, _ block: () throws -> Void) {
    do {
        try block()
    } catch let error as DefgenError {
        if matches(error) { return }
        print("FAIL: \(what): threw the wrong DefgenError case (\(error))")
        failures += 1
        return
    } catch {
        print("FAIL: \(what): threw a non-DefgenError (\(error))")
        failures += 1
        return
    }
    print("FAIL: \(what): did not throw")
    failures += 1
}

// ---------------------------------------------------------------------------
// Struct: bit packing, nesting, scaled fields, reserved bits (§6)
// ---------------------------------------------------------------------------
//
// Status is 64 little-endian bits, packed LSB-first in declaration order:
//
//   [0..4)   activeProfile  u4
//   [4..8)   volume         u4  (alias of u4)
//   [8..12)  mode           u4  (open enum)
//   [12..13) muted          bool
//   [13..21) battery        u8  (scaled, 0.02)
//   [21..45) orientation    3 x i8
//   [45..60) padding        u15
//   [60..64) reserved flags u4

func testStatus() throws {
    check(Status.size == 8, "Status.size == 8")

    let s = Status(
        activeProfile: 0x3,
        volume: 0xA,
        mode: .cinema,
        muted: true,
        battery: 2.0, // raw 100 = 0x64
        orientation: Orientation(x: -1, y: 2, z: -128),
        flags: 0x5
    )

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
    let buf = try s.encode()
    check(buf.count == 8, "Status encodes to 8 bytes")
    checkBytes(buf, [0xA3, 0x93, 0xEC, 0x5F, 0x00, 0x10, 0x00, 0x50], "Status bytes")

    let back = try Status.decode(buf)
    check(back.activeProfile == 0x3, "Status.activeProfile round-trips")
    check(back.volume == 0xA, "Status.volume round-trips")
    check(back.mode == .cinema, "Status.mode round-trips")
    check(back.muted == true, "Status.muted round-trips")
    check(1.99 < back.battery && back.battery < 2.01, "Status.battery round-trips")
    check(back.orientation.x == -1, "Orientation.x is sign-extended, not 255")
    check(back.orientation.y == 2, "Orientation.y round-trips")
    check(back.orientation.z == -128, "Orientation.z round-trips")
    check(back.flags == 0x5, "reserved bits round-trip (§6.2)")

    // A value too wide for its field is a hard error, never a truncation.
    // `activeProfile` is carried in a `UInt8` (§2's carrier rule), which holds
    // 0..255 — but the wire width is only 4 bits, so 16 is a legal `UInt8`
    // that still has to fail the encode-side range check.
    var overflow = s
    overflow.activeProfile = 16
    checkThrows("u4 overflow is a range error", isRange) { _ = try overflow.encode() }
    checkThrows("short buffer", isLength) { _ = try Status.decode(Array(buf[0..<7])) }
    checkThrows("long buffer", isLength) { _ = try Status.decode(buf + [0]) }
}

func testOpenEnum() throws {
    // An open enum keeps an unrecognized wire value rather than failing (§5).
    check(HearingMode.stereo.raw == 1, "HearingMode.stereo == 1")

    var buf = [UInt8](repeating: 0, count: 8)
    buf[1] = 0x09 // mode = 9, undeclared
    let s = try Status.decode(buf)
    guard case .unknown(let raw) = s.mode else {
        check(false, "unknown mode decodes to the else case")
        return
    }
    check(raw == 9, "keeping the wire value it came from")
    check(s.mode != .stereo, "and never to a declared variant")

    // and re-encodes to exactly the bytes it came from.
    checkBytes(try s.encode(), buf, "unknown enum value re-encodes verbatim")
}

// ---------------------------------------------------------------------------
// Byte order (§8)
// ---------------------------------------------------------------------------

func testEndianness() throws {
    // LegacySerial carries #[endian(big)]: its one value reads MSB-first.
    let s = LegacySerial(serial: 0x0102_0304)
    let buf = try s.encode()
    checkBytes(buf, [0x01, 0x02, 0x03, 0x04], "LegacySerial is big-endian")
    check(try LegacySerial.decode(buf).serial == 0x0102_0304, "LegacySerial round-trips")

    // while the file default is little-endian.
    let statusBuf = try Status(activeProfile: 1).encode()
    check(statusBuf[0] == 0x01, "Status is little-endian: low bits land in byte 0")
    check(statusBuf[7] == 0x00, "Status is little-endian: byte 7 is untouched")
}

// A big-endian container with more than one field: they stay in declaration
// order, first field in the first byte, and only the direction each multi-byte
// value reads in changes (§8). The nested Orientation is flattened into this
// container and picks up its byte order.
func testBigEndianRecord() throws {
    let r = LegacyReading(
        id_: 0x11,
        value: 0x2233,
        orientation: Orientation(x: 1, y: -2, z: 3),
        crc: 0x4455
    )
    let buf = try r.encode()
    checkBytes(
        buf,
        [0x11, 0x22, 0x33, 0x01, 0xfe, 0x03, 0x44, 0x55],
        "LegacyReading keeps its fields in declaration order"
    )
    check(try LegacyReading.decode(buf) == r, "LegacyReading round-trips")
}

// A fixed array and a variable-length tail in one big-endian container: both
// keep their elements in declaration order, with byte order applying inside an
// element and never across elements (§8).
func testBigEndianSequences() throws {
    let key: [UInt8] = [0xde, 0xad, 0xbe, 0xef]
    // raw 100 = 0x0064, raw -200 = 0xff38
    let log = LegacyLog(key: key, samples: [1.0, -2.0])
    let buf = try log.encode()
    checkBytes(
        buf,
        [0xde, 0xad, 0xbe, 0xef, 0x00, 0x64, 0xff, 0x38],
        "LegacyLog keeps its array and tail elements in order"
    )
    let back = try LegacyLog.decode(buf)
    check(back.key == key, "LegacyLog's fixed array round-trips")
    check(try temperatureToRaw(back.samples[0]) == 100, "LegacyLog sample 0")
    check(try temperatureToRaw(back.samples[1]) == -200, "LegacyLog sample 1")

    // An empty tail is just the prefix (§6.3).
    checkBytes(
        try LegacyLog(key: key, samples: []).encode(),
        [0xde, 0xad, 0xbe, 0xef],
        "LegacyLog with no samples is its prefix alone"
    )
}

// ---------------------------------------------------------------------------
// Arrays (§6.1) and scaled types (§4)
// ---------------------------------------------------------------------------

func testTemperatureLog() throws {
    let log = TemperatureLog(
        samples: [
            21.5, // raw  2150 = 0x0866
            -0.01, // raw    -1 = 0xffff
            0.0,
            327.67, // raw 32767, the i16 maximum
        ]
    )

    let buf = try log.encode()
    checkBytes(buf, [0x66, 0x08, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x7F], "TemperatureLog bytes")

    let back = try TemperatureLog.decode(buf)
    check(21.49 < back.samples[0] && back.samples[0] < 21.51, "samples[0] round-trips")
    check(
        try temperatureToRaw(back.samples[1]) == -1,
        "samples[1] is sign-extended exactly once, i.e. raw -1 and not -65537"
    )
    check(back.samples[3] > 327.66, "samples[3] round-trips")

    // The raw integer stays reachable, so a round trip need not go through
    // floating point at all (§4).
    check(try temperatureToRaw(21.5) == 2150, "temperatureToRaw")
    check(temperatureFromRaw(2150) > 21.49, "temperatureFromRaw")

    // Rounding decides the raw unit: truncation would send both of these to 2151.
    check(try temperatureToRaw(21.514) == 2151, "rounds down below the half")
    check(try temperatureToRaw(21.516) == 2152, "and up above it")
    check(try temperatureToRaw(-21.514) == -2151, "symmetrically below zero")
    check(try temperatureToRaw(-21.516) == -2152, "on both sides")
    // The exact tie: half away from zero, never half to even.
    check(try temperatureToRaw(0.005) == 1, "a tie rounds away from zero")
    check(try temperatureToRaw(-0.005) == -1, "on both sides of it")

    // Out of the raw type's range is an error, not a wrap.
    checkThrows("raw overflow", isRange) { _ = try temperatureToRaw(1000.0) }
    checkThrows("raw underflow", isRange) { _ = try temperatureToRaw(-1000.0) }
    checkThrows("NaN is rejected", isRange) { _ = try temperatureToRaw(Float.nan) }
    checkThrows("inf is rejected", isRange) { _ = try temperatureToRaw(Float.infinity) }
    checkThrows("and -inf", isRange) { _ = try temperatureToRaw(-Float.infinity) }

    // A fixed array carries exactly its declared count, always (§6.1).
    checkThrows("a short array fails to encode", isRange) {
        _ = try TemperatureLog(samples: [0.0, 0.0]).encode()
    }
    check(TemperatureLog().samples == [0.0, 0.0, 0.0, 0.0], "the default array is full-length")
}

func testPadding() throws {
    // `padding: uN = 0` is validated on decode; bare padding is not (§6.2).
    _ = try MotionPath.unpackFixed(DefgenBits(data: [UInt8](repeating: 0, count: 8), big: false), 0) // all-zero padding is fine

    var buf = [UInt8](repeating: 0, count: 8)
    buf[7] = 0x01 // inside the `padding: u16 = 0` run at bits 48..64
    checkThrows("non-zero `padding = 0` is rejected", isPadding) {
        _ = try MotionPath.unpackFixed(DefgenBits(data: buf, big: false), 0)
    }

    // Status's bare padding at bits 45..60 is ignored rather than validated.
    var noisy = try Status(activeProfile: 1).encode()
    noisy[6] = 0xFF
    _ = try Status.decode(noisy)
}

// ---------------------------------------------------------------------------
// Tagged unions (§7)
// ---------------------------------------------------------------------------

func testCommand() throws {
    check(Command.size == 8, "Command.size == 8")

    // A known variant: 16-bit tag in the low bits, payload above it.
    var buf = try Command.setVolume(volume: 7).encode()
    checkBytes(buf, [0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00], "SetVolume bytes")

    var back = try Command.decode(buf)
    guard case .setVolume(let volume) = back else {
        check(false, "decodes to the setVolume case")
        return
    }
    check(volume == 7, "SetVolume.volume round-trips")

    // A variant with no payload leaves the whole payload region zero.
    checkBytes(
        try Command.triggerFactoryReset.encode(),
        [0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        "TriggerFactoryReset bytes"
    )

    // A nested struct inside a variant is flattened into the payload region.
    buf = try Command.setOrientationOffset(offset: Orientation(x: 1, y: 2, z: 3)).encode()
    checkBytes(buf, [0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00], "SetOrientationOffset bytes")
    back = try Command.decode(buf)
    guard case .setOrientationOffset(let offset) = back else {
        check(false, "decodes to setOrientationOffset")
        return
    }
    check(offset.z == 3, "the nested struct round-trips")

    // A variant's own encode dispatches on itself.
    checkBytes(
        try Command.setMute(muted: true).encode(),
        [0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
        "SetMute bytes"
    )
}

func testCommandUnknown() throws {
    // An unrecognized id decodes to the `unknown` case, keeping both id and
    // the undecoded payload, and never silently becomes a known variant (§7).
    var buf = [UInt8](repeating: 0, count: 8)
    buf[0] = 0x34
    buf[1] = 0x12 // id = 0x1234, undeclared
    buf[2] = 0xAB

    let back = try Command.decode(buf)
    guard case .unknown(let id, let raw) = back else {
        check(false, "an unknown id decodes to the unknown case")
        return
    }
    check(id == 0x1234, "the unknown id is kept")
    check(raw == 0xAB, "and so is the undecoded payload")

    // and re-encodes to exactly the bytes it came from.
    checkBytes(try back.encode(), buf, "an unknown command re-encodes verbatim")
}

// ---------------------------------------------------------------------------
// Variable-length values (§6.3)
// ---------------------------------------------------------------------------

func testDiagnosticLabel() throws {
    check(DiagnosticLabel.fixedSize == 1, "DiagnosticLabel.fixedSize")
    check(DiagnosticLabel.maxSize == 25, "DiagnosticLabel.maxSize")

    let d = DiagnosticLabel(severity: 3, label: "hi")

    // The encoding is exactly prefix + actual tail — never padded to max.
    check(d.encodedSize() == 3, "encodedSize is prefix + tail")
    let buf = try d.encode()
    check(buf.count == 3, "and so is the encoding itself")
    checkBytes(buf, [0x03, UInt8(ascii: "h"), UInt8(ascii: "i")], "DiagnosticLabel bytes")

    let back = try DiagnosticLabel.decode(buf)
    check(back.severity == 3, "severity round-trips")
    check(back.label == "hi", "the tail round-trips")

    // An empty tail is legal: the value is just its fixed prefix.
    let empty = try DiagnosticLabel(severity: 3, label: "").encode()
    checkBytes(empty, [0x03], "an empty tail is just the prefix")
    check(try DiagnosticLabel.decode(empty).label == "", "and decodes back to empty")

    // Over `max` fails on encode, and a too-long buffer fails on decode.
    checkThrows("a tail over `max` fails to encode", isRange) {
        _ = try DiagnosticLabel(severity: 0, label: String(repeating: "x", count: 25)).encode()
    }
    checkThrows("a buffer over maxSize fails to decode", isLength) {
        _ = try DiagnosticLabel.decode([0] + [UInt8](repeating: UInt8(ascii: "x"), count: 25))
    }
    checkThrows("an empty buffer too", isLength) { _ = try DiagnosticLabel.decode([]) }

    // `max` bounds bytes, not characters: 12 two-byte characters fit, 13 do not.
    check(
        try DiagnosticLabel(severity: 0, label: String(repeating: "\u{e9}", count: 12)).encode().count == 25,
        "12 x é fits"
    )
    checkThrows("13 x é does not", isRange) {
        _ = try DiagnosticLabel(severity: 0, label: String(repeating: "\u{e9}", count: 13)).encode()
    }
}

func testOwnerName() throws {
    // An alias of a variable-length type binds straight to a characteristic.
    check(OWNER_NAME_FIXED_SIZE == 0, "OWNER_NAME_FIXED_SIZE")
    check(OWNER_NAME_MAX_SIZE == 32, "OWNER_NAME_MAX_SIZE")

    var buf = try encodeOwnerName("Ada")
    checkBytes(buf, [UInt8(ascii: "A"), UInt8(ascii: "d"), UInt8(ascii: "a")], "OwnerName bytes")
    check(try decodeOwnerName(buf) == "Ada", "OwnerName round-trips")

    // Multi-byte UTF-8 survives, and the byte count is what `max` bounds.
    buf = try encodeOwnerName("\u{e9}")
    check(buf.count == 2, "é is two bytes on the wire")
    check(try decodeOwnerName(buf) == "\u{e9}", "and round-trips")
    checkThrows("17 x é exceeds max: 32", isRange) {
        _ = try encodeOwnerName(String(repeating: "\u{e9}", count: 17))
    }

    // Malformed UTF-8 fails rather than being replaced (§6.3).
    checkThrows("rejects a byte no encoding starts with", isUtf8) { _ = try decodeOwnerName([0xFF]) }
    checkThrows("rejects a lead byte with no continuation", isUtf8) { _ = try decodeOwnerName([0xC3]) }
    checkThrows("rejects an overlong encoding of U+0000", isUtf8) { _ = try decodeOwnerName([0xC0, 0x80]) }
    checkThrows("rejects a surrogate, U+D800", isUtf8) { _ = try decodeOwnerName([0xED, 0xA0, 0x80]) }

    // Over `max` is a decode error too.
    checkThrows("a 33-byte buffer", isLength) {
        _ = try decodeOwnerName([UInt8](repeating: UInt8(ascii: "x"), count: 33))
    }
}

// ---------------------------------------------------------------------------

func testMetadata() throws {
    check(HEARING_AID_CONTROL_UUID == "7d8f0000-3c1a-4e8a-9b5a-000000000000", "service UUID")
    check(
        HEARING_AID_CONTROL_STATUS_CHAR_UUID == "7d8f0001-3c1a-4e8a-9b5a-000000000000",
        "characteristic UUID"
    )

    check(SERVICES == [HEARING_AID_CONTROL], "SERVICES lists every service")
    let service = SERVICES[0]
    check(service.name == "HearingAidControl", "the service keeps its schema name")
    check(service.characteristics.count == 8, "with all eight characteristics, in source order")

    let statusChar = service.characteristics[0]
    check(statusChar.name == "StatusChar", "characteristics are in source order")
    check(statusChar.properties == [.read, .notify], "properties are a flag set")
    check(
        service.characteristics[1].properties == [.write, .writeWithoutResponse],
        "write properties"
    )
    check(statusChar.properties.contains(.read), "and support `.contains`")
}

func testConstants() {
    check(MAX_WRITE_LENGTH == 32, "MAX_WRITE_LENGTH")
    check(MIN_RATED_TEMPERATURE == -40, "MIN_RATED_TEMPERATURE")
}

func main() throws {
    try testStatus()
    try testOpenEnum()
    try testEndianness()
    try testBigEndianRecord()
    try testBigEndianSequences()
    try testTemperatureLog()
    try testPadding()
    try testCommand()
    try testCommandUnknown()
    try testDiagnosticLabel()
    try testOwnerName()
    try testMetadata()
    testConstants()

    if failures == 0 {
        print("ok")
    }
}

try main()
exit(Int32(failures))
