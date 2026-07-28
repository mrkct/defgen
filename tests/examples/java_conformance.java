/**
 * Conformance fixture for the Java backend.
 *
 * The Rust test generates `Commands.java` from `commands.defs`, drops this file
 * next to it as `Conformance.java`, compiles both together and runs the result.
 * Everything here is a claim about the *wire format*: a hand-written byte string
 * on one side, the decoded value on the other. Bit offsets are worked out from
 * SPEC.md §6 by hand rather than read back out of the generated file, so a bug
 * in the emitter's layout arithmetic cannot agree with itself into a passing
 * test.
 *
 * The byte strings are the very same ones `c_conformance.c`,
 * `python_conformance.py` and `kotlin_conformance.kt` assert — that is the point
 * of §14: several backends, one wire format. `defgenRound` is package-private to
 * the generated file, so the rounding table the C and Python fixtures check
 * directly is exercised here through `temperatureToRaw`/`temperatureFromRaw`
 * instead — the only public door onto it a Java caller has.
 *
 * Everything is spelled `Commands.X` because a generated file carries no
 * `package` declaration, and the default package cannot be imported from. A
 * project that drops the file into a package of its own writes
 * `import com.example.Commands.Status;` and just says `Status`.
 *
 * Exit status is the number of failures.
 */

import java.util.List;
import java.util.Set;

public class Conformance {
    static int failures = 0;

    /** A block that may fail the way a generated codec does. */
    interface Fallible {
        void run() throws Commands.DefgenError;
    }

    static void check(boolean cond, String what) {
        if (!cond) {
            System.out.println("FAIL: " + what);
            failures++;
        }
    }

    static void checkBytes(byte[] got, byte[] want, String what) {
        if (java.util.Arrays.equals(got, want)) {
            return;
        }
        System.out.println("FAIL: " + what);
        System.out.println("  want: " + hex(want));
        System.out.println("  got:  " + hex(got));
        failures++;
    }

    static String hex(byte[] data) {
        StringBuilder out = new StringBuilder();
        for (byte b : data) {
            out.append(String.format("%02x ", b));
        }
        return out.toString().trim();
    }

    static void checkThrows(Class<? extends Throwable> want, String what, Fallible block) {
        try {
            block.run();
        } catch (Throwable exc) {
            if (want.isInstance(exc)) {
                return;
            }
            System.out.println(
                    "FAIL: " + what + ": threw " + exc.getClass().getSimpleName() + " (" + exc
                            + "), want " + want.getSimpleName());
            failures++;
            return;
        }
        System.out.println("FAIL: " + what + ": did not throw " + want.getSimpleName());
        failures++;
    }

    static byte b(int v) {
        return (byte) v;
    }

    static short s(int v) {
        return (short) v;
    }

    /** A Status with everything but `activeProfile` left at its zero value. */
    static Commands.Status status(int activeProfile) {
        return new Commands.Status(
                b(activeProfile),
                b(0),
                new Commands.HearingMode.Default(),
                false,
                0.0f,
                new Commands.Orientation(),
                b(0));
    }

    // -----------------------------------------------------------------------
    // Struct: bit packing, nesting, scaled fields, reserved bits (§6)
    // -----------------------------------------------------------------------
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

    static void testStatus() throws Commands.DefgenError {
        check(Commands.Status.SIZE == 8, "Status.SIZE == 8");

        Commands.Status value = new Commands.Status(
                b(0x3),
                b(0xA),
                new Commands.HearingMode.Cinema(),
                true,
                2.0f, // raw 100 = 0x64
                new Commands.Orientation(b(-1), b(2), b(-128)),
                b(0x5));

        // With battery raw = 0x64 and orientation = (0xff, 0x02, 0x80), the
        // fields straddle byte boundaries — which is the point of the exercise:
        //
        //   byte 0  bits  0..8   profile 3 | volume 0xa << 4              -> 0xa3
        //   byte 1  bits  8..16  mode 3 | muted << 4 | battery[0..3] << 5 -> 0x93
        //   byte 2  bits 16..24  battery[3..8] | x[0..3] << 5             -> 0xec
        //   byte 3  bits 24..32  x[3..8] | y[0..3] << 5                   -> 0x5f
        //   byte 4  bits 32..40  y[3..8] | z[0..3] << 5                   -> 0x00
        //   byte 5  bits 40..48  z[3..8] | padding                        -> 0x10
        //   byte 6  bits 48..56  padding                                  -> 0x00
        //   byte 7  bits 56..64  padding | flags 5 << 4                   -> 0x50
        byte[] buf = value.encode();
        check(buf.length == 8, "Status encodes to 8 bytes");
        checkBytes(
                buf,
                new byte[] {b(0xA3), b(0x93), b(0xEC), b(0x5F), b(0x00), b(0x10), b(0x00), b(0x50)},
                "Status bytes");

        Commands.Status back = Commands.Status.decode(buf);
        check(back.activeProfile() == 0x3, "Status.activeProfile round-trips");
        check(back.volume() == 0xA, "Status.volume round-trips");
        check(back.mode().equals(new Commands.HearingMode.Cinema()), "Status.mode round-trips");
        check(back.muted(), "Status.muted round-trips");
        check(1.99f < back.battery() && back.battery() < 2.01f, "Status.battery round-trips");
        check(back.orientation().x() == -1, "Orientation.x is sign-extended, not 255");
        check(back.orientation().y() == 2, "Orientation.y round-trips");
        check(back.orientation().z() == -128, "Orientation.z round-trips");
        check(back.flags() == 0x5, "reserved bits round-trip (§6.2)");

        // A record's own equality covers the whole value at once.
        check(back.equals(value), "a decoded Status equals the one it was encoded from");

        // A value too wide for its field is a hard error, never a truncation.
        // `activeProfile` is carried in a `byte` (§2's widening rule), which
        // holds 0..127 — but the wire width is only 4 bits, so 16 is a legal
        // `byte` that still has to fail the encode-side range check.
        checkThrows(Commands.DefgenRangeError.class, "u4 overflow is a range error", () -> status(16).encode());
        checkThrows(
                Commands.DefgenLengthError.class,
                "short buffer",
                () -> Commands.Status.decode(java.util.Arrays.copyOfRange(buf, 0, 7)));
        checkThrows(
                Commands.DefgenLengthError.class,
                "long buffer",
                () -> Commands.Status.decode(new byte[9]));
    }

    static void testOpenEnum() throws Commands.DefgenError {
        // An open enum keeps an unrecognized wire value rather than failing (§5).
        check(new Commands.HearingMode.Stereo().raw() == 1, "HearingMode.Stereo == 1");

        byte[] buf = new byte[8];
        buf[1] = 0x09; // mode = 9, undeclared
        Commands.Status value = Commands.Status.decode(buf);
        check(value.mode() instanceof Commands.HearingMode.Unknown, "unknown mode decodes to the else variant");
        check(
                !(value.mode() instanceof Commands.HearingMode.Stereo),
                "and never to a declared variant");
        check(
                ((Commands.HearingMode.Unknown) value.mode()).raw() == 9,
                "keeping the wire value it came from");

        // and re-encodes to exactly the bytes it came from.
        checkBytes(value.encode(), buf, "unknown enum value re-encodes verbatim");
    }

    // -----------------------------------------------------------------------
    // Byte order (§8)
    // -----------------------------------------------------------------------

    static void testEndianness() throws Commands.DefgenError {
        // LegacySerial carries #[endian(big)]: its one value reads MSB-first.
        byte[] buf = new Commands.LegacySerial(0x01020304L).encode();
        checkBytes(buf, new byte[] {0x01, 0x02, 0x03, 0x04}, "LegacySerial is big-endian");
        check(Commands.LegacySerial.decode(buf).serial() == 0x01020304L, "LegacySerial round-trips");

        // while the file default is little-endian.
        byte[] statusBuf = status(1).encode();
        check(statusBuf[0] == 0x01, "Status is little-endian: low bits land in byte 0");
        check(statusBuf[7] == 0x00, "Status is little-endian: byte 7 is untouched");
    }

    // A big-endian container with more than one field: they stay in declaration
    // order, first field in the first byte, and only the direction each
    // multi-byte value reads in changes (§8). The nested Orientation is
    // flattened into this container and picks up its byte order.
    static void testBigEndianRecord() throws Commands.DefgenError {
        Commands.LegacyReading r = new Commands.LegacyReading(
                (short) 0x11, 0x2233, new Commands.Orientation((byte) 1, (byte) -2, (byte) 3), 0x4455);
        byte[] buf = r.encode();
        checkBytes(
                buf,
                new byte[] {0x11, 0x22, 0x33, 0x01, (byte) 0xfe, 0x03, 0x44, 0x55},
                "LegacyReading keeps its fields in declaration order");
        check(Commands.LegacyReading.decode(buf).equals(r), "LegacyReading round-trips");
    }

    // A fixed array and a variable-length tail in one big-endian container:
    // both keep their elements in declaration order, with byte order applying
    // inside an element and never across elements (§8).
    static void testBigEndianSequences() throws Commands.DefgenError {
        List<Short> key = List.of((short) 0xde, (short) 0xad, (short) 0xbe, (short) 0xef);
        // raw 100 = 0x0064, raw -200 = 0xff38
        Commands.LegacyLog log = new Commands.LegacyLog(key, List.of(1.0f, -2.0f));
        byte[] buf = log.encode();
        checkBytes(
                buf,
                new byte[] {(byte) 0xde, (byte) 0xad, (byte) 0xbe, (byte) 0xef, 0x00, 0x64, (byte) 0xff, 0x38},
                "LegacyLog keeps its array and tail elements in order");
        Commands.LegacyLog back = Commands.LegacyLog.decode(buf);
        check(back.key().equals(key), "LegacyLog's fixed array round-trips");
        check(Commands.temperatureToRaw(back.samples().get(0)) == 100, "LegacyLog sample 0");
        check(Commands.temperatureToRaw(back.samples().get(1)) == -200, "LegacyLog sample 1");

        // An empty tail is just the prefix (§6.3).
        byte[] empty = new Commands.LegacyLog(key, List.of()).encode();
        checkBytes(
                empty,
                new byte[] {(byte) 0xde, (byte) 0xad, (byte) 0xbe, (byte) 0xef},
                "LegacyLog with no samples is its prefix alone");
    }

    // -----------------------------------------------------------------------
    // Arrays (§6.1) and scaled types (§4)
    // -----------------------------------------------------------------------

    static void testTemperatureLog() throws Commands.DefgenError {
        Commands.TemperatureLog log = new Commands.TemperatureLog(List.of(
                21.5f, // raw  2150 = 0x0866
                -0.01f, // raw    -1 = 0xffff
                0.0f,
                327.67f)); // raw 32767, the i16 maximum

        byte[] buf = log.encode();
        checkBytes(
                buf,
                new byte[] {0x66, 0x08, b(0xFF), b(0xFF), 0x00, 0x00, b(0xFF), 0x7F},
                "TemperatureLog bytes");

        Commands.TemperatureLog back = Commands.TemperatureLog.decode(buf);
        check(21.49f < back.samples().get(0) && back.samples().get(0) < 21.51f, "samples[0] round-trips");
        check(
                Commands.temperatureToRaw(back.samples().get(1)) == -1,
                "samples[1] is sign-extended exactly once, i.e. raw -1 and not -65537");
        check(back.samples().get(3) > 327.66f, "samples[3] round-trips");

        // The raw integer stays reachable, so a round trip need not go through
        // floating point at all (§4).
        check(Commands.temperatureToRaw(21.5f) == 2150, "temperatureToRaw");
        check(Commands.temperatureFromRaw(s(2150)) > 21.49f, "temperatureFromRaw");

        // Rounding decides the raw unit: truncation would send both of these to 2151.
        check(Commands.temperatureToRaw(21.514f) == 2151, "rounds down below the half");
        check(Commands.temperatureToRaw(21.516f) == 2152, "and up above it");
        check(Commands.temperatureToRaw(-21.514f) == -2151, "symmetrically below zero");
        check(Commands.temperatureToRaw(-21.516f) == -2152, "on both sides");
        // The exact tie: half away from zero, never half to even.
        check(Commands.temperatureToRaw(0.005f) == 1, "a tie rounds away from zero");
        check(Commands.temperatureToRaw(-0.005f) == -1, "on both sides of it");

        // Out of the raw type's range is an error, not a wrap.
        checkThrows(Commands.DefgenRangeError.class, "raw overflow", () -> Commands.temperatureToRaw(1000.0f));
        checkThrows(Commands.DefgenRangeError.class, "raw underflow", () -> Commands.temperatureToRaw(-1000.0f));
        checkThrows(Commands.DefgenRangeError.class, "NaN is rejected", () -> Commands.temperatureToRaw(Float.NaN));
        checkThrows(
                Commands.DefgenRangeError.class,
                "inf is rejected",
                () -> Commands.temperatureToRaw(Float.POSITIVE_INFINITY));
        checkThrows(
                Commands.DefgenRangeError.class,
                "and -inf",
                () -> Commands.temperatureToRaw(Float.NEGATIVE_INFINITY));

        // A fixed array carries exactly its declared count, always (§6.1).
        checkThrows(
                Commands.DefgenRangeError.class,
                "a short array fails to encode",
                () -> new Commands.TemperatureLog(List.of(0.0f, 0.0f)).encode());
        check(
                new Commands.TemperatureLog().samples().equals(List.of(0.0f, 0.0f, 0.0f, 0.0f)),
                "the default array is full-length");
        // Sharing one list between zero values is safe precisely because it
        // cannot be written to.
        checkThrows(
                UnsupportedOperationException.class,
                "a decoded array is immutable",
                () -> Commands.TemperatureLog.decode(buf).samples().set(0, 1.0f));
    }

    static void testPadding() throws Commands.DefgenError {
        // `padding: uN = 0` is validated on decode; bare padding is not (§6.2).
        // MotionPath is never bound to a characteristic, so it has no decode of
        // its own (§8) — the plumbing under one is what there is to test.
        Commands.MotionPath.unpackFixed(Commands.DefgenBits.fromBytes(new byte[8], false), 0);

        byte[] buf = new byte[8];
        buf[7] = 0x01; // inside the `padding: u16 = 0` run at bits 48..64
        checkThrows(
                Commands.DefgenPaddingError.class,
                "non-zero `padding = 0` is rejected",
                () -> Commands.MotionPath.unpackFixed(Commands.DefgenBits.fromBytes(buf, false), 0));

        // Status's bare padding at bits 45..60 is ignored rather than validated.
        byte[] noisy = status(1).encode();
        noisy[6] = b(0xFF);
        Commands.Status.decode(noisy);
    }

    // -----------------------------------------------------------------------
    // Tagged unions (§7)
    // -----------------------------------------------------------------------

    static void testCommand() throws Commands.DefgenError {
        check(Commands.Command.SIZE == 8, "Command.SIZE == 8");
        check(Commands.Command.SetVolume.ID == 0x0001, "SetVolume's wire id");
        check(Commands.Command.TriggerFactoryReset.ID == 0xFFFF, "TriggerFactoryReset's wire id");

        // A known variant: 16-bit tag in the low bits, payload above it.
        byte[] buf = new Commands.Command.SetVolume(b(7)).encode();
        checkBytes(buf, new byte[] {0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00}, "SetVolume bytes");

        Commands.Command back = Commands.Command.decode(buf);
        check(back instanceof Commands.Command.SetVolume, "decodes to the SetVolume record");
        check(((Commands.Command.SetVolume) back).volume() == 7, "SetVolume.volume round-trips");

        // A variant with no payload leaves the whole payload region zero.
        checkBytes(
                new Commands.Command.TriggerFactoryReset().encode(),
                new byte[] {b(0xFF), b(0xFF), 0x00, 0x00, 0x00, 0x00, 0x00, 0x00},
                "TriggerFactoryReset bytes");

        // A nested struct inside a variant is flattened into the payload region.
        buf = new Commands.Command.SetOrientationOffset(new Commands.Orientation(b(1), b(2), b(3))).encode();
        checkBytes(buf, new byte[] {0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00}, "SetOrientationOffset bytes");
        back = Commands.Command.decode(buf);
        check(back instanceof Commands.Command.SetOrientationOffset, "decodes to SetOrientationOffset");
        check(
                ((Commands.Command.SetOrientationOffset) back).offset().z() == 3,
                "the nested struct round-trips");

        // A variant's own encode is the one it inherits, and dispatches on itself.
        checkBytes(
                new Commands.Command.SetMute(true).encode(),
                new byte[] {0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00},
                "SetMute bytes");
    }

    static void testCommandUnknown() throws Commands.DefgenError {
        // An unrecognized id decodes to the `else` variant, keeping both id and
        // the undecoded payload, and never silently becomes a known variant (§7).
        byte[] buf = new byte[8];
        buf[0] = 0x34;
        buf[1] = 0x12; // id = 0x1234, undeclared
        buf[2] = b(0xAB);

        Commands.Command back = Commands.Command.decode(buf);
        check(back instanceof Commands.Command.Unknown, "an unknown id decodes to the else variant");
        Commands.Command.Unknown unknown = (Commands.Command.Unknown) back;
        check(unknown.id() == 0x1234, "the unknown id is kept");
        check(unknown.raw() == 0xABL, "and so is the undecoded payload");

        // and re-encodes to exactly the bytes it came from.
        checkBytes(back.encode(), buf, "an unknown command re-encodes verbatim");

        // A closed union would throw here instead; this one cannot.
        check(Commands.Command.decode(new byte[8]) instanceof Commands.Command.Unknown, "id 0 is undeclared too");
    }

    // -----------------------------------------------------------------------
    // Variable-length values (§6.3)
    // -----------------------------------------------------------------------

    static void testDiagnosticLabel() throws Commands.DefgenError {
        check(Commands.DiagnosticLabel.FIXED_SIZE == 1, "DiagnosticLabel.FIXED_SIZE");
        check(Commands.DiagnosticLabel.MAX_SIZE == 25, "DiagnosticLabel.MAX_SIZE");

        Commands.DiagnosticLabel value = new Commands.DiagnosticLabel(s(3), "hi");

        // The encoding is exactly prefix + actual tail — never padded to max.
        check(value.encodedSize() == 3, "encodedSize is prefix + tail");
        byte[] buf = value.encode();
        check(buf.length == 3, "and so is the encoding itself");
        checkBytes(buf, new byte[] {0x03, 'h', 'i'}, "DiagnosticLabel bytes");

        Commands.DiagnosticLabel back = Commands.DiagnosticLabel.decode(buf);
        check(back.severity() == 3, "severity round-trips");
        check(back.label().equals("hi"), "the tail round-trips");

        // An empty tail is legal: the value is just its fixed prefix.
        byte[] empty = new Commands.DiagnosticLabel(s(3), "").encode();
        checkBytes(empty, new byte[] {0x03}, "an empty tail is just the prefix");
        check(Commands.DiagnosticLabel.decode(empty).label().isEmpty(), "and decodes back to empty");

        // Over `max` fails on encode, and a too-long buffer fails on decode.
        checkThrows(
                Commands.DefgenRangeError.class,
                "a tail over `max` fails to encode",
                () -> new Commands.DiagnosticLabel(s(0), "x".repeat(25)).encode());
        checkThrows(
                Commands.DefgenLengthError.class,
                "a buffer over MAX_SIZE fails to decode",
                () -> Commands.DiagnosticLabel.decode(new byte[26]));
        checkThrows(
                Commands.DefgenLengthError.class,
                "an empty buffer too",
                () -> Commands.DiagnosticLabel.decode(new byte[0]));

        // `max` bounds bytes, not characters: 12 two-byte characters fit, 13 do not.
        check(new Commands.DiagnosticLabel(s(0), "é".repeat(12)).encode().length == 25, "12 x é fits");
        checkThrows(
                Commands.DefgenRangeError.class,
                "13 x é does not",
                () -> new Commands.DiagnosticLabel(s(0), "é".repeat(13)).encode());
    }

    static void testOwnerName() throws Commands.DefgenError {
        // An alias of a variable-length type binds straight to a characteristic.
        check(Commands.OWNER_NAME_FIXED_SIZE == 0, "OWNER_NAME_FIXED_SIZE");
        check(Commands.OWNER_NAME_MAX_SIZE == 32, "OWNER_NAME_MAX_SIZE");

        byte[] buf = Commands.encodeOwnerName("Ada");
        checkBytes(buf, new byte[] {'A', 'd', 'a'}, "OwnerName bytes");
        check(Commands.decodeOwnerName(buf).equals("Ada"), "OwnerName round-trips");

        // Multi-byte UTF-8 survives, and the byte count is what `max` bounds.
        byte[] accented = Commands.encodeOwnerName("é");
        check(accented.length == 2, "é is two bytes on the wire");
        check(Commands.decodeOwnerName(accented).equals("é"), "and round-trips");
        checkThrows(
                Commands.DefgenRangeError.class,
                "17 x é exceeds max: 32",
                () -> Commands.encodeOwnerName("é".repeat(17)));

        // Malformed UTF-8 fails rather than being replaced (§6.3).
        checkThrows(
                Commands.DefgenUtf8Error.class,
                "rejects a byte no encoding starts with",
                () -> Commands.decodeOwnerName(new byte[] {b(0xFF)}));
        checkThrows(
                Commands.DefgenUtf8Error.class,
                "rejects a lead byte with no continuation",
                () -> Commands.decodeOwnerName(new byte[] {b(0xC3)}));
        checkThrows(
                Commands.DefgenUtf8Error.class,
                "rejects an overlong encoding of U+0000",
                () -> Commands.decodeOwnerName(new byte[] {b(0xC0), b(0x80)}));
        checkThrows(
                Commands.DefgenUtf8Error.class,
                "rejects a surrogate, U+D800",
                () -> Commands.decodeOwnerName(new byte[] {b(0xED), b(0xA0), b(0x80)}));

        // Over `max` is a decode error too.
        checkThrows(
                Commands.DefgenLengthError.class,
                "a 33-byte buffer",
                () -> Commands.decodeOwnerName(new byte[33]));
    }

    // -----------------------------------------------------------------------

    static void testMetadata() {
        check(
                Commands.HEARING_AID_CONTROL_UUID.equals("7d8f0000-3c1a-4e8a-9b5a-000000000000"),
                "service UUID");
        check(
                Commands.HEARING_AID_CONTROL_STATUS_CHAR_UUID.equals("7d8f0001-3c1a-4e8a-9b5a-000000000000"),
                "characteristic UUID");

        check(
                Commands.SERVICES.equals(List.of(Commands.HEARING_AID_CONTROL)),
                "SERVICES lists every service");
        Commands.GattService service = Commands.SERVICES.get(0);
        check(service.name().equals("HearingAidControl"), "the service keeps its schema name");
        check(service.characteristics().size() == 8, "with all eight characteristics, in source order");

        Commands.GattCharacteristic statusChar = service.characteristics().get(0);
        check(statusChar.name().equals("StatusChar"), "characteristics are in source order");
        check(
                statusChar.properties().equals(Set.of(Commands.GattProperty.READ, Commands.GattProperty.NOTIFY)),
                "properties are a flag set");
        check(
                service.characteristics().get(1).properties().equals(
                        Set.of(Commands.GattProperty.WRITE, Commands.GattProperty.WRITE_WITHOUT_RESPONSE)),
                "write properties");
        check(statusChar.properties().contains(Commands.GattProperty.READ), "and support membership tests");
    }

    static void testConstants() {
        check(Commands.MAX_WRITE_LENGTH == 32, "MAX_WRITE_LENGTH");
        check(Commands.MIN_RATED_TEMPERATURE == -40, "MIN_RATED_TEMPERATURE");
    }

    public static void main(String[] args) throws Commands.DefgenError {
        testStatus();
        testOpenEnum();
        testEndianness();
        testBigEndianRecord();
        testBigEndianSequences();
        testTemperatureLog();
        testPadding();
        testCommand();
        testCommandUnknown();
        testDiagnosticLabel();
        testOwnerName();
        testMetadata();
        testConstants();

        if (failures == 0) {
            System.out.println("ok");
        }
        System.exit(failures);
    }
}
