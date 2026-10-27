/*
 * hidshim XUSB pad: a UMDF2 function driver that xinput1_4 and
 * Windows.Gaming.Input find through the XUSB device interface.
 *
 * The SDK opens the control interface and sends reports. The XUSB interface
 * is enabled only while that control handle is open, so the pad exists for
 * XInput exactly as long as its owner does.
 */

#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#include <wdf.h>

#include "hidshim.h"

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD PadDeviceAdd;
EVT_WDF_DEVICE_FILE_CREATE PadFileCreate;
EVT_WDF_FILE_CLEANUP PadFileCleanup;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL PadIoControl;

static const GUID XUSB_GUID =
    { 0xEC87F1E3, 0xC13B, 0x4100, { 0xB5, 0xF7, 0x8B, 0x84, 0xD5, 0x42, 0x60, 0xCB } };
static const GUID CONTROL_GUID = HIDSHIM_CONTROL_GUID_INIT;
static const GUID HID_GUID =
    { 0x4D1E55B2, 0xF16F, 0x11CF, { 0x88, 0xCB, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30 } };

#define IOCTL_XUSB_GET_INFORMATION    0x80006000
#define IOCTL_XUSB_GET_CAPABILITIES   0x8000E004
#define IOCTL_XUSB_GET_LED_STATE      0x8000E008
#define IOCTL_XUSB_GET_STATE          0x8000E00C
#define IOCTL_XUSB_SET_STATE          0x8000A010
#define IOCTL_XUSB_WAIT_GUIDE         0x8000E014
#define IOCTL_XUSB_GET_BATTERY_INFO   0x8000E018
#define IOCTL_XUSB_GET_INFORMATION_EX 0x8000E3FC
#define IOCTL_XUSB_WAIT_FOR_INPUT     0x8000E3AC
#define IOCTL_XUSB_POWER_INFO         0x80006380
/* GameInput's service hides and shows controllers by input focus. */
#define IOCTL_XUSB_HIDE               0x8000A390
#define IOCTL_XUSB_SHOW               0x8000A394

#define XUSB_VERSION 0x0103
#define XUSB_STATE_SIZE 29
#define PAD_VID 0x045E
#define PAD_PID 0x028E

typedef struct PAD_CTX PAD_CTX;

/* The owner's section and the event it sets when the driver waits. */
typedef struct {
    PAD_CTX *Ctx;
    HANDLE Section;
    volatile HIDSHIM_SHARED *View;
    HANDLE Event;
    PTP_WAIT Wait;
} MAPPING;

struct PAD_CTX {
    /* Serializes ownership changes with the interface state that follows
     * them, and guards InterfacesCreated. */
    WDFWAITLOCK OwnerLock;
    BOOLEAN InterfacesCreated;
    /* Guards everything below. */
    WDFWAITLOCK Lock;
    WDFFILEOBJECT Owner;
    ULONG OwnerProcessId;
    HIDSHIM_GAMEPAD_REPORT Report;
    ULONG Packet;
    WDFQUEUE WaitQueue;
    WDFQUEUE HoldQueue;
    MAPPING *Mapping;
};

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(PAD_CTX, GetPadCtx)

typedef struct {
    BOOLEAN Control;
    ULONG SeenPacket;
} FILE_CTX;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(FILE_CTX, GetFileCtx)

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, PadDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}

NTSTATUS PadDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    NTSTATUS status;
    WDFDEVICE device;
    PAD_CTX *ctx;
    UNREFERENCED_PARAMETER(Driver);

    WDF_FILEOBJECT_CONFIG fileConfig;
    WDF_FILEOBJECT_CONFIG_INIT(&fileConfig, PadFileCreate, WDF_NO_EVENT_CALLBACK, PadFileCleanup);
    WDF_OBJECT_ATTRIBUTES fileAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttrs, FILE_CTX);
    WdfDeviceInitSetFileObjectConfig(DeviceInit, &fileConfig, &fileAttrs);

    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attrs, PAD_CTX);
    status = WdfDeviceCreate(&DeviceInit, &attrs, &device);
    if (!NT_SUCCESS(status)) return status;
    ctx = GetPadCtx(device);

    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    attrs.ParentObject = device;
    status = WdfWaitLockCreate(&attrs, &ctx->OwnerLock);
    if (!NT_SUCCESS(status)) return status;
    status = WdfWaitLockCreate(&attrs, &ctx->Lock);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG queueConfig;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoDeviceControl = PadIoControl;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchManual);
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &ctx->WaitQueue);
    if (!NT_SUCCESS(status)) return status;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &ctx->HoldQueue);
    if (!NT_SUCCESS(status)) return status;

    UNICODE_STRING reference;
    RtlInitUnicodeString(&reference, HIDSHIM_CONTROL_REFERENCE);
    status = WdfDeviceCreateDeviceInterface(device, &CONTROL_GUID, &reference);
    if (!NT_SUCCESS(status)) return status;
    RtlInitUnicodeString(&reference, HIDSHIM_ADMIN_REFERENCE);
    return WdfDeviceCreateDeviceInterface(device, &CONTROL_GUID, &reference);
}

/*
 * The pad's interfaces are created on the first claim rather than here: the
 * framework enables interfaces at start, which would show the pad for a
 * moment without an owner.
 *
 * The HID-class interface carries no HID protocol. SDL rescans for XInput
 * pads only when a HID interface arrives or leaves, so without it Steam and
 * SDL games never see a pad that appears after they start. WGI ignores it
 * for devices with the xinputhid filter string.
 */
#define NOTIFY_REFERENCE L"notify"

static VOID SetNotifyState(WDFDEVICE Device, BOOLEAN enabled)
{
    UNICODE_STRING reference;
    RtlInitUnicodeString(&reference, NOTIFY_REFERENCE);
    WdfDeviceSetDeviceInterfaceState(Device, &HID_GUID, &reference, enabled);
}

static NTSTATUS ShowPad(WDFDEVICE Device, PAD_CTX *ctx)
{
    if (!ctx->InterfacesCreated) {
        UNICODE_STRING reference;
        NTSTATUS status = WdfDeviceCreateDeviceInterface(Device, &XUSB_GUID, NULL);
        if (!NT_SUCCESS(status)) return status;
        RtlInitUnicodeString(&reference, NOTIFY_REFERENCE);
        status = WdfDeviceCreateDeviceInterface(Device, &HID_GUID, &reference);
        if (!NT_SUCCESS(status)) return status;
        ctx->InterfacesCreated = TRUE;
    }
    WdfDeviceSetDeviceInterfaceState(Device, &XUSB_GUID, NULL, TRUE);
    SetNotifyState(Device, TRUE);
    return STATUS_SUCCESS;
}

static VOID HidePad(WDFDEVICE Device)
{
    SetNotifyState(Device, FALSE);
    WdfDeviceSetDeviceInterfaceState(Device, &XUSB_GUID, NULL, FALSE);
}

static BOOLEAN IsControlOpen(WDFFILEOBJECT FileObject)
{
    PUNICODE_STRING name = WdfFileObjectGetFileName(FileObject);
    UNICODE_STRING reference;
    if (name == NULL || name->Length == 0) return FALSE;
    RtlInitUnicodeString(&reference, HIDSHIM_CONTROL_REFERENCE);
    UNICODE_STRING tail = *name;
    if (tail.Buffer[0] == L'\\') {
        tail.Buffer++;
        tail.Length -= sizeof(WCHAR);
        tail.MaximumLength -= sizeof(WCHAR);
    }
    return tail.Length == reference.Length
        && CompareStringOrdinal(tail.Buffer, tail.Length / sizeof(WCHAR),
                                reference.Buffer, reference.Length / sizeof(WCHAR), TRUE) == CSTR_EQUAL;
}

static VOID BuildState(const PAD_CTX *ctx, BOOLEAN waitReply, UCHAR state[XUSB_STATE_SIZE])
{
    RtlZeroMemory(state, XUSB_STATE_SIZE);
    *(UNALIGNED USHORT *)&state[0] = XUSB_VERSION;
    if (waitReply) {
        /* WGI: 3 = resumed; byte 9 is the report ID its parser template
         * expects, byte 10 a non-zero gate before the XINPUT_GAMEPAD. */
        state[2] = 0x03;
        state[9] = 0x00;
        state[10] = 0x14;
    } else {
        state[2] = 0x01;
    }
    *(UNALIGNED ULONG *)&state[5] = ctx->Packet;
    RtlCopyMemory(&state[11], &ctx->Report, sizeof(ctx->Report));
}

static VOID CompleteWithData(WDFREQUEST Request, const VOID *data, size_t len, size_t minLen)
{
    PVOID out;
    size_t outLen;
    NTSTATUS status = WdfRequestRetrieveOutputBuffer(Request, minLen, &out, &outLen);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    if (len > outLen) len = outLen;
    RtlCopyMemory(out, data, len);
    WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, len);
}

static VOID CompleteWait(PAD_CTX *ctx, WDFREQUEST Request)
{
    UCHAR state[XUSB_STATE_SIZE];
    WDFFILEOBJECT file = WdfRequestGetFileObject(Request);
    if (file != NULL) GetFileCtx(file)->SeenPacket = ctx->Packet;
    BuildState(ctx, TRUE, state);
    CompleteWithData(Request, state, sizeof(state), sizeof(state));
}

/* Caller holds ctx->Lock. Tells the owner whether a report must wake the
 * driver. A wait cancelled since only costs the owner one needless wake. */
static VOID SetWaitingLocked(PAD_CTX *ctx, BOOLEAN waiting)
{
    if (ctx->Mapping == NULL) return;
    InterlockedExchange((volatile LONG *)&ctx->Mapping->View->Waiting, waiting);
}

/* Caller holds ctx->Lock. */
static VOID RefreshWaitingLocked(PAD_CTX *ctx)
{
    ULONG parked = 0;
    WdfIoQueueGetState(ctx->WaitQueue, &parked, NULL);
    SetWaitingLocked(ctx, parked != 0);
}

/* Caller holds ctx->Lock. */
static VOID PublishLocked(PAD_CTX *ctx, const HIDSHIM_GAMEPAD_REPORT *report)
{
    WDFREQUEST waiting;
    if (RtlEqualMemory(&ctx->Report, report, sizeof(*report))) return;
    ctx->Report = *report;
    ctx->Packet++;
    while (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(ctx->WaitQueue, &waiting))) {
        CompleteWait(ctx, waiting);
    }
    SetWaitingLocked(ctx, FALSE);
}

/* Caller holds ctx->Lock. Any bytes are a valid report, so a writer that
 * never settles only costs a stale report, not a wrong one. */
static VOID SyncSharedLocked(PAD_CTX *ctx)
{
    volatile const HIDSHIM_SHARED *shared;
    HIDSHIM_GAMEPAD_REPORT report;
    if (ctx->Mapping == NULL) return;
    shared = ctx->Mapping->View;
    for (int attempt = 0; attempt < 16; attempt++) {
        ULONG before = ReadAcquire((volatile LONG *)&shared->Sequence);
        if (before & 1) {
            YieldProcessor();
            continue;
        }
        /* Volatile word loads stay between the two reads of Sequence. */
        for (int word = 0; word < sizeof(report) / sizeof(ULONG); word++) {
            ((ULONG *)&report)[word] = ((volatile const ULONG *)&shared->Report)[word];
        }
        if ((ULONG)ReadAcquire((volatile LONG *)&shared->Sequence) == before) {
            PublishLocked(ctx, &report);
            return;
        }
    }
}

static VOID CALLBACK MappingWoken(
    PTP_CALLBACK_INSTANCE Instance, PVOID Context, PTP_WAIT Wait, TP_WAIT_RESULT WaitResult)
{
    MAPPING *mapping = Context;
    PAD_CTX *ctx = mapping->Ctx;
    UNREFERENCED_PARAMETER(Instance);
    UNREFERENCED_PARAMETER(WaitResult);

    WdfWaitLockAcquire(ctx->Lock, NULL);
    /* Unmapped meanwhile: DestroyMapping is waiting for this callback. */
    if (ctx->Mapping == mapping) {
        SyncSharedLocked(ctx);
        RefreshWaitingLocked(ctx);
        SetThreadpoolWait(Wait, mapping->Event, NULL);
    }
    WdfWaitLockRelease(ctx->Lock);
}

/* Without ctx->Lock held: a running MappingWoken takes it. */
static VOID DestroyMapping(MAPPING *mapping)
{
    if (mapping == NULL) return;
    if (mapping->Wait != NULL) {
        SetThreadpoolWait(mapping->Wait, NULL, NULL);
        WaitForThreadpoolWaitCallbacks(mapping->Wait, TRUE);
        CloseThreadpoolWait(mapping->Wait);
    }
    if (mapping->Event != NULL) CloseHandle(mapping->Event);
    if (mapping->View != NULL) UnmapViewOfFile((LPCVOID)mapping->View);
    if (mapping->Section != NULL) CloseHandle(mapping->Section);
    HeapFree(GetProcessHeap(), 0, mapping);
}

static MAPPING *CreateMapping(PAD_CTX *ctx)
{
    MAPPING *mapping = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*mapping));
    if (mapping == NULL) return NULL;
    mapping->Ctx = ctx;
    /* Backed by the paging file: a file-backed view could fault on a read. */
    mapping->Section = CreateFileMappingW(
        INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, sizeof(HIDSHIM_SHARED), NULL);
    if (mapping->Section != NULL) {
        mapping->View = MapViewOfFile(mapping->Section, FILE_MAP_WRITE, 0, 0, sizeof(HIDSHIM_SHARED));
    }
    mapping->Event = CreateEventW(NULL, FALSE, FALSE, NULL);
    mapping->Wait = CreateThreadpoolWait(MappingWoken, mapping, NULL);
    if (mapping->View == NULL || mapping->Event == NULL || mapping->Wait == NULL) {
        DestroyMapping(mapping);
        return NULL;
    }
    return mapping;
}

/* Caller holds ctx->Lock and destroys the returned mapping after releasing it. */
static MAPPING *UnmapLocked(PAD_CTX *ctx)
{
    MAPPING *mapping = ctx->Mapping;
    ctx->Mapping = NULL;
    return mapping;
}

VOID PadFileCreate(_In_ WDFDEVICE Device, _In_ WDFREQUEST Request, _In_ WDFFILEOBJECT FileObject)
{
    PAD_CTX *ctx = GetPadCtx(Device);
    FILE_CTX *file = GetFileCtx(FileObject);
    NTSTATUS status = STATUS_SUCCESS;

    file->Control = IsControlOpen(FileObject);
    if (!file->Control) {
        WdfWaitLockAcquire(ctx->Lock, NULL);
        /* A fresh XInput handle gets the current state on its first wait. */
        file->SeenPacket = ctx->Packet - 1;
        WdfWaitLockRelease(ctx->Lock);
        WdfRequestComplete(Request, STATUS_SUCCESS);
        return;
    }

    WdfWaitLockAcquire(ctx->OwnerLock, NULL);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->Owner != NULL) {
        status = STATUS_DEVICE_BUSY;
    } else {
        ctx->Owner = FileObject;
        ctx->OwnerProcessId = WdfRequestGetRequestorProcessId(Request);
        RtlZeroMemory(&ctx->Report, sizeof(ctx->Report));
        ctx->Packet++;
    }
    WdfWaitLockRelease(ctx->Lock);
    if (NT_SUCCESS(status)) {
        status = ShowPad(Device, ctx);
        if (!NT_SUCCESS(status)) {
            WdfWaitLockAcquire(ctx->Lock, NULL);
            ctx->Owner = NULL;
            ctx->OwnerProcessId = 0;
            WdfWaitLockRelease(ctx->Lock);
        }
    }
    WdfWaitLockRelease(ctx->OwnerLock);
    WdfRequestComplete(Request, status);
}

VOID PadFileCleanup(_In_ WDFFILEOBJECT FileObject)
{
    PAD_CTX *ctx = GetPadCtx(WdfFileObjectGetDevice(FileObject));
    HIDSHIM_GAMEPAD_REPORT neutral;
    BOOLEAN released = FALSE;
    MAPPING *unmapped = NULL;

    if (!GetFileCtx(FileObject)->Control) return;

    RtlZeroMemory(&neutral, sizeof(neutral));
    WdfWaitLockAcquire(ctx->OwnerLock, NULL);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->Owner == FileObject) {
        WDFREQUEST waiting;
        unmapped = UnmapLocked(ctx);
        PublishLocked(ctx, &neutral);
        ctx->Owner = NULL;
        ctx->OwnerProcessId = 0;
        released = TRUE;
        /* xusb22 fails parked waits this way when a controller goes away. */
        while (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(ctx->WaitQueue, &waiting))) {
            WdfRequestComplete(waiting, STATUS_DEVICE_REMOVED);
        }
        while (NT_SUCCESS(WdfIoQueueRetrieveRequestByFileObject(ctx->HoldQueue, FileObject, &waiting))) {
            WdfRequestComplete(waiting, STATUS_SUCCESS);
        }
    }
    WdfWaitLockRelease(ctx->Lock);
    DestroyMapping(unmapped);
    if (released) {
        HidePad(WdfFileObjectGetDevice(FileObject));
    }
    WdfWaitLockRelease(ctx->OwnerLock);
}

static VOID HandleSetReport(PAD_CTX *ctx, WDFREQUEST Request)
{
    PVOID in;
    size_t inLen;
    NTSTATUS status = WdfRequestRetrieveInputBuffer(Request, sizeof(HIDSHIM_GAMEPAD_REPORT), &in, &inLen);
    if (!NT_SUCCESS(status) || inLen != sizeof(HIDSHIM_GAMEPAD_REPORT)) {
        WdfRequestComplete(Request, STATUS_INVALID_PARAMETER);
        return;
    }
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->Owner == NULL || ctx->Owner != WdfRequestGetFileObject(Request)) {
        status = STATUS_ACCESS_DENIED;
    } else {
        PublishLocked(ctx, (const HIDSHIM_GAMEPAD_REPORT *)in);
    }
    WdfWaitLockRelease(ctx->Lock);
    WdfRequestComplete(Request, status);
}

typedef struct {
    ULONG ProcessId;
    HANDLE Process;
} OPEN_OWNER;

/* WUDFHost runs as LocalService, which may not open the owner's process; the
 * owner itself may. */
static VOID OpenOwner(_In_ WDFREQUEST Request, _In_opt_ PVOID Context)
{
    OPEN_OWNER *owner = Context;
    UNREFERENCED_PARAMETER(Request);
    owner->Process = OpenProcess(PROCESS_DUP_HANDLE, FALSE, owner->ProcessId);
}

static BOOLEAN IsOwnerLocked(PAD_CTX *ctx, WDFREQUEST Request)
{
    return ctx->Owner != NULL && ctx->Owner == WdfRequestGetFileObject(Request);
}

/* Puts the mapping's section and event into the requestor's process. */
static NTSTATUS GiveMapping(WDFREQUEST Request, MAPPING *mapping, HIDSHIM_MAPPING *given)
{
    HANDLE section, event;
    OPEN_OWNER owner = { WdfRequestGetRequestorProcessId(Request), NULL };
    NTSTATUS status = WdfRequestImpersonate(Request, SecurityImpersonation, OpenOwner, &owner);
    if (!NT_SUCCESS(status)) return status;
    if (owner.Process == NULL) return STATUS_ACCESS_DENIED;
    status = STATUS_INSUFFICIENT_RESOURCES;
    if (DuplicateHandle(GetCurrentProcess(), mapping->Section, owner.Process, &section,
                        FILE_MAP_READ | FILE_MAP_WRITE, FALSE, 0)) {
        /* The owner may only set the event, not wait on it or reset it. */
        if (DuplicateHandle(GetCurrentProcess(), mapping->Event, owner.Process, &event,
                            EVENT_MODIFY_STATE, FALSE, 0)) {
            given->Section = (ULONG64)(ULONG_PTR)section;
            given->Event = (ULONG64)(ULONG_PTR)event;
            status = STATUS_SUCCESS;
        } else {
            DuplicateHandle(owner.Process, section, NULL, NULL, 0, FALSE, DUPLICATE_CLOSE_SOURCE);
        }
    }
    CloseHandle(owner.Process);
    return status;
}

static VOID HandleMapReport(PAD_CTX *ctx, WDFREQUEST Request)
{
    HIDSHIM_MAPPING *given;
    MAPPING *mapping;
    NTSTATUS status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*given), (PVOID *)&given, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    WdfWaitLockAcquire(ctx->Lock, NULL);
    status = !IsOwnerLocked(ctx, Request) ? STATUS_ACCESS_DENIED
        : ctx->Mapping != NULL ? STATUS_DEVICE_BUSY : STATUS_SUCCESS;
    WdfWaitLockRelease(ctx->Lock);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, status);
        return;
    }

    mapping = CreateMapping(ctx);
    if (mapping == NULL) {
        WdfRequestComplete(Request, STATUS_INSUFFICIENT_RESOURCES);
        return;
    }
    /* The section starts zeroed: the neutral report the claim published. */
    WdfWaitLockAcquire(ctx->Lock, NULL);
    status = !IsOwnerLocked(ctx, Request) ? STATUS_ACCESS_DENIED
        : ctx->Mapping != NULL ? STATUS_DEVICE_BUSY : STATUS_SUCCESS;
    if (NT_SUCCESS(status)) {
        status = GiveMapping(Request, mapping, given);
    }
    if (NT_SUCCESS(status)) {
        ctx->Mapping = mapping;
        mapping = NULL;
        RefreshWaitingLocked(ctx);
        SetThreadpoolWait(ctx->Mapping->Wait, ctx->Mapping->Event, NULL);
    }
    WdfWaitLockRelease(ctx->Lock);
    DestroyMapping(mapping);
    if (NT_SUCCESS(status)) {
        WdfRequestCompleteWithInformation(Request, status, sizeof(*given));
    } else {
        WdfRequestComplete(Request, status);
    }
}

static VOID HandleHold(PAD_CTX *ctx, WDFREQUEST Request)
{
    NTSTATUS status;
    WdfWaitLockAcquire(ctx->Lock, NULL);
    status = IsOwnerLocked(ctx, Request)
        ? WdfRequestForwardToIoQueue(Request, ctx->HoldQueue)
        : STATUS_ACCESS_DENIED;
    WdfWaitLockRelease(ctx->Lock);
    if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
}

static VOID HandleWaitForInput(PAD_CTX *ctx, WDFREQUEST Request)
{
    WDFFILEOBJECT file = WdfRequestGetFileObject(Request);
    NTSTATUS status;

    /* Pending under the lock: a report published in between would
     * otherwise leave this request parked until the next change. Waiting
     * goes up first, so the owner either sees it or wrote before the sync. */
    WdfWaitLockAcquire(ctx->Lock, NULL);
    SetWaitingLocked(ctx, TRUE);
    SyncSharedLocked(ctx);
    if (ctx->Owner == NULL) {
        status = STATUS_DEVICE_NOT_CONNECTED;
    } else if (file != NULL && GetFileCtx(file)->SeenPacket != ctx->Packet) {
        CompleteWait(ctx, Request);
        RefreshWaitingLocked(ctx);
        WdfWaitLockRelease(ctx->Lock);
        return;
    } else {
        status = WdfRequestForwardToIoQueue(Request, ctx->WaitQueue);
        if (NT_SUCCESS(status)) {
            WdfWaitLockRelease(ctx->Lock);
            return;
        }
    }
    WdfWaitLockRelease(ctx->Lock);
    WdfRequestComplete(Request, status);
}

static VOID CheckAdmin(_In_ WDFREQUEST Request, _In_opt_ PVOID Context)
{
    BYTE sid[SECURITY_MAX_SID_SIZE];
    DWORD size = sizeof(sid);
    BOOL member = FALSE;
    UNREFERENCED_PARAMETER(Request);
    if (CreateWellKnownSid(WinBuiltinAdministratorsSid, NULL, sid, &size)) {
        CheckTokenMembership(NULL, sid, &member);
    }
    *(BOOLEAN *)Context = member ? TRUE : FALSE;
}

/* Uninstall: failing the device makes PnP remove it as unplugged. An orderly
 * removal would be vetoed by WGI clients, which keep their handles open
 * through a query-remove, and leave the removal pending a reboot. */
static VOID HandleRemove(WDFDEVICE Device, WDFREQUEST Request)
{
    BOOLEAN admin = FALSE;
    NTSTATUS status = WdfRequestImpersonate(Request, SecurityImpersonation, CheckAdmin, &admin);
    if (NT_SUCCESS(status) && !admin) status = STATUS_ACCESS_DENIED;
    WdfRequestComplete(Request, status);
    if (NT_SUCCESS(status)) WdfDeviceSetFailed(Device, WdfDeviceFailedNoRestart);
}

static VOID HandleGetCapabilities(WDFREQUEST Request)
{
    /* Two layouts measured against a physical wired pad's xusb22: 24 bytes,
     * or 36 with a 16-byte header when the caller (WGI) asks for 36. */
    static const UCHAR capsV1[24] = {
        0x03, 0x01,
        0x00, 0x01,
        0xFF, 0xF7,
        0xFF, 0xFF,
        0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF,
        0x00, 0x00,
        0xFF, 0xFF
    };
    static const UCHAR capsV2[36] = {
        0x03, 0x01,
        0x01, 0x01,
        0x0C, 0x00,
        PAD_VID & 0xFF, PAD_VID >> 8,
        PAD_PID & 0xFF, PAD_PID >> 8,
        0x10, 0x01,
        0x00, 0xFA, 0x34, 0x22,
        0xFF, 0xF7,
        0xFF,
        0xFF,
        0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF,
        0xFF, 0xFF, 0xFF, 0xFF,
        0x00, 0x00,
        0xFF, 0xFF
    };
    PVOID out;
    size_t outLen;
    if (!NT_SUCCESS(WdfRequestRetrieveOutputBuffer(Request, sizeof(capsV1), &out, &outLen))) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
    } else if (outLen >= sizeof(capsV2)) {
        CompleteWithData(Request, capsV2, sizeof(capsV2), sizeof(capsV2));
    } else {
        CompleteWithData(Request, capsV1, sizeof(capsV1), sizeof(capsV1));
    }
}

VOID PadIoControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode)
{
    PAD_CTX *ctx = GetPadCtx(WdfIoQueueGetDevice(Queue));
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    /* Without an owner the port answers like an empty xusb22 slot. WGI drops
     * its port when capabilities fail this way. */
    switch (IoControlCode) {
    case IOCTL_XUSB_GET_CAPABILITIES:
    case IOCTL_XUSB_GET_STATE:
    case IOCTL_XUSB_GET_LED_STATE:
    case IOCTL_XUSB_GET_BATTERY_INFO:
    case IOCTL_XUSB_SET_STATE: {
        BOOLEAN connected;
        WdfWaitLockAcquire(ctx->Lock, NULL);
        connected = ctx->Owner != NULL;
        WdfWaitLockRelease(ctx->Lock);
        if (!connected) {
            WdfRequestComplete(Request, STATUS_DEVICE_NOT_CONNECTED);
            return;
        }
        break;
    }
    }

    switch (IoControlCode) {
    case IOCTL_HIDSHIM_GET_VERSION: {
        HIDSHIM_VERSION version = {
            HIDSHIM_PROTOCOL_VERSION, HIDSHIM_CAP_PAD_INFO | HIDSHIM_CAP_MAP_REPORT
        };
        CompleteWithData(Request, &version, sizeof(version), sizeof(version));
        break;
    }

    case IOCTL_HIDSHIM_GET_PAD_INFO: {
        HIDSHIM_PAD_INFO info;
        WdfWaitLockAcquire(ctx->Lock, NULL);
        info.Owned = ctx->Owner != NULL;
        info.OwnerProcessId = ctx->OwnerProcessId;
        WdfWaitLockRelease(ctx->Lock);
        CompleteWithData(Request, &info, sizeof(info), sizeof(info));
        break;
    }

    case IOCTL_HIDSHIM_SET_REPORT:
        HandleSetReport(ctx, Request);
        break;

    case IOCTL_HIDSHIM_MAP_REPORT:
        HandleMapReport(ctx, Request);
        break;

    case IOCTL_HIDSHIM_HOLD:
        HandleHold(ctx, Request);
        break;

    case IOCTL_HIDSHIM_REMOVE:
        HandleRemove(WdfIoQueueGetDevice(Queue), Request);
        break;

    case IOCTL_XUSB_GET_INFORMATION: {
        UCHAR info[12] = { 0 };
        *(UNALIGNED USHORT *)&info[0] = XUSB_VERSION;
        info[2] = 0x01;
        *(UNALIGNED USHORT *)&info[8] = PAD_VID;
        *(UNALIGNED USHORT *)&info[10] = PAD_PID;
        CompleteWithData(Request, info, sizeof(info), sizeof(info));
        break;
    }

    case IOCTL_XUSB_GET_INFORMATION_EX: {
        UCHAR info[64] = { 0 };
        *(UNALIGNED USHORT *)&info[0] = XUSB_VERSION;
        info[2] = 0x01;
        info[3] = 0x01;
        *(UNALIGNED USHORT *)&info[8] = PAD_VID;
        *(UNALIGNED USHORT *)&info[10] = PAD_PID;
        CompleteWithData(Request, info, sizeof(info), 1);
        break;
    }

    case IOCTL_XUSB_GET_CAPABILITIES:
        HandleGetCapabilities(Request);
        break;

    case IOCTL_XUSB_GET_STATE: {
        UCHAR state[XUSB_STATE_SIZE];
        WdfWaitLockAcquire(ctx->Lock, NULL);
        SyncSharedLocked(ctx);
        BuildState(ctx, FALSE, state);
        WdfWaitLockRelease(ctx->Lock);
        CompleteWithData(Request, state, sizeof(state), sizeof(state));
        break;
    }

    case IOCTL_XUSB_WAIT_FOR_INPUT:
        HandleWaitForInput(ctx, Request);
        break;

    case IOCTL_XUSB_SET_STATE:
    case IOCTL_XUSB_POWER_INFO:
    case IOCTL_XUSB_HIDE:
    case IOCTL_XUSB_SHOW:
        WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, 0);
        break;

    case IOCTL_XUSB_GET_LED_STATE: {
        static const UCHAR led[3] = { 0, 0, 0x06 };
        CompleteWithData(Request, led, sizeof(led), sizeof(led));
        break;
    }

    case IOCTL_XUSB_GET_BATTERY_INFO: {
        /* Version word, then type and level: WIRED, FULL. */
        static const UCHAR battery[4] = { 0, 0, 0x01, 0x03 };
        CompleteWithData(Request, battery, sizeof(battery), sizeof(battery));
        break;
    }

    case IOCTL_XUSB_WAIT_GUIDE:
        /* xinput1_4 falls back to polling GET_STATE. */
    default:
        WdfRequestComplete(Request, STATUS_INVALID_DEVICE_REQUEST);
        break;
    }
}
