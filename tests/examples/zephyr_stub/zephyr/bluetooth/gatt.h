/*
 * Stand-in for Zephyr's <zephyr/bluetooth/gatt.h>, for the Zephyr stack's
 * tests.
 *
 * These headers exist so CI can compile and run what `defgen server --stack
 * zephyr` generates without a Zephyr checkout. They are not a Zephyr
 * emulation: nothing here speaks Bluetooth, and the test fixture supplies the
 * bodies of `bt_gatt_attr_read`, `bt_gatt_notify` and `bt_gatt_indicate`.
 *
 * What they do reproduce faithfully is the one thing the generated table can
 * get *wrong*: how many attributes each macro contributes, and in what order.
 * `BT_GATT_CHARACTERISTIC` expands to two attributes and `BT_GATT_CCC` to one,
 * exactly as in Zephyr, so an attribute index the generator miscounted shows
 * up as a failing test rather than as a device that notifies the wrong handle.
 * The `kind` field is what lets the fixture check that — it has no Zephyr
 * counterpart.
 */
#ifndef DEFGEN_TEST_ZEPHYR_BLUETOOTH_GATT_H
#define DEFGEN_TEST_ZEPHYR_BLUETOOTH_GATT_H

#include <stddef.h>
#include <stdint.h>

#include <zephyr/bluetooth/conn.h>
#include <zephyr/bluetooth/uuid.h>

#ifdef __SSIZE_TYPE__
typedef __SSIZE_TYPE__ ssize_t;
#else
typedef long ssize_t;
#endif

/* Characteristic properties, as advertised in the declaration. */
#define BT_GATT_CHRC_READ                  0x02
#define BT_GATT_CHRC_WRITE_WITHOUT_RESP    0x04
#define BT_GATT_CHRC_WRITE                 0x08
#define BT_GATT_CHRC_NOTIFY                0x10
#define BT_GATT_CHRC_INDICATE              0x20

/* ATT permissions on the attribute itself. */
#define BT_GATT_PERM_NONE                  0x00
#define BT_GATT_PERM_READ                  0x01
#define BT_GATT_PERM_WRITE                 0x02

#define BT_GATT_WRITE_FLAG_PREPARE         0x01

/* A callback returns a negated ATT error code. */
#define BT_GATT_ERR(_att_err) (-(_att_err))

struct bt_gatt_attr;

typedef ssize_t (*bt_gatt_attr_read_func_t)(struct bt_conn *conn, const struct bt_gatt_attr *attr, void *buf,
                                            uint16_t len, uint16_t offset);

typedef ssize_t (*bt_gatt_attr_write_func_t)(struct bt_conn *conn, const struct bt_gatt_attr *attr,
                                             const void *buf, uint16_t len, uint16_t offset, uint8_t flags);

typedef void (*bt_gatt_ccc_cfg_changed_func_t)(const struct bt_gatt_attr *attr, uint16_t value);

struct bt_gatt_attr {
    const struct bt_uuid *uuid;
    bt_gatt_attr_read_func_t read;
    bt_gatt_attr_write_func_t write;
    void *user_data;
    uint16_t perm;
    /* Test-only: which macro produced this attribute. No Zephyr counterpart. */
    const char *kind;
    bt_gatt_ccc_cfg_changed_func_t ccc_changed;
};

struct bt_gatt_service_static {
    const struct bt_gatt_attr *attrs;
    size_t attr_count;
};

#define BT_GATT_PRIMARY_SERVICE(_service) \
    { (const struct bt_uuid *)(_service), NULL, NULL, NULL, 0, "service", NULL }

/* Two attributes, as in Zephyr: the characteristic declaration, then its
   value. Only the value attribute carries the callbacks. */
#define BT_GATT_CHARACTERISTIC(_uuid, _props, _perm, _read, _write, _user_data)             \
    { NULL, NULL, NULL, NULL, (uint16_t)(_props), "declaration", NULL },                    \
    { (_uuid), (_read), (_write), (_user_data), (uint16_t)(_perm), "value", NULL }

#define BT_GATT_CCC(_changed, _perm) \
    { NULL, NULL, NULL, NULL, (uint16_t)(_perm), "ccc", (_changed) }

#define BT_GATT_SERVICE_DEFINE(_name, ...)                                       \
    static const struct bt_gatt_attr _name##_attrs[] = { __VA_ARGS__ };           \
    static const struct bt_gatt_service_static _name = {                          \
        _name##_attrs, sizeof(_name##_attrs) / sizeof((_name##_attrs)[0])         \
    }

struct bt_gatt_indicate_params;

typedef void (*bt_gatt_indicate_func_t)(struct bt_conn *conn, struct bt_gatt_indicate_params *params,
                                        uint8_t err);

struct bt_gatt_indicate_params {
    const struct bt_gatt_attr *attr;
    bt_gatt_indicate_func_t func;
    const void *data;
    uint16_t len;
};

/* Defined by the test fixture, not here: a stub with a body would be an
   unused static function in every other translation unit. */
ssize_t bt_gatt_attr_read(struct bt_conn *conn, const struct bt_gatt_attr *attr, void *buf, uint16_t buf_len,
                          uint16_t offset, const void *value, uint16_t value_len);
int bt_gatt_notify(struct bt_conn *conn, const struct bt_gatt_attr *attr, const void *data, uint16_t len);
int bt_gatt_indicate(struct bt_conn *conn, struct bt_gatt_indicate_params *params);

#endif /* DEFGEN_TEST_ZEPHYR_BLUETOOTH_GATT_H */
