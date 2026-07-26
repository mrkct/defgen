/**
 * Conformance fixture for the Kotlin backend.
 *
 * The Rust test generates `commands.kt` from `commands.defs`, drops this file
 * next to it, compiles both together and runs the result. Everything here is
 * a claim about the *wire format*: a hand-written byte string on one side,
 * the decoded value on the other. Bit offsets are worked out from SPEC.md §6
 * by hand rather than read back out of the generated file, so a bug in the
 * emitter's layout arithmetic cannot agree with itself into a passing test.
 *
 * The byte strings are the very same ones `c_conformance.c` and
 * `python_conformance.py` assert — that is the point of §14: three backends,
 * one wire format. `defgenRound` is `private` to the generated file (real
 * file-level privacy, unlike Python's leading-underscore convention), so the
 * rounding table those two fixtures check directly is exercised here through
 * `temperatureToRaw`/`temperatureFromRaw` instead — the only public door onto
 * it a Kotlin caller has.
 *
 * Exit status is the number of failures.
 */

import java.math.BigInteger

var failures = 0

fun check(cond: Boolean, what: String) {
    if (!cond) {
        println("FAIL: $what")
        failures++
    }
}

fun checkBytes(got: ByteArray, want: ByteArray, what: String) {
    if (got.contentEquals(want)) return
    println("FAIL: $what")
    println("  want: ${want.joinToString(" ") { "%02x".format(it) }}")
    println("  got:  ${got.joinToString(" ") { "%02x".format(it) }}")
    failures++
}

inline fun <reified T : DefgenError> checkThrows(what: String, block: () -> Unit) {
    try {
        block()
    } catch (e: Throwable) {
        if (e is T) return
        println("FAIL: $what: threw ${e::class.simpleName} ($e), want ${T::class.simpleName}")
        failures++
        return
    }
    println("FAIL: $what: did not throw ${T::class.simpleName}")
    failures++
}

fun u(v: Int) = v.toUByte()
fun us(v: Int) = v.toUShort()

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

fun testStatus() {
    check(Status.SIZE == 8, "Status.SIZE == 8")

    val s = Status(
        activeProfile = u(0x3),
        volume = u(0xA),
        mode = HearingMode.Cinema,
        muted = true,
        battery = 2.0f, // raw 100 = 0x64
        orientation = Orientation(x = (-1).toByte(), y = 2.toByte(), z = (-128).toByte()),
        flags = u(0x5),
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
    val buf = s.encode()
    check(buf.size == 8, "Status encodes to 8 bytes")
    checkBytes(buf, byteArrayOf(0xA3.toByte(), 0x93.toByte(), 0xEC.toByte(), 0x5F, 0x00, 0x10, 0x00, 0x50), "Status bytes")

    val back = Status.decode(buf)
    check(back.activeProfile == u(0x3), "Status.activeProfile round-trips")
    check(back.volume == u(0xA), "Status.volume round-trips")
    check(back.mode === HearingMode.Cinema, "Status.mode round-trips")
    check(back.muted == true, "Status.muted round-trips")
    check(1.99f < back.battery && back.battery < 2.01f, "Status.battery round-trips")
    check(back.orientation.x == (-1).toByte(), "Orientation.x is sign-extended, not 255")
    check(back.orientation.y == 2.toByte(), "Orientation.y round-trips")
    check(back.orientation.z == (-128).toByte(), "Orientation.z round-trips")
    check(back.flags == u(0x5), "reserved bits round-trip (§6.2)")

    // A value too wide for its field is a hard error, never a truncation.
    // `activeProfile` is carried in a `UByte` (§2's carrier rule), which holds
    // 0..255 — but the wire width is only 4 bits, so 16 is a legal `UByte`
    // that still has to fail the encode-side range check.
    checkThrows<DefgenRangeError>("u4 overflow is a range error") {
        s.copy(activeProfile = 16.toUByte()).encode()
    }
    checkThrows<DefgenLengthError>("short buffer") { Status.decode(buf.copyOfRange(0, 7)) }
    checkThrows<DefgenLengthError>("long buffer") { Status.decode(buf + byteArrayOf(0)) }
}

fun testOpenEnum() {
    // An open enum keeps an unrecognized wire value rather than failing (§5).
    check(HearingMode.Stereo.raw == u(1), "HearingMode.Stereo == 1")

    val buf = ByteArray(8)
    buf[1] = 0x09 // mode = 9, undeclared
    val s = Status.decode(buf)
    check(s.mode is HearingMode.Unknown, "unknown mode decodes to the else variant")
    check(s.mode !is HearingMode.Stereo, "and never to a declared variant")
    check((s.mode as HearingMode.Unknown).raw == u(9), "keeping the wire value it came from")

    // and re-encodes to exactly the bytes it came from.
    checkBytes(s.encode(), buf, "unknown enum value re-encodes verbatim")
}

// ---------------------------------------------------------------------------
// Byte order (§8)
// ---------------------------------------------------------------------------

fun testEndianness() {
    // LegacySerial carries #[endian(big)]: the flattened bit sequence is
    // written from the far end of the buffer.
    val s = LegacySerial(serial = 0x01020304u)
    val buf = s.encode()
    checkBytes(buf, byteArrayOf(0x01, 0x02, 0x03, 0x04), "LegacySerial is big-endian")
    check(LegacySerial.decode(buf).serial == 0x01020304u, "LegacySerial round-trips")

    // while the file default is little-endian.
    val statusBuf = Status(activeProfile = u(1)).encode()
    check(statusBuf[0] == 0x01.toByte(), "Status is little-endian: low bits land in byte 0")
    check(statusBuf[7] == 0x00.toByte(), "Status is little-endian: byte 7 is untouched")
}

// ---------------------------------------------------------------------------
// Arrays (§6.1) and scaled types (§4)
// ---------------------------------------------------------------------------

fun testTemperatureLog() {
    val log = TemperatureLog(
        samples = listOf(
            21.5f, // raw  2150 = 0x0866
            -0.01f, // raw    -1 = 0xffff
            0.0f,
            327.67f, // raw 32767, the i16 maximum
        )
    )

    val buf = log.encode()
    checkBytes(
        buf,
        byteArrayOf(0x66, 0x08, 0xFF.toByte(), 0xFF.toByte(), 0x00, 0x00, 0xFF.toByte(), 0x7F),
        "TemperatureLog bytes",
    )

    val back = TemperatureLog.decode(buf)
    check(21.49f < back.samples[0] && back.samples[0] < 21.51f, "samples[0] round-trips")
    check(back.samples[1] < 0.0f, "samples[1] is negative, i.e. sign-extended")
    check(back.samples[3] > 327.66f, "samples[3] round-trips")

    // The raw integer stays reachable, so a round trip need not go through
    // floating point at all (§4).
    check(temperatureToRaw(21.5f) == 2150.toShort(), "temperatureToRaw")
    check(temperatureFromRaw(2150.toShort()) > 21.49f, "temperatureFromRaw")

    // Rounding decides the raw unit: truncation would send both of these to 2151.
    check(temperatureToRaw(21.514f) == 2151.toShort(), "rounds down below the half")
    check(temperatureToRaw(21.516f) == 2152.toShort(), "and up above it")
    check(temperatureToRaw(-21.514f) == (-2151).toShort(), "symmetrically below zero")
    check(temperatureToRaw(-21.516f) == (-2152).toShort(), "on both sides")
    // The exact tie: half away from zero, never half to even.
    check(temperatureToRaw(0.005f) == 1.toShort(), "a tie rounds away from zero")
    check(temperatureToRaw(-0.005f) == (-1).toShort(), "on both sides of it")

    // Out of the raw type's range is an error, not a wrap.
    checkThrows<DefgenRangeError>("raw overflow") { temperatureToRaw(1000.0f) }
    checkThrows<DefgenRangeError>("raw underflow") { temperatureToRaw(-1000.0f) }
    checkThrows<DefgenRangeError>("NaN is rejected") { temperatureToRaw(Float.NaN) }
    checkThrows<DefgenRangeError>("inf is rejected") { temperatureToRaw(Float.POSITIVE_INFINITY) }
    checkThrows<DefgenRangeError>("and -inf") { temperatureToRaw(Float.NEGATIVE_INFINITY) }

    // A fixed array carries exactly its declared count, always (§6.1).
    checkThrows<DefgenRangeError>("a short array fails to encode") {
        TemperatureLog(samples = listOf(0.0f, 0.0f)).encode()
    }
    check(
        TemperatureLog().samples == listOf(0.0f, 0.0f, 0.0f, 0.0f),
        "the default array is full-length",
    )
    check(
        TemperatureLog().samples !== TemperatureLog().samples,
        "and each instance gets its own list — a Kotlin default argument is a fresh " +
            "expression per call, not a value built once and shared",
    )
}

fun testPadding() {
    // `padding: uN = 0` is validated on decode; bare padding is not (§6.2).
    MotionPath.unpackFixed(DefgenBits.fromBytes(ByteArray(8), big = false), 0) // all-zero padding is fine

    val buf = ByteArray(8)
    buf[7] = 0x01 // inside the `padding: u16 = 0` run at bits 48..64
    checkThrows<DefgenPaddingError>("non-zero `padding = 0` is rejected") {
        MotionPath.unpackFixed(DefgenBits.fromBytes(buf, big = false), 0)
    }

    // Status's bare padding at bits 45..60 is ignored rather than validated.
    val noisy = Status(activeProfile = u(1)).encode()
    noisy[6] = 0xFF.toByte()
    Status.decode(noisy)
}

// ---------------------------------------------------------------------------
// Tagged unions (§7)
// ---------------------------------------------------------------------------

fun testCommand() {
    check(Command.SIZE == 8, "Command.SIZE == 8")
    check(Command.SetVolume.ID == us(0x0001), "SetVolume's wire id")
    check(Command.TriggerFactoryReset.ID == us(0xFFFF), "TriggerFactoryReset's wire id")

    // A known variant: 16-bit tag in the low bits, payload above it.
    var buf = Command.SetVolume(volume = u(7)).encode()
    checkBytes(buf, byteArrayOf(0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00), "SetVolume bytes")

    var back = Command.decode(buf)
    check(back is Command.SetVolume, "decodes to the SetVolume class")
    check((back as Command.SetVolume).volume == u(7), "SetVolume.volume round-trips")

    // A variant with no payload leaves the whole payload region zero.
    checkBytes(
        Command.TriggerFactoryReset.encode(),
        byteArrayOf(0xFF.toByte(), 0xFF.toByte(), 0x00, 0x00, 0x00, 0x00, 0x00, 0x00),
        "TriggerFactoryReset bytes",
    )

    // A nested struct inside a variant is flattened into the payload region.
    buf = Command.SetOrientationOffset(offset = Orientation(1.toByte(), 2.toByte(), 3.toByte())).encode()
    checkBytes(buf, byteArrayOf(0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00), "SetOrientationOffset bytes")
    back = Command.decode(buf)
    check(back is Command.SetOrientationOffset, "decodes to SetOrientationOffset")
    check((back as Command.SetOrientationOffset).offset.z == 3.toByte(), "the nested struct round-trips")

    // A variant's own encode is the one it inherits, and dispatches on itself.
    checkBytes(
        Command.SetMute(muted = true).encode(),
        byteArrayOf(0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00),
        "SetMute bytes",
    )
}

fun testCommandUnknown() {
    // An unrecognized id decodes to the `else` variant, keeping both id and
    // the undecoded payload, and never silently becomes a known variant (§7).
    val buf = ByteArray(8)
    buf[0] = 0x34
    buf[1] = 0x12 // id = 0x1234, undeclared
    buf[2] = 0xAB.toByte()

    val back = Command.decode(buf)
    check(back is Command.Unknown, "an unknown id decodes to the else variant")
    check((back as Command.Unknown).id == us(0x1234), "the unknown id is kept")
    check(back.raw == 0xABuL, "and so is the undecoded payload")

    // and re-encodes to exactly the bytes it came from.
    checkBytes(back.encode(), buf, "an unknown command re-encodes verbatim")
}

// ---------------------------------------------------------------------------
// Variable-length values (§6.3)
// ---------------------------------------------------------------------------

fun testDiagnosticLabel() {
    check(DiagnosticLabel.FIXED_SIZE == 1, "DiagnosticLabel.FIXED_SIZE")
    check(DiagnosticLabel.MAX_SIZE == 25, "DiagnosticLabel.MAX_SIZE")

    val d = DiagnosticLabel(severity = u(3), label = "hi")

    // The encoding is exactly prefix + actual tail — never padded to max.
    check(d.encodedSize() == 3, "encodedSize is prefix + tail")
    val buf = d.encode()
    check(buf.size == 3, "and so is the encoding itself")
    checkBytes(buf, byteArrayOf(0x03, 'h'.code.toByte(), 'i'.code.toByte()), "DiagnosticLabel bytes")

    val back = DiagnosticLabel.decode(buf)
    check(back.severity == u(3), "severity round-trips")
    check(back.label == "hi", "the tail round-trips")

    // An empty tail is legal: the value is just its fixed prefix.
    val empty = DiagnosticLabel(severity = u(3), label = "").encode()
    checkBytes(empty, byteArrayOf(0x03), "an empty tail is just the prefix")
    check(DiagnosticLabel.decode(empty).label == "", "and decodes back to empty")

    // Over `max` fails on encode, and a too-long buffer fails on decode.
    checkThrows<DefgenRangeError>("a tail over `max` fails to encode") {
        DiagnosticLabel(severity = u(0), label = "x".repeat(25)).encode()
    }
    checkThrows<DefgenLengthError>("a buffer over MAX_SIZE fails to decode") {
        DiagnosticLabel.decode(byteArrayOf(0) + ByteArray(25) { 'x'.code.toByte() })
    }
    checkThrows<DefgenLengthError>("an empty buffer too") { DiagnosticLabel.decode(ByteArray(0)) }

    // `max` bounds bytes, not characters: 12 two-byte characters fit, 13 do not.
    check(DiagnosticLabel(severity = u(0), label = "é".repeat(12)).encode().size == 25, "12 x é fits")
    checkThrows<DefgenRangeError>("13 x é does not") {
        DiagnosticLabel(severity = u(0), label = "é".repeat(13)).encode()
    }
}

fun testOwnerName() {
    // An alias of a variable-length type binds straight to a characteristic.
    check(OWNER_NAME_FIXED_SIZE == 0, "OWNER_NAME_FIXED_SIZE")
    check(OWNER_NAME_MAX_SIZE == 32, "OWNER_NAME_MAX_SIZE")

    var buf = encodeOwnerName("Ada")
    checkBytes(buf, byteArrayOf('A'.code.toByte(), 'd'.code.toByte(), 'a'.code.toByte()), "OwnerName bytes")
    check(decodeOwnerName(buf) == "Ada", "OwnerName round-trips")

    // Multi-byte UTF-8 survives, and the byte count is what `max` bounds.
    buf = encodeOwnerName("é")
    check(buf.size == 2, "é is two bytes on the wire")
    check(decodeOwnerName(buf) == "é", "and round-trips")
    checkThrows<DefgenRangeError>("17 x é exceeds max: 32") { encodeOwnerName("é".repeat(17)) }

    // Malformed UTF-8 fails rather than being replaced (§6.3).
    checkThrows<DefgenUtf8Error>("rejects a byte no encoding starts with") {
        decodeOwnerName(byteArrayOf(0xFF.toByte()))
    }
    checkThrows<DefgenUtf8Error>("rejects a lead byte with no continuation") {
        decodeOwnerName(byteArrayOf(0xC3.toByte()))
    }
    checkThrows<DefgenUtf8Error>("rejects an overlong encoding of U+0000") {
        decodeOwnerName(byteArrayOf(0xC0.toByte(), 0x80.toByte()))
    }
    checkThrows<DefgenUtf8Error>("rejects a surrogate, U+D800") {
        decodeOwnerName(byteArrayOf(0xED.toByte(), 0xA0.toByte(), 0x80.toByte()))
    }

    // Over `max` is a decode error too.
    checkThrows<DefgenLengthError>("a 33-byte buffer") {
        decodeOwnerName(ByteArray(33) { 'x'.code.toByte() })
    }
}

// ---------------------------------------------------------------------------

fun testMetadata() {
    check(SCHEMA_VERSION == 2uL, "SCHEMA_VERSION")
    check(HEARING_AID_CONTROL_UUID == "7d8f0000-3c1a-4e8a-9b5a-000000000000", "service UUID")
    check(
        HEARING_AID_CONTROL_STATUS_CHAR_UUID == "7d8f0001-3c1a-4e8a-9b5a-000000000000",
        "characteristic UUID",
    )

    check(SERVICES == listOf(HEARING_AID_CONTROL), "SERVICES lists every service")
    val service = SERVICES[0]
    check(service.name == "HearingAidControl", "the service keeps its schema name")
    check(service.characteristics.size == 6, "with all six characteristics, in source order")

    val statusChar = service.characteristics[0]
    check(statusChar.name == "StatusChar", "characteristics are in source order")
    check(
        statusChar.properties == setOf(GattProperty.READ, GattProperty.NOTIFY),
        "properties are a flag set",
    )
    check(
        service.characteristics[1].properties == setOf(GattProperty.WRITE, GattProperty.WRITE_WITHOUT_RESPONSE),
        "write properties",
    )
    check(GattProperty.READ in statusChar.properties, "and support `in`")
}

fun main() {
    testStatus()
    testOpenEnum()
    testEndianness()
    testTemperatureLog()
    testPadding()
    testCommand()
    testCommandUnknown()
    testDiagnosticLabel()
    testOwnerName()
    testMetadata()

    if (failures == 0) {
        println("ok")
    }
    kotlin.system.exitProcess(failures)
}
