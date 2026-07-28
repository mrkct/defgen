/*
 * Stand-in for Zephyr's <zephyr/bluetooth/uuid.h>, for the Zephyr stack's
 * tests. See gatt.h for what these stubs are and are not.
 *
 * `BT_UUID_128_ENCODE` is reproduced faithfully — it is the one piece of real
 * Zephyr behaviour the generated table depends on for correctness, since it is
 * what puts a 128-bit UUID's bytes into transmission order.
 */
#ifndef DEFGEN_TEST_ZEPHYR_BLUETOOTH_UUID_H
#define DEFGEN_TEST_ZEPHYR_BLUETOOTH_UUID_H

#include <stdint.h>

enum {
    BT_UUID_TYPE_16,
    BT_UUID_TYPE_32,
    BT_UUID_TYPE_128
};

struct bt_uuid {
    uint8_t type;
};

struct bt_uuid_16 {
    struct bt_uuid uuid;
    uint16_t val;
};

struct bt_uuid_32 {
    struct bt_uuid uuid;
    uint32_t val;
};

struct bt_uuid_128 {
    struct bt_uuid uuid;
    uint8_t val[16];
};

#define BT_UUID_INIT_16(value) { { BT_UUID_TYPE_16 }, (value) }
#define BT_UUID_INIT_32(value) { { BT_UUID_TYPE_32 }, (value) }
#define BT_UUID_INIT_128(...) { { BT_UUID_TYPE_128 }, { __VA_ARGS__ } }

#define BT_UUID_128_ENCODE(w32, w1, w2, w3, w48)                             \
    (((uint64_t)(w48) >> 0) & 0xFF), (((uint64_t)(w48) >> 8) & 0xFF),        \
    (((uint64_t)(w48) >> 16) & 0xFF), (((uint64_t)(w48) >> 24) & 0xFF),      \
    (((uint64_t)(w48) >> 32) & 0xFF), (((uint64_t)(w48) >> 40) & 0xFF),      \
    (((uint16_t)(w3) >> 0) & 0xFF), (((uint16_t)(w3) >> 8) & 0xFF),          \
    (((uint16_t)(w2) >> 0) & 0xFF), (((uint16_t)(w2) >> 8) & 0xFF),          \
    (((uint16_t)(w1) >> 0) & 0xFF), (((uint16_t)(w1) >> 8) & 0xFF),          \
    (((uint32_t)(w32) >> 0) & 0xFF), (((uint32_t)(w32) >> 8) & 0xFF),        \
    (((uint32_t)(w32) >> 16) & 0xFF), (((uint32_t)(w32) >> 24) & 0xFF)

#endif /* DEFGEN_TEST_ZEPHYR_BLUETOOTH_UUID_H */
