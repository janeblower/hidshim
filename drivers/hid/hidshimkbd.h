/*
 * Protocol of the hidshim keyboard: between the SDK and the control device
 * (hidshimkbdctl), and between the control device and the HID minidriver
 * (hidshimhid). crates/sdk/src/kbdproto.rs mirrors this header.
 */

#pragma once

#define HIDSHIM_KBD_PROTOCOL_VERSION 1

/* {428DA1F6-1BFE-4C29-9BB4-D6AA00B39A4B} */
#define HIDSHIM_KBD_CONTROL_GUID_INIT \
    { 0x428DA1F6, 0x1BFE, 0x4C29, { 0x9B, 0xB4, 0xD6, 0xAA, 0x00, 0xB3, 0x9A, 0x4B } }

/* Opening this reference claims the keyboard; a second open gets
 * STATUS_DEVICE_BUSY. SYSTEM and Administrators only. */
#define HIDSHIM_KBD_CONTROL_REFERENCE L"ctl"
/* Reaches the device without claiming it, for REMOVE and GET_INFO. */
#define HIDSHIM_KBD_ADMIN_REFERENCE L"admin"
/* The HID minidriver's own handle, from its WUDFHost only. */
#define HIDSHIM_KBD_HID_REFERENCE L"hid"

#define IOCTL_HIDSHIM_KBD_GET_VERSION \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)
/* admin only, from an administrator: fails both devices so PnP removes them
 * as unplugged, without vetoes from open handles. */
#define IOCTL_HIDSHIM_KBD_REMOVE \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* admin only. Answers with HIDSHIM_KBD_INFO. */
#define IOCTL_HIDSHIM_KBD_GET_INFO \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS)
/* Owner only, once per claim. Answers with HIDSHIM_KBD_MAPPING: handles, in
 * the owner's process, to a fresh HIDSHIM_KBD_SHARED section and to the event
 * that wakes the HID minidriver. STATUS_DEVICE_NOT_CONNECTED while the HID
 * minidriver is not attached. */
#define IOCTL_HIDSHIM_KBD_MAP_REPORT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x804, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* Owner only. Stays pending while the claim is alive and the HID minidriver
 * attached; completes once either is gone, which a writer of the section
 * cannot see otherwise. */
#define IOCTL_HIDSHIM_KBD_HOLD \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x805, METHOD_BUFFERED, FILE_WRITE_ACCESS)
/* hid only. Input: the last generation seen (ULONG). Stays pending until the
 * generation differs, then answers with HIDSHIM_KBD_ATTACH. */
#define IOCTL_HIDSHIM_KBD_ATTACH \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x810, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* HIDSHIM_KBD_ATTACH.Flags: a claim is mapped; Section and Event are valid. */
#define HIDSHIM_KBD_OWNED 0x1
/* Uninstall: the HID minidriver fails its device. */
#define HIDSHIM_KBD_REMOVING 0x2

typedef struct {
    ULONG Version;
    ULONG Capabilities;
} HIDSHIM_KBD_VERSION;

typedef struct {
    ULONG Owned;
    /* Process that claimed the keyboard, 0 when it is free. */
    ULONG OwnerProcessId;
    ULONG HidConnected;
    /* WUDFHost of the HID minidriver, 0 when it is not attached. */
    ULONG HidProcessId;
} HIDSHIM_KBD_INFO;

typedef struct {
    ULONG64 Section;
    ULONG64 Event;
} HIDSHIM_KBD_MAPPING;

typedef struct {
    ULONG Generation;
    ULONG Flags;
    /* Handles in the HID minidriver's process, valid with HIDSHIM_KBD_OWNED. */
    ULONG64 Section;
    ULONG64 Event;
} HIDSHIM_KBD_ATTACH;

#define HIDSHIM_KBD_REPORT_KEYBOARD 1
#define HIDSHIM_KBD_REPORT_CONSUMER 2
#define HIDSHIM_KBD_REPORT_MOUSE 3
/* Lengths with the report ID byte. */
#define HIDSHIM_KBD_KEYBOARD_LEN 30
#define HIDSHIM_KBD_CONSUMER_LEN 9
#define HIDSHIM_KBD_MOUSE_LEN 8

#define HIDSHIM_KBD_SLOTS 64

typedef struct {
    /* [0] is the report ID; bytes past the report's length are ignored. */
    UCHAR Report[32];
} HIDSHIM_KBD_SLOT;

/*
 * The section: a ring of ready reports, one writer and one reader, so that a
 * release and a press sent back to back both reach Windows.
 *
 * The owner fills slots Head % 64 onward, publishes Head with release
 * semantics, then a full barrier, then reads Waiting and sets the event if it
 * is non-zero. Head and Tail grow without a mask and wrap at 2^32.
 *
 * The driver keeps the real Tail to itself and only copies it here for the
 * owner to count free slots. It sets Waiting while Windows waits for a report
 * and the ring is empty.
 */
typedef struct {
    ULONG Head;
    ULONG Tail;
    ULONG Waiting;
    ULONG Reserved;
    HIDSHIM_KBD_SLOT Slots[HIDSHIM_KBD_SLOTS];
} HIDSHIM_KBD_SHARED;

C_ASSERT(sizeof(HIDSHIM_KBD_VERSION) == 8);
C_ASSERT(sizeof(HIDSHIM_KBD_INFO) == 16);
C_ASSERT(sizeof(HIDSHIM_KBD_MAPPING) == 16);
C_ASSERT(sizeof(HIDSHIM_KBD_ATTACH) == 24);
C_ASSERT(sizeof(HIDSHIM_KBD_SHARED) == 2064);
