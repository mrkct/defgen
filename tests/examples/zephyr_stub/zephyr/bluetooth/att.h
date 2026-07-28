/*
 * Stand-in for Zephyr's <zephyr/bluetooth/att.h>, for the Zephyr stack's
 * tests. See gatt.h for what these stubs are and are not.
 *
 * The error codes carry their real values, since the tests assert on what a
 * rejected write returns.
 */
#ifndef DEFGEN_TEST_ZEPHYR_BLUETOOTH_ATT_H
#define DEFGEN_TEST_ZEPHYR_BLUETOOTH_ATT_H

#define BT_ATT_ERR_INVALID_HANDLE       0x01
#define BT_ATT_ERR_READ_NOT_PERMITTED   0x02
#define BT_ATT_ERR_WRITE_NOT_PERMITTED  0x03
#define BT_ATT_ERR_INVALID_OFFSET       0x07
#define BT_ATT_ERR_INVALID_ATTRIBUTE_LEN 0x0d
#define BT_ATT_ERR_UNLIKELY             0x0e
#define BT_ATT_ERR_NOT_SUPPORTED        0x06
#define BT_ATT_ERR_VALUE_NOT_ALLOWED    0x13

#endif /* DEFGEN_TEST_ZEPHYR_BLUETOOTH_ATT_H */
