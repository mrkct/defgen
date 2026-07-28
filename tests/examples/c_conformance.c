/*
 * Conformance fixture for the C backend.
 *
 * The Rust test generates a header from `commands.defs`, compiles this file
 * against it, and runs it. Everything here is a claim about the *wire format*:
 * a hand-written byte string on one side, the decoded value on the other. Bit
 * offsets are worked out from SPEC.md §6 by hand rather than taken from the
 * generated header, so a bug in the emitter's layout arithmetic cannot agree
 * with itself into a passing test.
 *
 * Exit status is the number of failures.
 */

#include <stdio.h>
#include <string.h>

#include "commands.h"

static int failures = 0;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (!(cond)) {                                                         \
            printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);             \
            failures++;                                                        \
        }                                                                      \
    } while (0)

#define CHECK_BYTES(buf, len, ...)                                             \
    do {                                                                       \
        const uint8_t want[] = {__VA_ARGS__};                                  \
        check_bytes(__LINE__, (buf), (len), want, sizeof(want));               \
    } while (0)

static void check_bytes(int line, const uint8_t *got, size_t got_len,
                        const uint8_t *want, size_t want_len) {
    size_t i;
    if (got_len == want_len && memcmp(got, want, want_len) == 0) return;
    printf("FAIL %s:%d: bytes differ\n  want:", __FILE__, line);
    for (i = 0; i < want_len; i++) printf(" %02x", want[i]);
    printf("\n  got: ");
    for (i = 0; i < got_len; i++) printf(" %02x", got[i]);
    printf("\n");
    failures++;
}

/* ------------------------------------------------------------------------ */
/* Struct: bit packing, nesting, scaled fields, reserved bits (§6)           */
/* ------------------------------------------------------------------------ */

/*
 * Status is 64 little-endian bits, packed LSB-first in declaration order:
 *
 *   [0..4)   active_profile u4
 *   [4..8)   volume         u4  (alias of u4)
 *   [8..12)  mode           u4  (open enum)
 *   [12..13) muted          bool
 *   [13..21) battery        u8  (scaled, 0.02)
 *   [21..45) orientation    3 x i8
 *   [45..60) padding        u15
 *   [60..64) reserved flags u4
 */
static void test_status(void) {
    Status s, back;
    uint8_t buf[STATUS_SIZE];
    size_t len = 0;

    CHECK(STATUS_SIZE == 8u);

    memset(&s, 0, sizeof(s));
    s.active_profile = 0x3;
    s.volume = 0xa;
    s.mode = HEARING_MODE_CINEMA; /* 3 */
    s.muted = true;
    s.battery = 2.0f; /* raw 100 = 0x64 */
    s.orientation.x = -1;
    s.orientation.y = 2;
    s.orientation.z = -128;
    s.flags = 0x5;

    CHECK(status_encode(&s, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK(len == 8u);

    /*
     * With battery raw = 0x64 and orientation = (0xff, 0x02, 0x80), the fields
     * straddle byte boundaries — which is the point of the exercise:
     *
     *   byte 0  bits  0..8   profile 3 | volume 0xa << 4              -> 0xa3
     *   byte 1  bits  8..16  mode 3 | muted << 4 | battery[0..3] << 5 -> 0x93
     *   byte 2  bits 16..24  battery[3..8] | x[0..3] << 5             -> 0xec
     *   byte 3  bits 24..32  x[3..8] | y[0..3] << 5                   -> 0x5f
     *   byte 4  bits 32..40  y[3..8] | z[0..3] << 5                   -> 0x00
     *   byte 5  bits 40..48  z[3..8] | padding                        -> 0x10
     *   byte 6  bits 48..56  padding                                  -> 0x00
     *   byte 7  bits 56..64  padding | flags 5 << 4                   -> 0x50
     */
    CHECK_BYTES(buf, len, 0xa3, 0x93, 0xec, 0x5f, 0x00, 0x10, 0x00, 0x50);

    CHECK(status_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.active_profile == 0x3);
    CHECK(back.volume == 0xa);
    CHECK(back.mode == HEARING_MODE_CINEMA);
    CHECK(back.muted == true);
    CHECK(back.battery > 1.99f && back.battery < 2.01f);
    CHECK(back.orientation.x == -1); /* sign-extended, not 255 */
    CHECK(back.orientation.y == 2);
    CHECK(back.orientation.z == -128);
    CHECK(back.flags == 0x5); /* reserved bits round-trip (§6.2) */

    /* A value too wide for its field is a hard error, never a truncation. */
    s.active_profile = 16;
    CHECK(status_encode(&s, buf, sizeof(buf), &len) == DEFGEN_ERR_RANGE);
    s.active_profile = 3;

    /* Wrong lengths are rejected on both sides. */
    CHECK(status_encode(&s, buf, 7, &len) == DEFGEN_ERR_BUFFER_TOO_SMALL);
    CHECK(status_decode(&back, buf, 7) == DEFGEN_ERR_LENGTH);
    CHECK(status_decode(&back, buf, 9) == DEFGEN_ERR_LENGTH);
}

/* An open enum keeps an unrecognized wire value rather than failing (§5). */
static void test_open_enum(void) {
    Status s;
    uint8_t buf[STATUS_SIZE];

    CHECK(hearing_mode_is_known(HEARING_MODE_MONO));
    CHECK(!hearing_mode_is_known((HearingMode)9));
    CHECK(strcmp(hearing_mode_name(HEARING_MODE_STEREO), "Stereo") == 0);
    CHECK(hearing_mode_name((HearingMode)9) == NULL);

    memset(buf, 0, sizeof(buf));
    buf[1] = 0x09; /* mode = 9, undeclared */
    CHECK(status_decode(&s, buf, sizeof(buf)) == DEFGEN_OK);
    CHECK(s.mode == (HearingMode)9);
    CHECK(!hearing_mode_is_known(s.mode));
}

/* ------------------------------------------------------------------------ */
/* Byte order (§8)                                                          */
/* ------------------------------------------------------------------------ */

static void test_endianness(void) {
    LegacySerial s, back;
    Status status;
    uint8_t buf[LEGACY_SERIAL_SIZE];
    uint8_t sbuf[STATUS_SIZE];
    size_t len = 0;

    /* LegacySerial carries #[endian(big)]: its one value reads MSB-first. */
    s.serial = 0x01020304u;
    CHECK(legacy_serial_encode(&s, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x01, 0x02, 0x03, 0x04);
    CHECK(legacy_serial_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.serial == 0x01020304u);

    /* while the file default is little-endian. */
    memset(&status, 0, sizeof(status));
    status.active_profile = 1;
    CHECK(status_encode(&status, sbuf, sizeof(sbuf), &len) == DEFGEN_OK);
    CHECK(sbuf[0] == 0x01);
    CHECK(sbuf[7] == 0x00);
}

/* A big-endian container with more than one field: they stay in declaration
   order, first field in the first byte, and only the direction each
   multi-byte value reads in changes (§8). The nested Orientation is
   flattened into this container and picks up its byte order. */
static void test_big_endian_record(void) {
    LegacyReading r, back;
    uint8_t buf[LEGACY_READING_SIZE];
    size_t len = 0;

    r.id = 0x11;
    r.value = 0x2233;
    r.orientation.x = 1;
    r.orientation.y = -2;
    r.orientation.z = 3;
    r.crc = 0x4455;
    CHECK(legacy_reading_encode(&r, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x11, 0x22, 0x33, 0x01, 0xfe, 0x03, 0x44, 0x55);
    CHECK(legacy_reading_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.id == 0x11 && back.value == 0x2233 && back.crc == 0x4455);
    CHECK(back.orientation.x == 1 && back.orientation.y == -2 && back.orientation.z == 3);
}

/* A fixed array and a variable-length tail in one big-endian container: both
   keep their elements in declaration order, with byte order applying inside
   an element and never across elements (§8). */
static void test_big_endian_sequences(void) {
    LegacyLog log, back;
    uint8_t buf[LEGACY_LOG_MAX_SIZE];
    size_t len = 0;
    TemperatureRaw raw = 0;

    log.key[0] = 0xde;
    log.key[1] = 0xad;
    log.key[2] = 0xbe;
    log.key[3] = 0xef;
    log.samples[0] = 1.0f;  /* raw 100 = 0x0064 */
    log.samples[1] = -2.0f; /* raw -200 = 0xff38 */
    log.samples_len = 2;
    CHECK(legacy_log_encode(&log, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x64, 0xff, 0x38);
    CHECK(legacy_log_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.key[0] == 0xde && back.key[3] == 0xef);
    CHECK(back.samples_len == 2);
    CHECK(temperature_to_raw(back.samples[0], &raw) == DEFGEN_OK && raw == 100);
    CHECK(temperature_to_raw(back.samples[1], &raw) == DEFGEN_OK && raw == -200);

    /* An empty tail is just the prefix (§6.3). */
    log.samples_len = 0;
    CHECK(legacy_log_encode(&log, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0xde, 0xad, 0xbe, 0xef);
}

/* ------------------------------------------------------------------------ */
/* Arrays (§6.1) and scaled types (§4)                                      */
/* ------------------------------------------------------------------------ */

static void test_temperature_log(void) {
    TemperatureLog log, back;
    uint8_t buf[TEMPERATURE_LOG_SIZE];
    size_t len = 0;
    TemperatureRaw raw = 0;

    log.samples[0] = 21.5f;  /* raw 2150 = 0x0866 */
    log.samples[1] = -0.01f; /* raw   -1 = 0xffff */
    log.samples[2] = 0.0f;
    log.samples[3] = 327.67f; /* raw 32767, the i16 maximum */

    CHECK(temperature_log_encode(&log, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x66, 0x08, 0xff, 0xff, 0x00, 0x00, 0xff, 0x7f);

    CHECK(temperature_log_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.samples[0] > 21.49f && back.samples[0] < 21.51f);
    CHECK(temperature_to_raw(back.samples[1], &raw) == DEFGEN_OK && raw == -1);
    CHECK(back.samples[3] > 327.66f);

    /* The raw integer stays reachable, so a round trip need not go through
       floating point at all (§4). */
    CHECK(temperature_to_raw(21.5f, &raw) == DEFGEN_OK);
    CHECK(raw == 2150);
    CHECK(temperature_from_raw(2150) > 21.49f);

    /* Out of the raw type's range is an error, not a wrap. */
    CHECK(temperature_to_raw(1000.0f, &raw) == DEFGEN_ERR_RANGE);
    CHECK(temperature_to_raw(-1000.0f, &raw) == DEFGEN_ERR_RANGE);
}

/*
 * Rounding (§4, §14). The generated header carries its own `round()` so that
 * nothing has to link libm, and it has to be exactly `round()`: half away from
 * zero, with no bias added before the integer part is taken.
 *
 * `python_conformance.py` asserts this same table against the Python backend's
 * `_round`. If the two ever disagree, one backend is encoding scaled values a
 * unit away from the other.
 */
static void test_rounding(void) {
    struct { double in; double want; } cases[] = {
        {0.0, 0.0},   {0.4, 0.0},   {0.5, 1.0},   {0.6, 1.0},
        {1.5, 2.0},   {2.5, 3.0}, /* half away from zero, not half to even */
        {-0.4, 0.0},  {-0.5, -1.0}, {-1.5, -2.0}, {-2.5, -3.0},
        {1.0, 1.0},   {-1.0, -1.0},
        /* The double just below 0.5. `(int64_t)(v + 0.5)` gets this wrong: the
           addition itself rounds up to 1.0. */
        {0.49999999999999994, 0.0},
        {-0.49999999999999994, 0.0},
        /* At 2^52 and above every double is already an integer. */
        {4503599627370495.5, 4503599627370496.0},
        {4503599627370496.0, 4503599627370496.0},
        /* Far past any integer type: must not be cast, must not trap. */
        {1e308, 1e308},
        {-1e308, -1e308},
    };
    size_t i;
    for (i = 0; i < sizeof(cases) / sizeof(*cases); i++) {
        double got = defgen__round(cases[i].in);
        if (got != cases[i].want) {
            printf("FAIL %s:%d: round(%.20g) = %.20g, want %.20g\n", __FILE__,
                   __LINE__, cases[i].in, got, cases[i].want);
            failures++;
        }
    }

    /*
     * Reached through the public API, rounding is what decides the raw unit:
     * truncation would send both of these to 2151.
     *
     * An exact tie is deliberately not tested here. Temperature's physical
     * type is `f32` and its scale is 0.01, and neither 0.005 nor 0.01 is
     * representable in binary floating point, so no input actually lands on
     * one — the tie cases above, against the helper itself, are where that
     * behaviour is pinned.
     */
    {
        TemperatureRaw raw = 0;
        CHECK(temperature_to_raw(21.514f, &raw) == DEFGEN_OK && raw == 2151);
        CHECK(temperature_to_raw(21.516f, &raw) == DEFGEN_OK && raw == 2152);
        CHECK(temperature_to_raw(-21.514f, &raw) == DEFGEN_OK && raw == -2151);
        CHECK(temperature_to_raw(-21.516f, &raw) == DEFGEN_OK && raw == -2152);
    }
}

/* `padding: uN = 0` is validated on decode; bare padding is not (§6.2). */
static void test_padding(void) {
    MotionPath path;
    uint8_t buf[MOTION_PATH_SIZE];

    memset(buf, 0, sizeof(buf));
    CHECK(motion_path__unpack_fixed(&path, buf, MOTION_PATH_SIZE, 0, 0u) == DEFGEN_OK);

    buf[7] = 0x01; /* inside the `padding: u16 = 0` run at bits 48..64 */
    CHECK(motion_path__unpack_fixed(&path, buf, MOTION_PATH_SIZE, 0, 0u)
          == DEFGEN_ERR_PADDING);
}

/* ------------------------------------------------------------------------ */
/* Tagged unions (§7)                                                       */
/* ------------------------------------------------------------------------ */

static void test_command(void) {
    Command c, back;
    uint8_t buf[COMMAND_SIZE];
    size_t len = 0;

    CHECK(COMMAND_SIZE == 8u);
    CHECK(COMMAND_SET_VOLUME == 0x0001);
    CHECK(COMMAND_TRIGGER_FACTORY_RESET == 0xffff);

    /* A known variant: 16-bit tag in the low bits, payload above it. */
    memset(&c, 0, sizeof(c));
    c.id = COMMAND_SET_VOLUME;
    c.payload.set_volume.volume = 7;
    CHECK(command_encode(&c, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00);

    CHECK(command_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.id == COMMAND_SET_VOLUME);
    CHECK(back.payload.set_volume.volume == 7);
    CHECK(command_is_known(back.id));
    CHECK(strcmp(command_name(back.id), "SetVolume") == 0);

    /* A variant with no payload leaves the whole payload region zero. */
    memset(&c, 0, sizeof(c));
    c.id = COMMAND_TRIGGER_FACTORY_RESET;
    CHECK(command_encode(&c, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00);

    /* A nested struct inside a variant is flattened into the payload region. */
    memset(&c, 0, sizeof(c));
    c.id = COMMAND_SET_ORIENTATION_OFFSET;
    c.payload.set_orientation_offset.offset.x = 1;
    c.payload.set_orientation_offset.offset.y = 2;
    c.payload.set_orientation_offset.offset.z = 3;
    CHECK(command_encode(&c, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x04, 0x00, 0x01, 0x02, 0x03, 0x00, 0x00, 0x00);
    CHECK(command_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.payload.set_orientation_offset.offset.z == 3);
}

/* An unrecognized id decodes to the `else` variant, keeping both id and the
   undecoded payload, and never silently becomes a known variant (§7). */
static void test_command_unknown(void) {
    Command c, back;
    uint8_t buf[COMMAND_SIZE];
    size_t len = 0;

    memset(buf, 0, sizeof(buf));
    buf[0] = 0x34;
    buf[1] = 0x12; /* id = 0x1234, undeclared */
    buf[2] = 0xab;

    CHECK(command_decode(&back, buf, sizeof(buf)) == DEFGEN_OK);
    CHECK(back.id == 0x1234);
    CHECK(!command_is_known(back.id));
    CHECK(command_name(back.id) == NULL);
    CHECK(back.payload.unknown.raw == 0xabu);

    /* and re-encodes to exactly the bytes it came from. */
    memset(&c, 0, sizeof(c));
    c.id = back.id;
    c.payload.unknown.raw = back.payload.unknown.raw;
    CHECK(command_encode(&c, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK_BYTES(buf, len, 0x34, 0x12, 0xab, 0x00, 0x00, 0x00, 0x00, 0x00);
}

/* ------------------------------------------------------------------------ */
/* Variable-length values (§6.3)                                            */
/* ------------------------------------------------------------------------ */

static void test_diagnostic_label(void) {
    DiagnosticLabel d, back;
    uint8_t buf[DIAGNOSTIC_LABEL_MAX_SIZE];
    size_t len = 0;

    CHECK(DIAGNOSTIC_LABEL_FIXED_SIZE == 1u);
    CHECK(DIAGNOSTIC_LABEL_MAX_SIZE == 25u);

    memset(&d, 0, sizeof(d));
    d.severity = 3;
    memcpy(d.label, "hi", 2);
    d.label_len = 2;

    /* The encoding is exactly prefix + actual tail — never padded to max. */
    CHECK(diagnostic_label_size(&d) == 3u);
    CHECK(diagnostic_label_encode(&d, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK(len == 3u);
    CHECK_BYTES(buf, len, 0x03, 'h', 'i');

    CHECK(diagnostic_label_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.severity == 3);
    CHECK(back.label_len == 2);
    CHECK(memcmp(back.label, "hi", 2) == 0);

    /* An empty tail is legal: the value is just its fixed prefix. */
    d.label_len = 0;
    CHECK(diagnostic_label_encode(&d, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK(len == 1u);
    CHECK(diagnostic_label_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.label_len == 0);

    /* Over `max` fails on encode, and a too-long buffer fails on decode. */
    d.label_len = 25;
    CHECK(diagnostic_label_encode(&d, buf, sizeof(buf), &len) == DEFGEN_ERR_RANGE);
    CHECK(diagnostic_label_decode(&back, buf, 26) == DEFGEN_ERR_LENGTH);
    CHECK(diagnostic_label_decode(&back, buf, 0) == DEFGEN_ERR_LENGTH);
}

/* An alias of a variable-length type binds straight to a characteristic. */
static void test_owner_name(void) {
    OwnerName n, back;
    uint8_t buf[OWNER_NAME_MAX_SIZE];
    size_t len = 0;

    CHECK(OWNER_NAME_FIXED_SIZE == 0u);
    CHECK(OWNER_NAME_MAX_SIZE == 32u);

    memset(&n, 0, sizeof(n));
    memcpy(n.data, "Ada", 3);
    n.len = 3;

    CHECK(owner_name_encode(&n, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK(len == 3u);
    CHECK_BYTES(buf, len, 'A', 'd', 'a');

    CHECK(owner_name_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.len == 3);
    CHECK(memcmp(back.data, "Ada", 3) == 0);

    /* Multi-byte UTF-8 survives, and the byte count is what `max` bounds. */
    memcpy(n.data, "\xc3\xa9", 2);
    n.len = 2;
    CHECK(owner_name_encode(&n, buf, sizeof(buf), &len) == DEFGEN_OK);
    CHECK(len == 2u);
    CHECK(owner_name_decode(&back, buf, len) == DEFGEN_OK);
    CHECK(back.len == 2);

    /* Malformed UTF-8 fails rather than being replaced (§6.3). */
    buf[0] = 0xff;
    CHECK(owner_name_decode(&back, buf, 1) == DEFGEN_ERR_UTF8);
    buf[0] = 0xc3; /* a lead byte with no continuation */
    CHECK(owner_name_decode(&back, buf, 1) == DEFGEN_ERR_UTF8);
    buf[0] = 0xc0;
    buf[1] = 0x80; /* overlong encoding of U+0000 */
    CHECK(owner_name_decode(&back, buf, 2) == DEFGEN_ERR_UTF8);
    buf[0] = 0xed;
    buf[1] = 0xa0;
    buf[2] = 0x80; /* a surrogate, U+D800 */
    CHECK(owner_name_decode(&back, buf, 3) == DEFGEN_ERR_UTF8);

    /* Over `max` is a decode error too. */
    CHECK(owner_name_decode(&back, buf, 33) == DEFGEN_ERR_LENGTH);
}

/* ------------------------------------------------------------------------ */

static void test_metadata(void) {
    CHECK(strcmp(HEARING_AID_CONTROL_SERVICE_UUID,
                 "7d8f0000-3c1a-4e8a-9b5a-000000000000") == 0);
    CHECK(strcmp(HEARING_AID_CONTROL_STATUS_CHAR_UUID,
                 "7d8f0001-3c1a-4e8a-9b5a-000000000000") == 0);
    CHECK(HEARING_AID_CONTROL_STATUS_CHAR_PROPERTIES
          == (DEFGEN_PROP_READ | DEFGEN_PROP_NOTIFY));
    CHECK(HEARING_AID_CONTROL_COMMAND_CHAR_PROPERTIES
          == (DEFGEN_PROP_WRITE | DEFGEN_PROP_WRITE_WITHOUT_RESPONSE));
    CHECK(strcmp(defgen_err_str(DEFGEN_ERR_UTF8), "invalid UTF-8") == 0);

    /* The _UUID_BYTES macros are the same UUID, wire order (reversed). */
    static const uint8_t service_uuid[] = HEARING_AID_CONTROL_SERVICE_UUID_BYTES;
    static const uint8_t service_uuid_expected[] = {
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5a, 0x9b,
        0x8a, 0x4e, 0x1a, 0x3c, 0x00, 0x00, 0x8f, 0x7d,
    };
    CHECK(sizeof(service_uuid) == sizeof(service_uuid_expected));
    CHECK(memcmp(service_uuid, service_uuid_expected, sizeof(service_uuid)) == 0);

    static const uint8_t status_uuid[] = HEARING_AID_CONTROL_STATUS_CHAR_UUID_BYTES;
    static const uint8_t status_uuid_expected[] = {
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5a, 0x9b,
        0x8a, 0x4e, 0x1a, 0x3c, 0x01, 0x00, 0x8f, 0x7d,
    };
    CHECK(sizeof(status_uuid) == sizeof(status_uuid_expected));
    CHECK(memcmp(status_uuid, status_uuid_expected, sizeof(status_uuid)) == 0);
}

/* ------------------------------------------------------------------------ */
/* Constants: no wire form, just a shared value (§3.1)                      */
/* ------------------------------------------------------------------------ */

static void test_constants(void) {
    CHECK(MAX_WRITE_LENGTH == 32);
    CHECK(MIN_RATED_TEMPERATURE == -40);
}

int main(void) {
    test_status();
    test_open_enum();
    test_endianness();
    test_big_endian_record();
    test_big_endian_sequences();
    test_temperature_log();
    test_rounding();
    test_padding();
    test_command();
    test_command_unknown();
    test_diagnostic_label();
    test_owner_name();
    test_metadata();
    test_constants();

    if (failures == 0) printf("ok\n");
    return failures;
}
