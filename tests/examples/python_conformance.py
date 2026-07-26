"""Conformance fixture for the Python backend.

The Rust test generates a module from `commands.defs`, drops this file next
to it, and runs it. Everything here is a claim about the *wire format*: a
hand-written byte string on one side, the decoded value on the other. Bit
offsets are worked out from SPEC.md §6 by hand rather than read back out of
the generated module, so a bug in the emitter's layout arithmetic cannot
agree with itself into a passing test.

The byte strings are the very same ones `c_conformance.c` asserts — that is
the point of §14: two backends, one wire format.

Exit status is the number of failures.
"""

from __future__ import annotations

import sys
from typing import Any, Callable

import commands as m

failures = 0

_INF = float("inf")
_NAN = float("nan")


def check(cond: bool, what: str) -> None:
    global failures
    if not cond:
        print(f"FAIL: {what}")
        failures += 1


def check_bytes(got: bytes, want: bytes, what: str) -> None:
    global failures
    if got == want:
        return
    print(f"FAIL: {what}\n  want: {want.hex(' ')}\n  got:  {got.hex(' ')}")
    failures += 1


def check_raises(exc: type[BaseException], fn: Callable[[], Any], what: str) -> None:
    global failures
    try:
        fn()
    except exc:
        return
    except BaseException as other:  # noqa: BLE001 - reporting, not handling
        print(f"FAIL: {what}: raised {type(other).__name__} ({other}), want {exc.__name__}")
        failures += 1
        return
    print(f"FAIL: {what}: did not raise {exc.__name__}")
    failures += 1


# ---------------------------------------------------------------------------
# Struct: bit packing, nesting, scaled fields, reserved bits (§6)
# ---------------------------------------------------------------------------
#
# Status is 64 little-endian bits, packed LSB-first in declaration order:
#
#   [0..4)   active_profile u4
#   [4..8)   volume         u4  (alias of u4)
#   [8..12)  mode           u4  (open enum)
#   [12..13) muted          bool
#   [13..21) battery        u8  (scaled, 0.02)
#   [21..45) orientation    3 x i8
#   [45..60) padding        u15
#   [60..64) reserved flags u4


def test_status() -> None:
    check(m.Status.SIZE == 8, "Status.SIZE == 8")

    s = m.Status(
        active_profile=0x3,
        volume=0xA,
        mode=m.HearingMode.CINEMA,  # 3
        muted=True,
        battery=2.0,  # raw 100 = 0x64
        orientation=m.Orientation(x=-1, y=2, z=-128),
        flags=0x5,
    )

    # With battery raw = 0x64 and orientation = (0xff, 0x02, 0x80), the fields
    # straddle byte boundaries — which is the point of the exercise:
    #
    #   byte 0  bits  0..8   profile 3 | volume 0xa << 4              -> 0xa3
    #   byte 1  bits  8..16  mode 3 | muted << 4 | battery[0..3] << 5 -> 0x93
    #   byte 2  bits 16..24  battery[3..8] | x[0..3] << 5             -> 0xec
    #   byte 3  bits 24..32  x[3..8] | y[0..3] << 5                   -> 0x5f
    #   byte 4  bits 32..40  y[3..8] | z[0..3] << 5                   -> 0x00
    #   byte 5  bits 40..48  z[3..8] | padding                        -> 0x10
    #   byte 6  bits 48..56  padding                                  -> 0x00
    #   byte 7  bits 56..64  padding | flags 5 << 4                   -> 0x50
    buf = s.encode()
    check(len(buf) == 8, "Status encodes to 8 bytes")
    check_bytes(buf, bytes([0xA3, 0x93, 0xEC, 0x5F, 0x00, 0x10, 0x00, 0x50]), "Status bytes")

    back = m.Status.decode(buf)
    check(back.active_profile == 0x3, "Status.active_profile round-trips")
    check(back.volume == 0xA, "Status.volume round-trips")
    check(back.mode is m.HearingMode.CINEMA, "Status.mode round-trips")
    check(back.muted is True, "Status.muted round-trips")
    check(1.99 < back.battery < 2.01, "Status.battery round-trips")
    check(back.orientation.x == -1, "Orientation.x is sign-extended, not 255")
    check(back.orientation.y == 2, "Orientation.y round-trips")
    check(back.orientation.z == -128, "Orientation.z round-trips")
    check(back.flags == 0x5, "reserved bits round-trip (§6.2)")

    # A value too wide for its field is a hard error, never a truncation.
    s.active_profile = 16
    check_raises(m.DefgenRangeError, s.encode, "u4 overflow is a range error")
    s.active_profile = -1
    check_raises(m.DefgenRangeError, s.encode, "u4 underflow is a range error")
    s.active_profile = 3

    # Wrong lengths are rejected.
    check_raises(m.DefgenLengthError, lambda: m.Status.decode(buf[:7]), "short buffer")
    check_raises(m.DefgenLengthError, lambda: m.Status.decode(buf + b"\x00"), "long buffer")

    # Every error is a DefgenError, so one `except` clause catches the lot.
    check(issubclass(m.DefgenLengthError, m.DefgenError), "DefgenLengthError is a DefgenError")
    check(issubclass(m.DefgenRangeError, m.DefgenError), "DefgenRangeError is a DefgenError")


def test_open_enum() -> None:
    """An open enum keeps an unrecognized wire value rather than failing (§5)."""
    check(m.HearingMode.STEREO == 1, "HearingMode.Stereo == 1")
    check(m.HearingMode.CINEMA.name == "CINEMA", "enum members are named")

    buf = bytearray(8)
    buf[1] = 0x09  # mode = 9, undeclared
    s = m.Status.decode(bytes(buf))
    check(isinstance(s.mode, m.HearingModeUnknown), "unknown mode decodes to the else variant")
    check(not isinstance(s.mode, m.HearingMode), "and never to a declared variant")
    check(s.mode == m.HearingModeUnknown(raw=9), "keeping the wire value it came from")

    # and re-encodes to exactly the bytes it came from.
    check_bytes(s.encode(), bytes(buf), "unknown enum value re-encodes verbatim")


# ---------------------------------------------------------------------------
# Byte order (§8)
# ---------------------------------------------------------------------------


def test_endianness() -> None:
    # LegacySerial carries #[endian(big)]: the flattened bit sequence is
    # written from the far end of the buffer.
    s = m.LegacySerial(serial=0x01020304)
    buf = s.encode()
    check_bytes(buf, bytes([0x01, 0x02, 0x03, 0x04]), "LegacySerial is big-endian")
    check(m.LegacySerial.decode(buf).serial == 0x01020304, "LegacySerial round-trips")

    # while the file default is little-endian.
    buf = m.Status(active_profile=1).encode()
    check(buf[0] == 0x01, "Status is little-endian: low bits land in byte 0")
    check(buf[7] == 0x00, "Status is little-endian: byte 7 is untouched")


# ---------------------------------------------------------------------------
# Arrays (§6.1) and scaled types (§4)
# ---------------------------------------------------------------------------


def test_temperature_log() -> None:
    log = m.TemperatureLog(
        samples=[
            21.5,  # raw  2150 = 0x0866
            -0.01,  # raw    -1 = 0xffff
            0.0,
            327.67,  # raw 32767, the i16 maximum
        ]
    )

    buf = log.encode()
    check_bytes(
        buf,
        bytes([0x66, 0x08, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x7F]),
        "TemperatureLog bytes",
    )

    back = m.TemperatureLog.decode(buf)
    check(21.49 < back.samples[0] < 21.51, "samples[0] round-trips")
    check(back.samples[1] < 0.0, "samples[1] is negative, i.e. sign-extended")
    check(back.samples[3] > 327.66, "samples[3] round-trips")

    # The raw integer stays reachable, so a round trip need not go through
    # floating point at all (§4).
    check(m.temperature_to_raw(21.5) == 2150, "temperature_to_raw")
    check(m.temperature_from_raw(2150) > 21.49, "temperature_from_raw")

    # Rounding decides the raw unit: truncation would send both of these to 2151.
    check(m.temperature_to_raw(21.514) == 2151, "rounds down below the half")
    check(m.temperature_to_raw(21.516) == 2152, "and up above it")
    check(m.temperature_to_raw(-21.514) == -2151, "symmetrically below zero")
    check(m.temperature_to_raw(-21.516) == -2152, "on both sides")

    # Out of the raw type's range is an error, not a wrap.
    check_raises(m.DefgenRangeError, lambda: m.temperature_to_raw(1000.0), "raw overflow")
    check_raises(m.DefgenRangeError, lambda: m.temperature_to_raw(-1000.0), "raw underflow")
    check_raises(m.DefgenRangeError, lambda: m.temperature_to_raw(_NAN), "NaN is rejected")
    check_raises(m.DefgenRangeError, lambda: m.temperature_to_raw(_INF), "inf is rejected")
    check_raises(m.DefgenRangeError, lambda: m.temperature_to_raw(-_INF), "and -inf")

    # A fixed array carries exactly its declared count, always (§6.1).
    check_raises(
        m.DefgenRangeError,
        lambda: m.TemperatureLog(samples=[0.0, 0.0]).encode(),
        "a short array fails to encode",
    )
    check(
        m.TemperatureLog().samples == [0.0, 0.0, 0.0, 0.0],
        "the default array is full-length, and not shared between instances",
    )
    check(
        m.TemperatureLog().samples is not m.TemperatureLog().samples,
        "each instance gets its own list",
    )


def test_rounding() -> None:
    """Rounding (§4, §14).

    `c_conformance.c` asserts this same table against the C backend's
    `defgen__round`. If the two ever disagree, one backend is encoding scaled
    values a unit away from the other.

    The C side returns a `double` and the Python side an `int`, which is the
    only difference: Python has no reason to hand back a float it would
    immediately have to convert.
    """
    cases: list[tuple[float, int]] = [
        (0.0, 0), (0.4, 0), (0.5, 1), (0.6, 1),
        (1.5, 2), (2.5, 3),  # half away from zero, not half to even
        (-0.4, 0), (-0.5, -1), (-1.5, -2), (-2.5, -3),
        (1.0, 1), (-1.0, -1),
        # The double just below 0.5. Adding 0.5 before truncating gets this
        # wrong: the addition itself rounds up to 1.0.
        (0.49999999999999994, 0),
        (-0.49999999999999994, 0),
        # At 2**52 and above every float is already an integer.
        (4503599627370495.5, 4503599627370496),
        (4503599627370496.0, 4503599627370496),
        # Far past any fixed-width integer — Python's int reaches it anyway.
        (1e308, int(1e308)),
        (-1e308, -int(1e308)),
    ]
    for value, want in cases:
        got = m._round(value, "rounding")
        check(got == want, f"_round({value!r}) == {want}, got {got}")

    # Python's own round() disagrees on every tie, which is the whole reason
    # the generated module carries its own.
    check(round(0.5) == 0 and m._round(0.5, "x") == 1, "the built-in rounds half to even")
    check(round(2.5) == 2 and m._round(2.5, "x") == 3, "and would drift from the C backend")

    # Nothing an int cannot represent gets through, and it fails as a
    # DefgenError rather than as ValueError or OverflowError.
    for bad in (_NAN, _INF, -_INF):
        check_raises(m.DefgenRangeError, lambda b=bad: m._round(b, "x"), f"rejects {bad}")


def test_padding() -> None:
    """`padding: uN = 0` is validated on decode; bare padding is not (§6.2)."""
    bits = m._Bits.from_bytes(bytes(8), big=False)
    m.MotionPath._unpack_fixed(bits, 0)  # all-zero padding is fine

    buf = bytearray(8)
    buf[7] = 0x01  # inside the `padding: u16 = 0` run at bits 48..64
    check_raises(
        m.DefgenPaddingError,
        lambda: m.MotionPath._unpack_fixed(m._Bits.from_bytes(bytes(buf), big=False), 0),
        "non-zero `padding = 0` is rejected",
    )

    # Status's bare padding at bits 45..60 is ignored rather than validated.
    noisy = bytearray(m.Status(active_profile=1).encode())
    noisy[6] = 0xFF
    m.Status.decode(bytes(noisy))


# ---------------------------------------------------------------------------
# Tagged unions (§7)
# ---------------------------------------------------------------------------


def test_command() -> None:
    check(m.Command.SIZE == 8, "Command.SIZE == 8")
    check(m.CommandSetVolume.ID == 0x0001, "SetVolume's wire id")
    check(m.CommandTriggerFactoryReset.ID == 0xFFFF, "TriggerFactoryReset's wire id")

    # A known variant: 16-bit tag in the low bits, payload above it.
    buf = m.CommandSetVolume(volume=7).encode()
    check_bytes(
        buf,
        bytes([0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00]),
        "SetVolume bytes",
    )

    back = m.Command.decode(buf)
    check(isinstance(back, m.CommandSetVolume), "decodes to the SetVolume class")
    assert isinstance(back, m.CommandSetVolume)  # narrows for the reader and the checker
    check(back.volume == 7, "SetVolume.volume round-trips")

    # A variant with no payload leaves the whole payload region zero.
    check_bytes(
        m.CommandTriggerFactoryReset().encode(),
        bytes([0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
        "TriggerFactoryReset bytes",
    )

    # A nested struct inside a variant is flattened into the payload region.
    buf = m.CommandSetOrientationOffset(offset=m.Orientation(x=1, y=2, z=3)).encode()
    check_bytes(
        buf,
        bytes([0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00]),
        "SetOrientationOffset bytes",
    )
    back = m.Command.decode(buf)
    assert isinstance(back, m.CommandSetOrientationOffset)
    check(back.offset.z == 3, "the nested struct round-trips")

    # Every variant is a Command, so one annotation covers them all (§7).
    for variant in (
        m.CommandSetVolume,
        m.CommandSetMute,
        m.CommandSetMode,
        m.CommandSetOrientationOffset,
        m.CommandTriggerFactoryReset,
        m.CommandUnknown,
    ):
        check(issubclass(variant, m.Command), f"{variant.__name__} is a Command")

    # A variant's own encode is the one it inherits, and dispatches on itself.
    check_bytes(
        m.CommandSetMute(muted=True).encode(),
        bytes([0x02, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]),
        "SetMute bytes",
    )


def test_command_unknown() -> None:
    """An unrecognized id decodes to the `else` variant, keeping both id and
    the undecoded payload, and never silently becomes a known variant (§7).
    """
    buf = bytearray(8)
    buf[0] = 0x34
    buf[1] = 0x12  # id = 0x1234, undeclared
    buf[2] = 0xAB

    back = m.Command.decode(bytes(buf))
    check(isinstance(back, m.CommandUnknown), "an unknown id decodes to the else variant")
    assert isinstance(back, m.CommandUnknown)
    check(back.id == 0x1234, "the unknown id is kept")
    check(back.raw == 0xAB, "and so is the undecoded payload")

    # and re-encodes to exactly the bytes it came from.
    check_bytes(back.encode(), bytes(buf), "an unknown command re-encodes verbatim")


# ---------------------------------------------------------------------------
# Variable-length values (§6.3)
# ---------------------------------------------------------------------------


def test_diagnostic_label() -> None:
    check(m.DiagnosticLabel.FIXED_SIZE == 1, "DiagnosticLabel.FIXED_SIZE")
    check(m.DiagnosticLabel.MAX_SIZE == 25, "DiagnosticLabel.MAX_SIZE")

    d = m.DiagnosticLabel(severity=3, label="hi")

    # The encoding is exactly prefix + actual tail — never padded to max.
    check(d.encoded_size() == 3, "encoded_size is prefix + tail")
    buf = d.encode()
    check(len(buf) == 3, "and so is the encoding itself")
    check_bytes(buf, b"\x03hi", "DiagnosticLabel bytes")

    back = m.DiagnosticLabel.decode(buf)
    check(back.severity == 3, "severity round-trips")
    check(back.label == "hi", "the tail round-trips")

    # An empty tail is legal: the value is just its fixed prefix.
    empty = m.DiagnosticLabel(severity=3, label="").encode()
    check_bytes(empty, b"\x03", "an empty tail is just the prefix")
    check(m.DiagnosticLabel.decode(empty).label == "", "and decodes back to empty")

    # Over `max` fails on encode, and a too-long buffer fails on decode.
    check_raises(
        m.DefgenRangeError,
        lambda: m.DiagnosticLabel(severity=0, label="x" * 25).encode(),
        "a tail over `max` fails to encode",
    )
    check_raises(
        m.DefgenLengthError,
        lambda: m.DiagnosticLabel.decode(b"\x00" + b"x" * 25),
        "a buffer over MAX_SIZE fails to decode",
    )
    check_raises(m.DefgenLengthError, lambda: m.DiagnosticLabel.decode(b""), "an empty buffer too")

    # `max` bounds bytes, not characters: 12 two-byte characters fit, 13 do not.
    check(len(m.DiagnosticLabel(severity=0, label="é" * 12).encode()) == 25, "12 x é fits")
    check_raises(
        m.DefgenRangeError,
        lambda: m.DiagnosticLabel(severity=0, label="é" * 13).encode(),
        "13 x é does not",
    )


def test_owner_name() -> None:
    """An alias of a variable-length type binds straight to a characteristic."""
    check(m.OWNER_NAME_FIXED_SIZE == 0, "OWNER_NAME_FIXED_SIZE")
    check(m.OWNER_NAME_MAX_SIZE == 32, "OWNER_NAME_MAX_SIZE")

    buf = m.encode_owner_name("Ada")
    check_bytes(buf, b"Ada", "OwnerName bytes")
    check(m.decode_owner_name(buf) == "Ada", "OwnerName round-trips")

    # Multi-byte UTF-8 survives, and the byte count is what `max` bounds.
    buf = m.encode_owner_name("é")
    check(len(buf) == 2, "é is two bytes on the wire")
    check(m.decode_owner_name(buf) == "é", "and round-trips")
    check_raises(
        m.DefgenRangeError, lambda: m.encode_owner_name("é" * 17), "17 x é exceeds max: 32"
    )

    # Malformed UTF-8 fails rather than being replaced (§6.3).
    for bad, why in (
        (b"\xff", "a byte no encoding starts with"),
        (b"\xc3", "a lead byte with no continuation"),
        (b"\xc0\x80", "an overlong encoding of U+0000"),
        (b"\xed\xa0\x80", "a surrogate, U+D800"),
    ):
        check_raises(m.DefgenUtf8Error, lambda b=bad: m.decode_owner_name(b), f"rejects {why}")

    # Over `max` is a decode error too.
    check_raises(
        m.DefgenLengthError, lambda: m.decode_owner_name(b"x" * 33), "a 33-byte buffer"
    )


# ---------------------------------------------------------------------------


def test_metadata() -> None:
    check(m.SCHEMA_VERSION == 2, "SCHEMA_VERSION")
    check(
        m.HEARING_AID_CONTROL_UUID == "7d8f0000-3c1a-4e8a-9b5a-000000000000",
        "service UUID",
    )
    check(
        m.HEARING_AID_CONTROL_STATUS_CHAR_UUID == "7d8f0001-3c1a-4e8a-9b5a-000000000000",
        "characteristic UUID",
    )

    check(m.SERVICES == (m.HEARING_AID_CONTROL,), "SERVICES lists every service")
    service = m.SERVICES[0]
    check(service.name == "HearingAidControl", "the service keeps its schema name")
    check(len(service.characteristics) == 6, "with all six characteristics, in source order")

    status_char = service.characteristics[0]
    check(status_char.name == "StatusChar", "characteristics are in source order")
    check(
        status_char.properties == m.GattProperty.READ | m.GattProperty.NOTIFY,
        "properties are a flag set",
    )
    check(
        service.characteristics[1].properties
        == m.GattProperty.WRITE | m.GattProperty.WRITE_WITHOUT_RESPONSE,
        "write properties",
    )
    check(m.GattProperty.READ in status_char.properties, "and support `in`")


def test_type_hints() -> None:
    """Every function the emitter wrote is fully annotated, and every
    annotation resolves (§13).

    The module opens with `from __future__ import annotations`, so annotations
    are never evaluated at import and a reference to a name that does not exist
    would import perfectly happily. `get_type_hints` is what evaluates them.

    Only functions whose code object comes from the generated file count:
    `dataclass` and `enum` synthesize `__init__`, `__eq__`, `__or__` and
    friends by `exec`ing a template, and those are the standard library's
    business, not the emitter's.
    """
    import inspect
    import typing

    def emitted(fn: Any) -> bool:
        code = getattr(fn, "__code__", None)
        return code is not None and code.co_filename == m.__file__

    functions: list[tuple[str, Any]] = []
    classes: list[tuple[str, type]] = []
    for name, obj in vars(m).items():
        if isinstance(obj, type) and obj.__module__ == m.__name__:
            classes.append((name, obj))
            for attr, member in vars(obj).items():
                if isinstance(member, (classmethod, staticmethod)):
                    member = member.__func__
                if inspect.isfunction(member) and emitted(member):
                    functions.append((f"{name}.{attr}", member))
        elif inspect.isfunction(obj) and emitted(obj):
            functions.append((name, obj))

    for name, cls in classes:
        try:
            # Covers dataclass fields, whose annotations become the synthesized
            # `__init__`'s, and the `ClassVar`s alongside them.
            typing.get_type_hints(cls, vars(m))
        except Exception as exc:  # noqa: BLE001 - the failure is the finding
            check(False, f"class {name}: unresolvable annotation ({exc})")

    for name, fn in functions:
        try:
            hints = typing.get_type_hints(fn, vars(m))
        except Exception as exc:  # noqa: BLE001 - the failure is the finding
            check(False, f"{name}: unresolvable annotation ({exc})")
            continue
        check("return" in hints, f"{name} has a return annotation")
        for param in inspect.signature(fn).parameters.values():
            if param.name in ("self", "cls"):
                continue
            check(param.name in hints, f"{name}({param.name}) is annotated")

    # A sweep that found nothing would pass every assertion above.
    check(len(functions) > 30, f"the sweep reached the module ({len(functions)} functions)")
    check(len(classes) > 10, f"and its classes ({len(classes)} classes)")


def main() -> int:
    test_status()
    test_open_enum()
    test_endianness()
    test_temperature_log()
    test_rounding()
    test_padding()
    test_command()
    test_command_unknown()
    test_diagnostic_label()
    test_owner_name()
    test_metadata()
    test_type_hints()

    if failures == 0:
        print("ok")
    return failures


if __name__ == "__main__":
    sys.exit(main())
