/*
 * Conformance fixture for the Zephyr stack, run against the files
 * `defgen server commands.defs --stack zephyr` generates.
 *
 * The generated table cannot be checked by inspection: a characteristic's
 * value lands at an attribute index the generator worked out by counting
 * macros, and its ATT callbacks are `static`, reachable only through the table
 * itself. So this fixture goes in the front door — it finds each
 * characteristic through its generated `*_attr()` accessor and drives the
 * callbacks the table published there, which is exactly the path a real
 * client's read or write takes.
 *
 * The Bluetooth side is stubbed (see zephyr_stub/), so what is under test is
 * the generated code: the attribute layout, the encode/decode wiring, and the
 * mapping from a codec failure to an ATT error.
 */
#include <errno.h>
#include <stdio.h>
#include <string.h>

#include <zephyr/bluetooth/att.h>
#include <zephyr/sys/util.h>

#include "commands_gatt.h"

static int failures;

#define CHECK(cond)                                                                        \
    do {                                                                                   \
        if (!(cond)) {                                                                     \
            printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);                         \
            failures++;                                                                    \
        }                                                                                  \
    } while (0)

/* ------------------------------------------------------------------------ */
/* The Bluetooth calls the generated code makes                             */
/* ------------------------------------------------------------------------ */

static uint8_t last_notify[64];
static uint16_t last_notify_len;
static int notify_calls;

ssize_t bt_gatt_attr_read(struct bt_conn *conn, const struct bt_gatt_attr *attr, void *buf, uint16_t buf_len,
                          uint16_t offset, const void *value, uint16_t value_len)
{
    ARG_UNUSED(conn);
    ARG_UNUSED(attr);
    if (offset > value_len) {
        return BT_GATT_ERR(BT_ATT_ERR_INVALID_OFFSET);
    }
    if (value_len - offset < buf_len) {
        buf_len = (uint16_t)(value_len - offset);
    }
    memcpy(buf, (const uint8_t *)value + offset, buf_len);
    return (ssize_t)buf_len;
}

int bt_gatt_notify(struct bt_conn *conn, const struct bt_gatt_attr *attr, const void *data, uint16_t len)
{
    ARG_UNUSED(conn);
    ARG_UNUSED(attr);
    notify_calls++;
    last_notify_len = len;
    memcpy(last_notify, data, len);
    return 0;
}

int bt_gatt_indicate(struct bt_conn *conn, struct bt_gatt_indicate_params *params)
{
    ARG_UNUSED(conn);
    ARG_UNUSED(params);
    return 0;
}

/* ------------------------------------------------------------------------ */
/* The application hooks the generated header declares                      */
/* ------------------------------------------------------------------------ */

static Status the_status;
static int status_read_result;

static Command last_command;
static int command_writes;
static int command_write_result;

int hearing_aid_control_status_char_read(struct bt_conn *conn, Status *out)
{
    ARG_UNUSED(conn);
    if (status_read_result != 0) {
        return status_read_result;
    }
    *out = the_status;
    return 0;
}

int hearing_aid_control_command_char_write(struct bt_conn *conn, const Command *value)
{
    ARG_UNUSED(conn);
    last_command = *value;
    command_writes++;
    return command_write_result;
}

int hearing_aid_control_temperature_log_char_read(struct bt_conn *conn, TemperatureLog *out)
{
    ARG_UNUSED(conn);
    memset(out, 0, sizeof *out);
    return 0;
}

int hearing_aid_control_serial_char_read(struct bt_conn *conn, LegacySerial *out)
{
    ARG_UNUSED(conn);
    memset(out, 0, sizeof *out);
    return 0;
}

int hearing_aid_control_owner_name_char_read(struct bt_conn *conn, OwnerName *out)
{
    ARG_UNUSED(conn);
    memcpy(out->data, "Ada", 3);
    out->len = 3;
    return 0;
}

int hearing_aid_control_owner_name_char_write(struct bt_conn *conn, const OwnerName *value)
{
    ARG_UNUSED(conn);
    ARG_UNUSED(value);
    return 0;
}

int hearing_aid_control_diagnostic_label_char_read(struct bt_conn *conn, DiagnosticLabel *out)
{
    ARG_UNUSED(conn);
    memset(out, 0, sizeof *out);
    return 0;
}

int hearing_aid_control_diagnostic_label_char_write(struct bt_conn *conn, const DiagnosticLabel *value)
{
    ARG_UNUSED(conn);
    ARG_UNUSED(value);
    return 0;
}

/* ------------------------------------------------------------------------ */
/* Tests                                                                    */
/* ------------------------------------------------------------------------ */

/* Every accessor must land on a *value* attribute, not on a declaration or a
   CCC. This is the attribute-index arithmetic, checked end to end. */
static void the_accessors_find_value_attributes(void)
{
    const struct bt_gatt_attr *attrs[] = {
        NULL, NULL, NULL, NULL, NULL, NULL, NULL
    };
    size_t i, j;

    attrs[0] = hearing_aid_control_status_char_attr();
    attrs[1] = hearing_aid_control_command_char_attr();
    attrs[2] = hearing_aid_control_temperature_log_char_attr();
    attrs[3] = hearing_aid_control_serial_char_attr();
    attrs[4] = hearing_aid_control_owner_name_char_attr();
    attrs[5] = hearing_aid_control_diagnostic_label_char_attr();

    for (i = 0; i < 6; i++) {
        CHECK(attrs[i] != NULL);
        CHECK(strcmp(attrs[i]->kind, "value") == 0);
        CHECK(attrs[i]->uuid != NULL);
        /* Distinct characteristics, so distinct attributes. */
        for (j = 0; j < i; j++) {
            CHECK(attrs[i] != attrs[j]);
        }
    }
}

/* A characteristic's declared properties decide its callbacks: a write-only
   one has no read callback to offer, and vice versa. */
static void the_table_wires_only_the_declared_directions(void)
{
    const struct bt_gatt_attr *status = hearing_aid_control_status_char_attr();
    const struct bt_gatt_attr *command = hearing_aid_control_command_char_attr();
    const struct bt_gatt_attr *owner = hearing_aid_control_owner_name_char_attr();

    CHECK(status->read != NULL); /* [read, notify] */
    CHECK(status->write == NULL);
    CHECK(command->read == NULL); /* [write, write_without_response] */
    CHECK(command->write != NULL);
    CHECK(owner->read != NULL); /* [read, write] */
    CHECK(owner->write != NULL);

    CHECK(status->perm == BT_GATT_PERM_READ);
    CHECK(command->perm == BT_GATT_PERM_WRITE);
    CHECK(owner->perm == (BT_GATT_PERM_READ | BT_GATT_PERM_WRITE));
}

/* An ATT read runs the application hook and returns the encoded value. */
static void a_read_encodes_what_the_hook_supplied(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_status_char_attr();
    uint8_t buf[STATUS_SIZE];
    uint8_t expected[STATUS_SIZE];
    size_t expected_len;
    ssize_t n;

    memset(&the_status, 0, sizeof the_status);
    the_status.active_profile = 2;
    the_status.volume = 7;
    the_status.mode = HEARING_MODE_CINEMA;
    the_status.muted = true;
    status_read_result = 0;

    CHECK(status_encode(&the_status, expected, sizeof expected, &expected_len) == DEFGEN_OK);

    n = attr->read(NULL, attr, buf, (uint16_t)sizeof buf, 0);
    CHECK(n == (ssize_t)STATUS_SIZE);
    CHECK(memcmp(buf, expected, STATUS_SIZE) == 0);
}

/* A hook that refuses fails the ATT operation rather than returning a value. */
static void a_refused_read_becomes_an_att_error(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_status_char_attr();
    uint8_t buf[STATUS_SIZE];
    ssize_t n;

    status_read_result = -EACCES;
    n = attr->read(NULL, attr, buf, (uint16_t)sizeof buf, 0);
    CHECK(n == BT_GATT_ERR(BT_ATT_ERR_READ_NOT_PERMITTED));
    status_read_result = 0;
}

/* An ATT write decodes before the application sees it. */
static void a_write_decodes_before_the_hook_runs(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_command_char_attr();
    Command command;
    uint8_t buf[COMMAND_SIZE];
    size_t len;
    ssize_t n;

    memset(&command, 0, sizeof command);
    command.id = COMMAND_SET_VOLUME;
    command.payload.set_volume.volume = 9;
    CHECK(command_encode(&command, buf, sizeof buf, &len) == DEFGEN_OK);

    command_writes = 0;
    command_write_result = 0;
    n = attr->write(NULL, attr, buf, (uint16_t)len, 0, 0);
    CHECK(n == (ssize_t)len);
    CHECK(command_writes == 1);
    CHECK(last_command.id == COMMAND_SET_VOLUME);
    CHECK(last_command.payload.set_volume.volume == 9);
}

/* A payload the codec rejects never reaches the application. */
static void a_write_of_the_wrong_length_is_rejected(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_command_char_attr();
    uint8_t buf[COMMAND_SIZE];
    ssize_t n;

    memset(buf, 0, sizeof buf);
    command_writes = 0;
    n = attr->write(NULL, attr, buf, (uint16_t)(COMMAND_SIZE - 1), 0, 0);
    CHECK(n == BT_GATT_ERR(BT_ATT_ERR_INVALID_ATTRIBUTE_LEN));
    CHECK(command_writes == 0);
}

/* A partial write has no meaning for a fixed layout (§6). */
static void a_write_at_an_offset_is_rejected(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_command_char_attr();
    uint8_t buf[COMMAND_SIZE];
    ssize_t n;

    memset(buf, 0, sizeof buf);
    command_writes = 0;
    n = attr->write(NULL, attr, buf, (uint16_t)sizeof buf, 1, 0);
    CHECK(n == BT_GATT_ERR(BT_ATT_ERR_INVALID_OFFSET));
    n = attr->write(NULL, attr, buf, (uint16_t)sizeof buf, 0, BT_GATT_WRITE_FLAG_PREPARE);
    CHECK(n == BT_GATT_ERR(BT_ATT_ERR_NOT_SUPPORTED));
    CHECK(command_writes == 0);
}

/* The notify helper encodes the same bytes a read would have produced. */
static void notify_sends_the_encoded_value(void)
{
    uint8_t expected[STATUS_SIZE];
    size_t expected_len;

    memset(&the_status, 0, sizeof the_status);
    the_status.mode = HEARING_MODE_STEREO;
    the_status.volume = 3;
    CHECK(status_encode(&the_status, expected, sizeof expected, &expected_len) == DEFGEN_OK);

    notify_calls = 0;
    CHECK(hearing_aid_control_status_char_notify(NULL, &the_status) == 0);
    CHECK(notify_calls == 1);
    CHECK(last_notify_len == STATUS_SIZE);
    CHECK(memcmp(last_notify, expected, STATUS_SIZE) == 0);
}

/* A variable-length value (§6.3) is sent at its actual length, not padded out
   to the maximum the type allows. */
static void a_variable_length_read_sends_only_what_was_written(void)
{
    const struct bt_gatt_attr *attr = hearing_aid_control_owner_name_char_attr();
    uint8_t buf[OWNER_NAME_MAX_SIZE];
    ssize_t n;

    memset(buf, 0xff, sizeof buf);
    n = attr->read(NULL, attr, buf, (uint16_t)sizeof buf, 0);
    CHECK(n == 3);
    CHECK(memcmp(buf, "Ada", 3) == 0);
}

/* Subscription state follows the CCC the table registered for a notifiable
   characteristic. */
static void the_ccc_tracks_subscription(void)
{
    /* Attribute 3: primary service, StatusChar declaration, StatusChar value,
       then its CCC. */
    const struct bt_gatt_attr *ccc = hearing_aid_control_status_char_attr() + 1;

    CHECK(strcmp(ccc->kind, "ccc") == 0);
    CHECK(ccc->ccc_changed != NULL);

    CHECK(hearing_aid_control_status_char_is_subscribed() == false);
    ccc->ccc_changed(ccc, 1);
    CHECK(hearing_aid_control_status_char_is_subscribed() == true);
    ccc->ccc_changed(ccc, 0);
    CHECK(hearing_aid_control_status_char_is_subscribed() == false);
}

int main(void)
{
    the_accessors_find_value_attributes();
    the_table_wires_only_the_declared_directions();
    a_read_encodes_what_the_hook_supplied();
    a_refused_read_becomes_an_att_error();
    a_write_decodes_before_the_hook_runs();
    a_write_of_the_wrong_length_is_rejected();
    a_write_at_an_offset_is_rejected();
    notify_sends_the_encoded_value();
    a_variable_length_read_sends_only_what_was_written();
    the_ccc_tracks_subscription();

    if (failures != 0) {
        printf("%d check(s) failed\n", failures);
        return 1;
    }
    printf("zephyr conformance: OK\n");
    return 0;
}
