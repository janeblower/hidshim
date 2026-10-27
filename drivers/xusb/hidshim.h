/*
 * Control protocol between the hidshim SDK and the XUSB pad driver.
 * crates/sdk/src/proto.rs mirrors this header.
 */

#pragma once

#define HIDSHIM_PROTOCOL_VERSION 1

/* {8D0B6C2E-3F1A-4E5B-9C7D-2A6E1F4B8D30} */
#define HIDSHIM_CONTROL_GUID_INIT \
    { 0x8D0B6C2E, 0x3F1A, 0x4E5B, { 0x9C, 0x7D, 0x2A, 0x6E, 0x1F, 0x4B, 0x8D, 0x30 } }

/* Distinguishes control opens from XInput opens of the same device. */
#define HIDSHIM_CONTROL_REFERENCE L"ctl"
/* Reaches the device without claiming the pad, for IOCTL_HIDSHIM_REMOVE and
 * IOCTL_HIDSHIM_GET_PAD_INFO. */
#define HIDSHIM_ADMIN_REFERENCE L"admin"

#define IOCTL_HIDSHIM_GET_VERSION \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)
#define IOCTL_HIDSHIM_SET_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* Administrators only; the driver checks the caller's token. */
#define IOCTL_HIDSHIM_REMOVE \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_ACCESS)
#define IOCTL_HIDSHIM_GET_PAD_INFO \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS)

/* Owner only, once per claim. Answers with HIDSHIM_MAPPING: handles, in the
 * owner's process, to a HIDSHIM_SHARED section the driver reads reports from
 * and to an event that wakes the driver. */
#define IOCTL_HIDSHIM_MAP_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* Owner only. Stays pending while the owner holds the pad and fails once the
 * device goes away, which a writer of the section cannot see otherwise. */
#define IOCTL_HIDSHIM_HOLD \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x805, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* HIDSHIM_VERSION.Capabilities: the driver answers IOCTL_HIDSHIM_GET_PAD_INFO. */
#define HIDSHIM_CAP_PAD_INFO 0x1
/* The driver answers IOCTL_HIDSHIM_MAP_REPORT and IOCTL_HIDSHIM_HOLD. */
#define HIDSHIM_CAP_MAP_REPORT 0x2

typedef struct {
    ULONG Version;
    ULONG Capabilities;
} HIDSHIM_VERSION;

typedef struct {
    ULONG Owned;
    /* Process that claimed the pad, 0 when it is free. */
    ULONG OwnerProcessId;
} HIDSHIM_PAD_INFO;

/* Same layout as XINPUT_GAMEPAD. */
typedef struct {
    USHORT Buttons;
    UCHAR LeftTrigger;
    UCHAR RightTrigger;
    SHORT ThumbLX;
    SHORT ThumbLY;
    SHORT ThumbRX;
    SHORT ThumbRY;
} HIDSHIM_GAMEPAD_REPORT;

typedef struct {
    ULONG64 Section;
    ULONG64 Event;
} HIDSHIM_MAPPING;

/*
 * The section. The owner makes Sequence odd, writes Report, makes Sequence
 * even, and then sets the event if Waiting is non-zero. The driver sets
 * Waiting while XInput or WGI wait for input; both sides put a full barrier
 * between their store and the other side's field, so a report is never left
 * unseen with a waiter parked.
 */
typedef struct {
    ULONG Sequence;
    HIDSHIM_GAMEPAD_REPORT Report;
    ULONG Waiting;
} HIDSHIM_SHARED;

C_ASSERT(sizeof(HIDSHIM_VERSION) == 8);
C_ASSERT(sizeof(HIDSHIM_MAPPING) == 16);
C_ASSERT(sizeof(HIDSHIM_SHARED) == 20);
C_ASSERT(sizeof(HIDSHIM_PAD_INFO) == 8);
C_ASSERT(sizeof(HIDSHIM_GAMEPAD_REPORT) == 12);
