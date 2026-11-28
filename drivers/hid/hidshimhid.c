/*
 * hidshim keyboard: a UMDF2 HID minidriver under mshidumdf with a keyboard,
 * a consumer control and a mouse collection.
 *
 * Reports come from a ring in a section shared with the keyboard's owner.
 * No one sends this driver private IOCTLs (hidclass is the top of the
 * stack), so it is itself a client of the control device (hidshimkbdctl):
 * one ATTACH stays pending there and hands over each claim's section.
 *
 * Derived from the HIDMaestro HID minidriver (MIT).
 */

#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#include <wdf.h>
#include <cfgmgr32.h>
#include <hidport.h>

#include "hidshimkbd.h"

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD HidDeviceAdd;
EVT_WDF_DEVICE_SELF_MANAGED_IO_INIT HidSelfManagedIoInit;
EVT_WDF_DEVICE_SELF_MANAGED_IO_CLEANUP HidSelfManagedIoCleanup;
EVT_WDF_DEVICE_D0_ENTRY HidD0Entry;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL HidIoControl;
EVT_WDF_REQUEST_COMPLETION_ROUTINE AttachCompleted;

static const GUID CONTROL_GUID = HIDSHIM_KBD_CONTROL_GUID_INIT;

static const UCHAR ReportDescriptor[] = {
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01,             /* Generic Desktop, Keyboard, Application */
    0x85, HIDSHIM_KBD_REPORT_KEYBOARD,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,             /* modifiers E0..E7 */
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02,
    0x95, 0x04, 0x81, 0x01,                         /* bits for usages 0..3 */
    0x19, 0x04, 0x29, 0xDF, 0x95, 0xDC, 0x81, 0x02, /* NKRO map 04..DF */
    0xC0,

    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01,             /* Consumer, Consumer Control, Application */
    0x85, HIDSHIM_KBD_REPORT_CONSUMER,
    0x19, 0x00, 0x2A, 0xFF, 0x03,
    0x15, 0x00, 0x26, 0xFF, 0x03,
    0x75, 0x10, 0x95, 0x04, 0x81, 0x00,             /* four usages, array */
    0xC0,

    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01,             /* Generic Desktop, Mouse, Application */
    0x85, HIDSHIM_KBD_REPORT_MOUSE,
    0x09, 0x01, 0xA1, 0x00,                         /* Pointer, Physical */
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05,             /* buttons 1..5 */
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81, 0x02,
    0x75, 0x03, 0x95, 0x01, 0x81, 0x01,
    /* mouhid looks for X and Y in a mouse collection. */
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x06,
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x81, 0x06,       /* AC Pan */
    0xC0,
    0xC0,
};
C_ASSERT(sizeof(ReportDescriptor) == 135);

static const HID_DESCRIPTOR HidDescriptor = {
    sizeof(HID_DESCRIPTOR), HID_HID_DESCRIPTOR_TYPE, 0x0101, 0x00, 1,
    { { HID_REPORT_DESCRIPTOR_TYPE, sizeof(ReportDescriptor) } }
};

/* Version 1 of VID/PID 0000:0000: kbdhid and mouhid ignore them, and taking
 * someone else's VID is not ours to do. */
static const HID_DEVICE_ATTRIBUTES Attributes = { sizeof(HID_DEVICE_ATTRIBUTES), 0x0000, 0x0000, 0x0001 };

static const WCHAR Manufacturer[] = L"hidshim";
static const WCHAR Product[] = L"hidshim keyboard";
static const WCHAR Serial[] = L"0";

/* hidclass's first reads all arrive within microseconds of the start. */
#define SETTLE_MS 20
#define RETRY_MS 1000
#define NEUTRAL_REPORTS 3

typedef struct HID_CTX HID_CTX;

/* The current claim's section and the event its owner sets. */
typedef struct {
    HID_CTX *Ctx;
    HANDLE Section;
    volatile HIDSHIM_KBD_SHARED *View;
    HANDLE Event;
    PTP_WAIT Wait;
} MAPPING;

struct HID_CTX {
    WDFDEVICE Device;
    WDFWAITLOCK Lock;
    /* Guarded by Lock. */
    WDFQUEUE ReadQueue;
    MAPPING *Mapping;
    ULONG Tail;
    /* A report taken from the ring or the neutral set, not yet delivered. */
    UCHAR Held[HIDSHIM_KBD_KEYBOARD_LEN];
    ULONG HeldLength;
    ULONG NeutralLeft;
    /* hidclass sent a read since the last delivery: it has finished with the
     * report before, so the next one cannot overtake it. */
    BOOLEAN Armed;
    /* All of hidclass's first reads have arrived. */
    BOOLEAN Settled;
    BOOLEAN SettleStarted;
    PTP_TIMER SettleTimer;

    /* The link to the control device. ConnectLock serializes Connect. */
    WDFWAITLOCK ConnectLock;
    PTP_WORK ConnectWork;
    PTP_TIMER RetryTimer;
    HCMNOTIFICATION Notification;
    /* Guarded by Lock. */
    WDFIOTARGET Target;
    BOOLEAN TargetDead;
    BOOLEAN Stopping;
    BOOLEAN FailDevice;
    ULONG Generation;
    LONG Sending;
    HANDLE SendIdle;
    WDFREQUEST Attach;
    WDFMEMORY AttachIn;
    WDFMEMORY AttachOut;
};

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(HID_CTX, GetHidCtx)

static VOID CALLBACK SettleFired(PTP_CALLBACK_INSTANCE, PVOID, PTP_TIMER);
static VOID CALLBACK ConnectRun(PTP_CALLBACK_INSTANCE, PVOID, PTP_WORK);
static VOID CALLBACK RetryFired(PTP_CALLBACK_INSTANCE, PVOID, PTP_TIMER);

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, HidDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}

NTSTATUS HidDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    NTSTATUS status;
    WDFDEVICE device;
    HID_CTX *ctx;
    UNREFERENCED_PARAMETER(Driver);

    /* mshidumdf is the function driver; this one sits below it. */
    WdfFdoInitSetFilter(DeviceInit);

    WDF_PNPPOWER_EVENT_CALLBACKS pnp;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDeviceSelfManagedIoInit = HidSelfManagedIoInit;
    pnp.EvtDeviceSelfManagedIoCleanup = HidSelfManagedIoCleanup;
    pnp.EvtDeviceD0Entry = HidD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &pnp);

    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attrs, HID_CTX);
    status = WdfDeviceCreate(&DeviceInit, &attrs, &device);
    if (!NT_SUCCESS(status)) return status;
    ctx = GetHidCtx(device);
    ctx->Device = device;

    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    attrs.ParentObject = device;
    status = WdfWaitLockCreate(&attrs, &ctx->Lock);
    if (!NT_SUCCESS(status)) return status;
    status = WdfWaitLockCreate(&attrs, &ctx->ConnectLock);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG queueConfig;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoDeviceControl = HidIoControl;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchManual);
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &ctx->ReadQueue);
    if (!NT_SUCCESS(status)) return status;

    ctx->SettleTimer = CreateThreadpoolTimer(SettleFired, ctx, NULL);
    ctx->RetryTimer = CreateThreadpoolTimer(RetryFired, ctx, NULL);
    ctx->ConnectWork = CreateThreadpoolWork(ConnectRun, ctx, NULL);
    ctx->SendIdle = CreateEventW(NULL, TRUE, TRUE, NULL);
    if (ctx->SettleTimer == NULL || ctx->RetryTimer == NULL || ctx->ConnectWork == NULL
        || ctx->SendIdle == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    return STATUS_SUCCESS;
}

static VOID StartTimer(PTP_TIMER timer, DWORD ms)
{
    LARGE_INTEGER due;
    FILETIME ft;
    due.QuadPart = -(LONGLONG)ms * 10000;
    ft.dwLowDateTime = due.LowPart;
    ft.dwHighDateTime = (DWORD)due.HighPart;
    SetThreadpoolTimer(timer, &ft, 0, 0);
}

static VOID StopTimer(PTP_TIMER timer)
{
    SetThreadpoolTimer(timer, NULL, 0, 0);
    WaitForThreadpoolTimerCallbacks(timer, TRUE);
}

/* Caller holds ctx->Lock. */
static VOID PublishTailLocked(HID_CTX *ctx)
{
    if (ctx->Mapping != NULL) WriteRelease((volatile LONG *)&ctx->Mapping->View->Tail, (LONG)ctx->Tail);
}

static ULONG ReportLength(UCHAR id)
{
    switch (id) {
    case HIDSHIM_KBD_REPORT_KEYBOARD: return HIDSHIM_KBD_KEYBOARD_LEN;
    case HIDSHIM_KBD_REPORT_CONSUMER: return HIDSHIM_KBD_CONSUMER_LEN;
    case HIDSHIM_KBD_REPORT_MOUSE: return HIDSHIM_KBD_MOUSE_LEN;
    default: return 0;
    }
}

/* Caller holds ctx->Lock. Fills Held with the next report: neutral ones
 * first, then the ring. The slot is copied before its report ID is looked
 * at, so the owner rewriting it meanwhile changes nothing; any payload is a
 * valid report. */
static BOOLEAN TakeReportLocked(HID_CTX *ctx)
{
    volatile HIDSHIM_KBD_SHARED *shared;
    if (ctx->HeldLength != 0) return TRUE;
    if (ctx->NeutralLeft != 0) {
        UCHAR id = (UCHAR)(NEUTRAL_REPORTS - ctx->NeutralLeft + 1);
        ctx->NeutralLeft--;
        ctx->HeldLength = ReportLength(id);
        RtlZeroMemory(ctx->Held, sizeof(ctx->Held));
        ctx->Held[0] = id;
        return TRUE;
    }
    if (ctx->Mapping == NULL) return FALSE;
    shared = ctx->Mapping->View;
    /* Bounded: a writer filling slots with bad report IDs as fast as they
     * are skipped must not hold the lock forever. */
    for (int pass = 0; pass < HIDSHIM_KBD_SLOTS; pass++) {
        ULONG head = (ULONG)ReadAcquire((volatile LONG *)&shared->Head);
        UCHAR slot[sizeof(HIDSHIM_KBD_SLOT)];
        ULONG length;
        if (head == ctx->Tail) return FALSE;
        if (head - ctx->Tail > HIDSHIM_KBD_SLOTS) {
            /* A broken writer: skip what it claims to have written. */
            ctx->Tail = head;
            PublishTailLocked(ctx);
            return FALSE;
        }
        for (int i = 0; i < sizeof(slot); i++) {
            slot[i] = shared->Slots[ctx->Tail % HIDSHIM_KBD_SLOTS].Report[i];
        }
        ctx->Tail++;
        PublishTailLocked(ctx);
        length = ReportLength(slot[0]);
        if (length != 0) {
            RtlCopyMemory(ctx->Held, slot, length);
            ctx->HeldLength = length;
            return TRUE;
        }
    }
    return FALSE;
}

/* Caller holds ctx->Lock. One report per read, the next only after hidclass
 * has sent a read again: reports given to two parked reads back to back are
 * handled on different cores and may swap. */
static VOID PumpLocked(HID_CTX *ctx)
{
    for (;;) {
        WDFREQUEST request;
        WDFMEMORY memory;
        size_t size;
        NTSTATUS status;

        if (!ctx->Armed || !ctx->Settled) break;
        if (!TakeReportLocked(ctx)) {
            if (ctx->Mapping == NULL) break;
            /* Waiting goes up before the last look at Head: either that look
             * sees the owner's report or the owner sees Waiting. */
            InterlockedExchange((volatile LONG *)&ctx->Mapping->View->Waiting, 1);
            if (!TakeReportLocked(ctx)) return;
        }
        if (!NT_SUCCESS(WdfIoQueueRetrieveNextRequest(ctx->ReadQueue, &request))) break;
        status = WdfRequestRetrieveOutputMemory(request, &memory);
        if (NT_SUCCESS(status)) {
            WdfMemoryGetBuffer(memory, &size);
            status = size < ctx->HeldLength ? STATUS_INVALID_BUFFER_SIZE
                : WdfMemoryCopyFromBuffer(memory, 0, ctx->Held, ctx->HeldLength);
        }
        if (NT_SUCCESS(status)) {
            ULONG length = ctx->HeldLength;
            ctx->HeldLength = 0;
            ctx->Armed = FALSE;
            WdfRequestCompleteWithInformation(request, STATUS_SUCCESS, length);
        } else {
            WdfRequestComplete(request, status);
        }
    }
    if (ctx->Mapping != NULL) InterlockedExchange((volatile LONG *)&ctx->Mapping->View->Waiting, 0);
}

static VOID CALLBACK SettleFired(PTP_CALLBACK_INSTANCE Instance, PVOID Context, PTP_TIMER Timer)
{
    HID_CTX *ctx = Context;
    UNREFERENCED_PARAMETER(Instance);
    UNREFERENCED_PARAMETER(Timer);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    ctx->Settled = TRUE;
    ctx->Armed = TRUE;
    PumpLocked(ctx);
    WdfWaitLockRelease(ctx->Lock);
}

static VOID CALLBACK MappingWoken(
    PTP_CALLBACK_INSTANCE Instance, PVOID Context, PTP_WAIT Wait, TP_WAIT_RESULT WaitResult)
{
    MAPPING *mapping = Context;
    HID_CTX *ctx = mapping->Ctx;
    UNREFERENCED_PARAMETER(Instance);
    UNREFERENCED_PARAMETER(WaitResult);

    WdfWaitLockAcquire(ctx->Lock, NULL);
    /* Unmapped meanwhile: DestroyMapping is waiting for this callback. */
    if (ctx->Mapping == mapping) {
        PumpLocked(ctx);
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
    if (mapping->View != NULL) UnmapViewOfFile((LPCVOID)mapping->View);
    if (mapping->Section != NULL) CloseHandle(mapping->Section);
    if (mapping->Event != NULL) CloseHandle(mapping->Event);
    HeapFree(GetProcessHeap(), 0, mapping);
}

/* Takes ownership of both handles. */
static MAPPING *CreateMapping(HID_CTX *ctx, HANDLE section, HANDLE event)
{
    MAPPING *mapping = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*mapping));
    if (mapping == NULL) {
        CloseHandle(section);
        CloseHandle(event);
        return NULL;
    }
    mapping->Ctx = ctx;
    mapping->Section = section;
    mapping->Event = event;
    mapping->View = MapViewOfFile(section, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, sizeof(HIDSHIM_KBD_SHARED));
    mapping->Wait = CreateThreadpoolWait(MappingWoken, mapping, NULL);
    if (mapping->View == NULL || mapping->Wait == NULL) {
        DestroyMapping(mapping);
        return NULL;
    }
    return mapping;
}

/* Caller holds ctx->Lock; destroys the returned mapping after releasing it.
 * Unread reports go, and the neutral set goes first, so nothing the owner
 * held stays pressed. */
static MAPPING *ReleaseLocked(HID_CTX *ctx)
{
    MAPPING *old = ctx->Mapping;
    ctx->Mapping = NULL;
    ctx->HeldLength = 0;
    ctx->NeutralLeft = NEUTRAL_REPORTS;
    return old;
}

/* Nothing is scheduled once Stopping is set: cleanup stops the timers and
 * waits for the work after that, and a late arm would outlive them. */
static VOID ScheduleConnect(HID_CTX *ctx, DWORD delayMs)
{
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (!ctx->Stopping) {
        if (delayMs == 0) {
            SubmitThreadpoolWork(ctx->ConnectWork);
        } else {
            StartTimer(ctx->RetryTimer, delayMs);
        }
    }
    WdfWaitLockRelease(ctx->Lock);
}

static VOID CALLBACK RetryFired(PTP_CALLBACK_INSTANCE Instance, PVOID Context, PTP_TIMER Timer)
{
    UNREFERENCED_PARAMETER(Instance);
    UNREFERENCED_PARAMETER(Timer);
    ScheduleConnect(Context, 0);
}

/* Caller holds ctx->Lock. While Sending is non-zero, ConnectRun does not
 * delete the target or the ATTACH request. */
static VOID EnterSendLocked(HID_CTX *ctx)
{
    if (ctx->Sending++ == 0) ResetEvent(ctx->SendIdle);
}

/* Caller holds ctx->Lock. */
static VOID LeaveSendLocked(HID_CTX *ctx)
{
    if (--ctx->Sending == 0) SetEvent(ctx->SendIdle);
}

/* Caller holds ctx->Lock. The target is captured here: ConnectRun clears
 * ctx->Target before it waits for the send to finish. */
static BOOLEAN BeginSendLocked(HID_CTX *ctx, WDFIOTARGET *target)
{
    if (ctx->Target == NULL || ctx->TargetDead || ctx->Stopping) return FALSE;
    EnterSendLocked(ctx);
    *target = ctx->Target;
    return TRUE;
}

static NTSTATUS SendAttach(HID_CTX *ctx, WDFIOTARGET target)
{
    WDF_REQUEST_REUSE_PARAMS reuse;
    NTSTATUS status;
    WDF_REQUEST_REUSE_PARAMS_INIT(&reuse, WDF_REQUEST_REUSE_NO_FLAGS, STATUS_SUCCESS);
    status = WdfRequestReuse(ctx->Attach, &reuse);
    if (!NT_SUCCESS(status)) return status;
    *(ULONG *)WdfMemoryGetBuffer(ctx->AttachIn, NULL) = ctx->Generation;
    status = WdfIoTargetFormatRequestForIoctl(
        target, ctx->Attach, IOCTL_HIDSHIM_KBD_ATTACH, ctx->AttachIn, NULL, ctx->AttachOut, NULL);
    if (!NT_SUCCESS(status)) return status;
    WdfRequestSetCompletionRoutine(ctx->Attach, AttachCompleted, ctx);
    if (!WdfRequestSend(ctx->Attach, target, WDF_NO_SEND_OPTIONS)) {
        return WdfRequestGetStatus(ctx->Attach);
    }
    return STATUS_SUCCESS;
}

/* After BeginSendLocked, without ctx->Lock held: the request may complete
 * inline, and its completion takes the lock. */
static VOID SendAttachOrDrop(HID_CTX *ctx, WDFIOTARGET target)
{
    NTSTATUS status = SendAttach(ctx, target);
    MAPPING *released = NULL;
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (!NT_SUCCESS(status)) {
        released = ReleaseLocked(ctx);
        PumpLocked(ctx);
        ctx->TargetDead = TRUE;
    }
    LeaveSendLocked(ctx);
    WdfWaitLockRelease(ctx->Lock);
    DestroyMapping(released);
    if (!NT_SUCCESS(status)) ScheduleConnect(ctx, RETRY_MS / 5);
}

/* The control device's handles in this process, for a reply not applied. */
static VOID CloseReplyHandles(const HIDSHIM_KBD_ATTACH *reply)
{
    if (!(reply->Flags & HIDSHIM_KBD_OWNED)) return;
    if (reply->Section != 0) CloseHandle((HANDLE)(ULONG_PTR)reply->Section);
    if (reply->Event != 0) CloseHandle((HANDLE)(ULONG_PTR)reply->Event);
}

/* Caller holds ctx->Lock. Returns the mapping to destroy. */
static MAPPING *ApplyAttachLocked(HID_CTX *ctx, const HIDSHIM_KBD_ATTACH *reply)
{
    MAPPING *old = NULL;
    if (reply->Generation == ctx->Generation) {
        CloseReplyHandles(reply);
        return NULL;
    }
    ctx->Generation = reply->Generation;
    old = ReleaseLocked(ctx);
    ctx->Tail = 0;
    if (reply->Flags & HIDSHIM_KBD_OWNED) {
        ctx->Mapping = CreateMapping(
            ctx, (HANDLE)(ULONG_PTR)reply->Section, (HANDLE)(ULONG_PTR)reply->Event);
        if (ctx->Mapping != NULL) SetThreadpoolWait(ctx->Mapping->Wait, ctx->Mapping->Event, NULL);
    }
    if (reply->Flags & HIDSHIM_KBD_REMOVING) ctx->FailDevice = TRUE;
    PumpLocked(ctx);
    return old;
}

VOID AttachCompleted(
    _In_ WDFREQUEST Request, _In_ WDFIOTARGET Target, _In_ PWDF_REQUEST_COMPLETION_PARAMS Params, _In_ WDFCONTEXT Context)
{
    HID_CTX *ctx = Context;
    NTSTATUS status = Params->IoStatus.Status;
    /* Only a full reply is trusted to hold handles; a short one is ignored
     * whole, handle values included. */
    BOOLEAN full = NT_SUCCESS(status) && Params->IoStatus.Information == sizeof(HIDSHIM_KBD_ATTACH);
    HIDSHIM_KBD_ATTACH reply;
    MAPPING *old = NULL;
    BOOLEAN again = FALSE, fail = FALSE;
    WDFIOTARGET next = NULL;
    UNREFERENCED_PARAMETER(Request);
    UNREFERENCED_PARAMETER(Target);

    RtlZeroMemory(&reply, sizeof(reply));
    if (full) reply = *(const HIDSHIM_KBD_ATTACH *)WdfMemoryGetBuffer(ctx->AttachOut, NULL);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    EnterSendLocked(ctx);
    if (full && !ctx->Stopping) {
        old = ApplyAttachLocked(ctx, &reply);
        fail = ctx->FailDevice;
        again = !fail && BeginSendLocked(ctx, &next);
    } else {
        if (full) CloseReplyHandles(&reply);
        /* The control device is gone or failed us: as good as a release. */
        old = ReleaseLocked(ctx);
        PumpLocked(ctx);
        ctx->TargetDead = TRUE;
    }
    LeaveSendLocked(ctx);
    WdfWaitLockRelease(ctx->Lock);
    DestroyMapping(old);
    if (fail) {
        WdfDeviceSetFailed(ctx->Device, WdfDeviceFailedNoRestart);
    } else if (again) {
        SendAttachOrDrop(ctx, next);
    } else {
        /* The target is closed and deleted outside its own completion. */
        ScheduleConnect(ctx, RETRY_MS / 5);
    }
}

/* Finds the path of the control device's hid interface. */
static BOOLEAN FindControl(PWSTR path, size_t capacity)
{
    ULONG size = 0;
    PWSTR list, entry;
    BOOLEAN found = FALSE;
    static const WCHAR suffix[] = L"\\" HIDSHIM_KBD_HID_REFERENCE;
    const size_t suffixLen = ARRAYSIZE(suffix) - 1;

    if (CM_Get_Device_Interface_List_SizeW(&size, (LPGUID)&CONTROL_GUID, NULL,
                                           CM_GET_DEVICE_INTERFACE_LIST_PRESENT) != CR_SUCCESS || size <= 1) {
        return FALSE;
    }
    list = HeapAlloc(GetProcessHeap(), 0, size * sizeof(WCHAR));
    if (list == NULL) return FALSE;
    if (CM_Get_Device_Interface_ListW((LPGUID)&CONTROL_GUID, NULL, list, size,
                                      CM_GET_DEVICE_INTERFACE_LIST_PRESENT) == CR_SUCCESS) {
        for (entry = list; *entry != L'\0'; entry += wcslen(entry) + 1) {
            size_t len = wcslen(entry);
            if (len < capacity && len > suffixLen
                && CompareStringOrdinal(entry + len - suffixLen, (int)suffixLen, suffix, (int)suffixLen, TRUE)
                    == CSTR_EQUAL) {
                wcscpy_s(path, capacity, entry);
                found = TRUE;
                break;
            }
        }
    }
    HeapFree(GetProcessHeap(), 0, list);
    return found;
}

static NTSTATUS OpenTarget(HID_CTX *ctx, PCWSTR path, WDFIOTARGET *target)
{
    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_IO_TARGET_OPEN_PARAMS params;
    UNICODE_STRING name;
    NTSTATUS status;

    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    attrs.ParentObject = ctx->Device;
    status = WdfIoTargetCreate(ctx->Device, &attrs, target);
    if (!NT_SUCCESS(status)) return status;
    RtlInitUnicodeString(&name, path);
    WDF_IO_TARGET_OPEN_PARAMS_INIT_OPEN_BY_NAME(&params, &name, GENERIC_READ | GENERIC_WRITE);
    status = WdfIoTargetOpen(*target, &params);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(*target);
        *target = NULL;
        return status;
    }

    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    attrs.ParentObject = *target;
    status = WdfRequestCreate(&attrs, *target, &ctx->Attach);
    if (NT_SUCCESS(status)) {
        WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
        attrs.ParentObject = ctx->Attach;
        status = WdfMemoryCreate(&attrs, NonPagedPool, 0, sizeof(ULONG), &ctx->AttachIn, NULL);
    }
    if (NT_SUCCESS(status)) {
        status = WdfMemoryCreate(&attrs, NonPagedPool, 0, sizeof(HIDSHIM_KBD_ATTACH), &ctx->AttachOut, NULL);
    }
    if (!NT_SUCCESS(status)) {
        WdfIoTargetClose(*target);
        WdfObjectDelete(*target);
        *target = NULL;
        ctx->Attach = NULL;
    }
    return status;
}

/* Deletes a dead target and, while there is none, looks for the control
 * device and attaches to it. Serialized by ConnectLock. */
static VOID CALLBACK ConnectRun(PTP_CALLBACK_INSTANCE Instance, PVOID Context, PTP_WORK Work)
{
    HID_CTX *ctx = Context;
    WDFIOTARGET dead = NULL, target = NULL;
    WCHAR path[512];
    BOOLEAN connect, send;
    UNREFERENCED_PARAMETER(Instance);
    UNREFERENCED_PARAMETER(Work);

    WdfWaitLockAcquire(ctx->ConnectLock, NULL);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->TargetDead || ctx->Stopping) {
        dead = ctx->Target;
        ctx->Target = NULL;
        ctx->TargetDead = FALSE;
    }
    connect = ctx->Target == NULL && !ctx->Stopping;
    WdfWaitLockRelease(ctx->Lock);
    if (dead != NULL) {
        /* Close cancels the pending ATTACH; a completion or send still
         * running holds Sending until it is done with the request. */
        WdfIoTargetClose(dead);
        WaitForSingleObject(ctx->SendIdle, INFINITE);
        WdfObjectDelete(dead);
        ctx->Attach = NULL;
    }

    if (connect && FindControl(path, ARRAYSIZE(path))) {
        if (NT_SUCCESS(OpenTarget(ctx, path, &target))) {
            WdfWaitLockAcquire(ctx->Lock, NULL);
            ctx->Target = target;
            /* No generation the control device can have: the first answer
             * comes at once, whatever it held before. */
            ctx->Generation = 0xFFFFFFFF;
            send = BeginSendLocked(ctx, &target);
            if (!send) ctx->Target = NULL;
            WdfWaitLockRelease(ctx->Lock);
            if (send) {
                SendAttachOrDrop(ctx, target);
            } else {
                WdfIoTargetClose(target);
                WdfObjectDelete(target);
                ctx->Attach = NULL;
            }
        } else {
            /* The interface is there but would not open, say mid-removal:
             * look again later rather than wait for an arrival that already
             * happened. */
            ScheduleConnect(ctx, RETRY_MS);
        }
    }
    WdfWaitLockRelease(ctx->ConnectLock);
}

static DWORD CALLBACK ControlArrived(
    HCMNOTIFICATION Notify, PVOID Context, CM_NOTIFY_ACTION Action, PCM_NOTIFY_EVENT_DATA EventData, DWORD EventDataSize)
{
    UNREFERENCED_PARAMETER(Notify);
    UNREFERENCED_PARAMETER(EventData);
    UNREFERENCED_PARAMETER(EventDataSize);
    if (Action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL) ScheduleConnect(Context, 0);
    return ERROR_SUCCESS;
}

NTSTATUS HidSelfManagedIoInit(_In_ WDFDEVICE Device)
{
    HID_CTX *ctx = GetHidCtx(Device);
    CM_NOTIFY_FILTER filter;
    RtlZeroMemory(&filter, sizeof(filter));
    filter.cbSize = sizeof(filter);
    filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
    filter.u.DeviceInterface.ClassGuid = CONTROL_GUID;
    if (CM_Register_Notification(&filter, ctx, ControlArrived, &ctx->Notification) != CR_SUCCESS) {
        ctx->Notification = NULL;
    }
    ScheduleConnect(ctx, 0);
    return STATUS_SUCCESS;
}

VOID HidSelfManagedIoCleanup(_In_ WDFDEVICE Device)
{
    HID_CTX *ctx = GetHidCtx(Device);
    MAPPING *old;

    WdfWaitLockAcquire(ctx->Lock, NULL);
    ctx->Stopping = TRUE;
    WdfWaitLockRelease(ctx->Lock);
    /* From here nothing arms a timer or submits the work. */
    if (ctx->Notification != NULL) CM_Unregister_Notification(ctx->Notification);
    WaitForThreadpoolWorkCallbacks(ctx->ConnectWork, FALSE);
    /* One more pass closes the target; its pending ATTACH completes
     * cancelled and schedules nothing. */
    ConnectRun(NULL, ctx, NULL);
    StopTimer(ctx->RetryTimer);
    StopTimer(ctx->SettleTimer);
    WaitForThreadpoolWorkCallbacks(ctx->ConnectWork, FALSE);

    WdfWaitLockAcquire(ctx->Lock, NULL);
    old = ctx->Mapping;
    ctx->Mapping = NULL;
    WdfWaitLockRelease(ctx->Lock);
    DestroyMapping(old);
    CloseThreadpoolTimer(ctx->RetryTimer);
    CloseThreadpoolTimer(ctx->SettleTimer);
    CloseThreadpoolWork(ctx->ConnectWork);
    CloseHandle(ctx->SendIdle);
}

/* hidclass sends its first reads again after the device powers up. */
NTSTATUS HidD0Entry(_In_ WDFDEVICE Device, _In_ WDF_POWER_DEVICE_STATE PreviousState)
{
    HID_CTX *ctx = GetHidCtx(Device);
    UNREFERENCED_PARAMETER(PreviousState);
    WdfWaitLockAcquire(ctx->Lock, NULL);
    ctx->Settled = FALSE;
    ctx->SettleStarted = FALSE;
    ctx->Armed = FALSE;
    WdfWaitLockRelease(ctx->Lock);
    return STATUS_SUCCESS;
}

static VOID HandleReadReport(HID_CTX *ctx, WDFREQUEST Request)
{
    NTSTATUS status;
    WdfWaitLockAcquire(ctx->Lock, NULL);
    status = WdfRequestForwardToIoQueue(Request, ctx->ReadQueue);
    if (NT_SUCCESS(status)) {
        if (ctx->Settled) {
            ctx->Armed = TRUE;
            PumpLocked(ctx);
        } else if (!ctx->SettleStarted && !ctx->Stopping) {
            ctx->SettleStarted = TRUE;
            StartTimer(ctx->SettleTimer, SETTLE_MS);
        }
    }
    WdfWaitLockRelease(ctx->Lock);
    if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
}

static NTSTATUS CopyOut(WDFREQUEST Request, const VOID *data, size_t length)
{
    WDFMEMORY memory;
    size_t size;
    NTSTATUS status = WdfRequestRetrieveOutputMemory(Request, &memory);
    if (!NT_SUCCESS(status)) return status;
    WdfMemoryGetBuffer(memory, &size);
    if (size < length) return STATUS_INVALID_BUFFER_SIZE;
    status = WdfMemoryCopyFromBuffer(memory, 0, (PVOID)data, length);
    if (NT_SUCCESS(status)) WdfRequestSetInformation(Request, length);
    return status;
}

static NTSTATUS GetString(WDFREQUEST Request)
{
    PVOID in;
    ULONG id = 0;
    if (NT_SUCCESS(WdfRequestRetrieveInputBuffer(Request, sizeof(ULONG), &in, NULL))) {
        id = *(ULONG *)in & 0xFFFF;
    }
    switch (id) {
    case HID_STRING_ID_IMANUFACTURER: return CopyOut(Request, Manufacturer, sizeof(Manufacturer));
    case HID_STRING_ID_ISERIALNUMBER: return CopyOut(Request, Serial, sizeof(Serial));
    default: return CopyOut(Request, Product, sizeof(Product));
    }
}

VOID HidIoControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode)
{
    HID_CTX *ctx = GetHidCtx(WdfIoQueueGetDevice(Queue));
    NTSTATUS status;
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    switch (IoControlCode) {
    case IOCTL_HID_GET_DEVICE_DESCRIPTOR:
        status = CopyOut(Request, &HidDescriptor, sizeof(HidDescriptor));
        break;
    case IOCTL_HID_GET_REPORT_DESCRIPTOR:
        status = CopyOut(Request, ReportDescriptor, sizeof(ReportDescriptor));
        break;
    case IOCTL_HID_GET_DEVICE_ATTRIBUTES:
        status = CopyOut(Request, &Attributes, sizeof(Attributes));
        break;
    case IOCTL_HID_GET_STRING:
        status = GetString(Request);
        break;
    case IOCTL_HID_READ_REPORT:
        HandleReadReport(ctx, Request);
        return;
    case IOCTL_HID_ACTIVATE_DEVICE:
    case IOCTL_HID_DEACTIVATE_DEVICE:
    case IOCTL_HID_SEND_IDLE_NOTIFICATION_REQUEST:
        status = STATUS_SUCCESS;
        break;
    default:
        /* No output or feature reports: nothing is written to the device. */
        status = STATUS_NOT_SUPPORTED;
        break;
    }
    WdfRequestComplete(Request, status);
}
