/*
 * hidshim keyboard control device: a UMDF2 driver on its own root devnode
 * that ties the keyboard's owner to the HID minidriver.
 *
 * The top of the keyboard's stack is hidclass, which passes no private IOCTL
 * down and never shows the minidriver the owner's handle closing. So the
 * owner claims this device instead, and the minidriver is itself a client
 * here: it keeps an ATTACH pending and gets the claim's section through it.
 * Reports never pass through this driver.
 */

#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#include <wdf.h>
#include <psapi.h>
#include <sddl.h>

#include "hidshimkbd.h"

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD CtlDeviceAdd;
EVT_WDF_DEVICE_FILE_CREATE CtlFileCreate;
EVT_WDF_FILE_CLEANUP CtlFileCleanup;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL CtlIoControl;

static const GUID CONTROL_GUID = HIDSHIM_KBD_CONTROL_GUID_INIT;

typedef enum { OPEN_CONTROL = 1, OPEN_ADMIN, OPEN_HID } OPEN_KIND;

typedef struct {
    WDFWAITLOCK Lock;
    /* Everything below is guarded by Lock. */
    WDFFILEOBJECT Owner;
    ULONG OwnerProcessId;
    /* The claim's section and event, NULL until MAP_REPORT. */
    HANDLE Section;
    HANDLE Event;
    /* The claim outlived the HID minidriver's attachment and is dead: the
     * owner has to open again. */
    BOOLEAN Broken;
    WDFFILEOBJECT Hid;
    ULONG HidProcessId;
    HANDLE HidProcess;
    ULONG Generation;
    BOOLEAN Removing;
    WDFQUEUE HoldQueue;
    WDFQUEUE AttachQueue;
} CTL_CTX;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(CTL_CTX, GetCtlCtx)

typedef struct {
    OPEN_KIND Kind;
} FILE_CTX;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(FILE_CTX, GetFileCtx)

NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, CtlDeviceAdd);
    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}

NTSTATUS CtlDeviceAdd(_In_ WDFDRIVER Driver, _Inout_ PWDFDEVICE_INIT DeviceInit)
{
    static const PCWSTR references[] = {
        HIDSHIM_KBD_CONTROL_REFERENCE, HIDSHIM_KBD_ADMIN_REFERENCE, HIDSHIM_KBD_HID_REFERENCE
    };
    NTSTATUS status;
    WDFDEVICE device;
    CTL_CTX *ctx;
    UNREFERENCED_PARAMETER(Driver);

    WDF_FILEOBJECT_CONFIG fileConfig;
    WDF_FILEOBJECT_CONFIG_INIT(&fileConfig, CtlFileCreate, WDF_NO_EVENT_CALLBACK, CtlFileCleanup);
    WDF_OBJECT_ATTRIBUTES fileAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttrs, FILE_CTX);
    WdfDeviceInitSetFileObjectConfig(DeviceInit, &fileConfig, &fileAttrs);

    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attrs, CTL_CTX);
    status = WdfDeviceCreate(&DeviceInit, &attrs, &device);
    if (!NT_SUCCESS(status)) return status;
    ctx = GetCtlCtx(device);

    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    attrs.ParentObject = device;
    status = WdfWaitLockCreate(&attrs, &ctx->Lock);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG queueConfig;
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoDeviceControl = CtlIoControl;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    if (!NT_SUCCESS(status)) return status;

    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchManual);
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &ctx->HoldQueue);
    if (!NT_SUCCESS(status)) return status;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &ctx->AttachQueue);
    if (!NT_SUCCESS(status)) return status;

    for (int i = 0; i < ARRAYSIZE(references); i++) {
        UNICODE_STRING reference;
        RtlInitUnicodeString(&reference, references[i]);
        status = WdfDeviceCreateDeviceInterface(device, &CONTROL_GUID, &reference);
        if (!NT_SUCCESS(status)) return status;
    }
    return STATUS_SUCCESS;
}

static BOOLEAN NameIs(PCUNICODE_STRING name, PCWSTR reference)
{
    UNICODE_STRING tail = *name;
    size_t length = wcslen(reference);
    if (tail.Length >= sizeof(WCHAR) && tail.Buffer[0] == L'\\') {
        tail.Buffer++;
        tail.Length -= sizeof(WCHAR);
    }
    return tail.Length == length * sizeof(WCHAR)
        && CompareStringOrdinal(tail.Buffer, (int)length, reference, (int)length, TRUE) == CSTR_EQUAL;
}

static OPEN_KIND OpenKind(WDFFILEOBJECT FileObject)
{
    PUNICODE_STRING name = WdfFileObjectGetFileName(FileObject);
    if (name == NULL || name->Length == 0) return 0;
    if (NameIs(name, HIDSHIM_KBD_CONTROL_REFERENCE)) return OPEN_CONTROL;
    if (NameIs(name, HIDSHIM_KBD_ADMIN_REFERENCE)) return OPEN_ADMIN;
    if (NameIs(name, HIDSHIM_KBD_HID_REFERENCE)) return OPEN_HID;
    return 0;
}

static BOOLEAN TokenHas(WELL_KNOWN_SID_TYPE type)
{
    BYTE sid[SECURITY_MAX_SID_SIZE];
    DWORD size = sizeof(sid);
    BOOL member = FALSE;
    return CreateWellKnownSid(type, NULL, sid, &size)
        && CheckTokenMembership(NULL, sid, &member) && member;
}

/* Runs impersonating the requestor. CheckTokenMembership ignores deny-only
 * groups, so a filtered UAC token is not an administrator here. */
static VOID CheckAdmin(_In_ WDFREQUEST Request, _In_opt_ PVOID Context)
{
    UNREFERENCED_PARAMETER(Request);
    *(BOOLEAN *)Context = TokenHas(WinBuiltinAdministratorsSid) || TokenHas(WinLocalSystemSid);
}

static BOOLEAN IsAdmin(WDFREQUEST Request)
{
    BOOLEAN admin = FALSE;
    return NT_SUCCESS(WdfRequestImpersonate(Request, SecurityImpersonation, CheckAdmin, &admin)) && admin;
}

static BOOLEAN EndsWithI(PCWSTR text, size_t textLen, PCWSTR suffix)
{
    size_t len = wcslen(suffix);
    return textLen >= len
        && CompareStringOrdinal(text + textLen - len, (int)len, suffix, (int)len, TRUE) == CSTR_EQUAL;
}

static BOOLEAN StartsWithI(PCWSTR text, size_t textLen, PCWSTR prefix, size_t len)
{
    return textLen >= len && CompareStringOrdinal(text, (int)len, prefix, (int)len, TRUE) == CSTR_EQUAL;
}

/*
 * USER MODE DRIVERS is in every WUDFHost's token, not only the keyboard's.
 * There is no API for the host of a devnode, so the requestor must be
 * WUDFHost.exe from System32 with hidshimhid.dll loaded from the driver
 * store; the keyboard's host is not shared, so that is the keyboard's host.
 */
static BOOLEAN IsKeyboardHost(HANDLE process)
{
    WCHAR system[MAX_PATH], path[MAX_PATH];
    UINT systemLen = GetSystemDirectoryW(system, MAX_PATH - 32);
    DWORD pathLen = MAX_PATH;
    HMODULE modules[256];
    DWORD needed = 0;
    WCHAR store[MAX_PATH];
    size_t storeLen;

    if (systemLen == 0 || systemLen >= MAX_PATH - 32) return FALSE;
    if (!QueryFullProcessImageNameW(process, 0, path, &pathLen)) return FALSE;
    if (pathLen != systemLen + wcslen(L"\\WUDFHost.exe")
        || !StartsWithI(path, pathLen, system, systemLen)
        || !EndsWithI(path, pathLen, L"\\WUDFHost.exe")) {
        return FALSE;
    }

    RtlCopyMemory(store, system, systemLen * sizeof(WCHAR));
    wcscpy_s(store + systemLen, MAX_PATH - systemLen, L"\\DriverStore\\");
    storeLen = wcslen(store);
    if (!K32EnumProcessModulesEx(process, modules, sizeof(modules), &needed, LIST_MODULES_ALL)) return FALSE;
    if (needed > sizeof(modules)) needed = sizeof(modules);
    for (DWORD i = 0; i < needed / sizeof(HMODULE); i++) {
        DWORD len = K32GetModuleFileNameExW(process, modules[i], path, MAX_PATH);
        if (len == 0 || len >= MAX_PATH) continue;
        if (StartsWithI(path, len, store, storeLen) && EndsWithI(path, len, L"\\hidshimhid.dll")) return TRUE;
    }
    return FALSE;
}

typedef struct {
    ULONG ProcessId;
    BOOLEAN Allowed;
    HANDLE Process;
} OPEN_HOST;

/* Runs impersonating the HID minidriver's open, so its process is opened
 * with the rights of its own token, not this host's. */
static VOID OpenHost(_In_ WDFREQUEST Request, _In_opt_ PVOID Context)
{
    OPEN_HOST *host = Context;
    HANDLE query;
    UNREFERENCED_PARAMETER(Request);
    if (!TokenHas(WinUserModeDriversSid)) return;
    query = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, FALSE, host->ProcessId);
    if (query == NULL) return;
    host->Allowed = IsKeyboardHost(query);
    CloseHandle(query);
    if (host->Allowed) host->Process = OpenProcess(PROCESS_DUP_HANDLE, FALSE, host->ProcessId);
}

/* Caller holds ctx->Lock. */
static VOID CloseClaimLocked(CTL_CTX *ctx)
{
    if (ctx->Section != NULL) CloseHandle(ctx->Section);
    if (ctx->Event != NULL) CloseHandle(ctx->Event);
    ctx->Section = NULL;
    ctx->Event = NULL;
}

/* Caller holds ctx->Lock. */
static VOID CompleteHoldsLocked(CTL_CTX *ctx, NTSTATUS status)
{
    WDFREQUEST request;
    while (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(ctx->HoldQueue, &request))) {
        WdfRequestComplete(request, status);
    }
}

/* Caller holds ctx->Lock. A failed duplicate leaves the claim without the
 * HID side, which breaks it like a lost attachment. */
static VOID AnswerAttachLocked(CTL_CTX *ctx, WDFREQUEST Request)
{
    HIDSHIM_KBD_ATTACH *reply;
    NTSTATUS status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*reply), (PVOID *)&reply, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    RtlZeroMemory(reply, sizeof(*reply));
    reply->Generation = ctx->Generation;
    if (ctx->Removing) reply->Flags |= HIDSHIM_KBD_REMOVING;
    if (ctx->Section != NULL && !ctx->Broken && !ctx->Removing) {
        HANDLE section = NULL, event = NULL;
        HANDLE self = GetCurrentProcess();
        if (DuplicateHandle(self, ctx->Section, ctx->HidProcess, &section,
                            SECTION_MAP_READ | SECTION_MAP_WRITE, FALSE, 0)
            && DuplicateHandle(self, ctx->Event, ctx->HidProcess, &event, SYNCHRONIZE, FALSE, 0)) {
            reply->Flags |= HIDSHIM_KBD_OWNED;
            reply->Section = (ULONG64)(ULONG_PTR)section;
            reply->Event = (ULONG64)(ULONG_PTR)event;
        } else {
            if (section != NULL) {
                DuplicateHandle(ctx->HidProcess, section, NULL, NULL, 0, FALSE, DUPLICATE_CLOSE_SOURCE);
            }
            ctx->Broken = TRUE;
            CloseClaimLocked(ctx);
            CompleteHoldsLocked(ctx, STATUS_DEVICE_NOT_CONNECTED);
        }
    }
    WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, sizeof(*reply));
}

/* Caller holds ctx->Lock. The generation is state, not an event: the HID
 * side compares it with the one it saw, so nothing is lost between two
 * ATTACH requests. */
static VOID BumpGenerationLocked(CTL_CTX *ctx)
{
    WDFREQUEST request;
    ctx->Generation++;
    while (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(ctx->AttachQueue, &request))) {
        AnswerAttachLocked(ctx, request);
    }
}

static NTSTATUS CreateHid(CTL_CTX *ctx, WDFREQUEST Request, WDFFILEOBJECT FileObject)
{
    OPEN_HOST host = { WdfRequestGetRequestorProcessId(Request), FALSE, NULL };
    NTSTATUS status = WdfRequestImpersonate(Request, SecurityImpersonation, OpenHost, &host);
    if (!NT_SUCCESS(status)) return status;
    if (!host.Allowed) return STATUS_ACCESS_DENIED;
    if (host.Process == NULL) return STATUS_INSUFFICIENT_RESOURCES;

    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->Hid != NULL) {
        status = STATUS_DEVICE_BUSY;
    } else {
        ctx->Hid = FileObject;
        ctx->HidProcessId = host.ProcessId;
        ctx->HidProcess = host.Process;
        host.Process = NULL;
    }
    WdfWaitLockRelease(ctx->Lock);
    if (host.Process != NULL) CloseHandle(host.Process);
    return status;
}

VOID CtlFileCreate(_In_ WDFDEVICE Device, _In_ WDFREQUEST Request, _In_ WDFFILEOBJECT FileObject)
{
    CTL_CTX *ctx = GetCtlCtx(Device);
    FILE_CTX *file = GetFileCtx(FileObject);
    NTSTATUS status = STATUS_SUCCESS;

    file->Kind = OpenKind(FileObject);
    switch (file->Kind) {
    case OPEN_HID:
        status = CreateHid(ctx, Request, FileObject);
        break;
    case OPEN_ADMIN:
        if (!IsAdmin(Request)) status = STATUS_ACCESS_DENIED;
        break;
    case OPEN_CONTROL:
        if (!IsAdmin(Request)) {
            status = STATUS_ACCESS_DENIED;
            break;
        }
        WdfWaitLockAcquire(ctx->Lock, NULL);
        if (ctx->Removing) {
            status = STATUS_DELETE_PENDING;
        } else if (ctx->Owner != NULL) {
            status = STATUS_DEVICE_BUSY;
        } else {
            ctx->Owner = FileObject;
            ctx->OwnerProcessId = WdfRequestGetRequestorProcessId(Request);
            ctx->Broken = FALSE;
        }
        WdfWaitLockRelease(ctx->Lock);
        break;
    default:
        status = STATUS_OBJECT_NAME_NOT_FOUND;
        break;
    }
    /* A failed create gets no cleanup, so the kind must not claim anything. */
    if (!NT_SUCCESS(status)) file->Kind = 0;
    WdfRequestComplete(Request, status);
}

VOID CtlFileCleanup(_In_ WDFFILEOBJECT FileObject)
{
    CTL_CTX *ctx = GetCtlCtx(WdfFileObjectGetDevice(FileObject));
    OPEN_KIND kind = GetFileCtx(FileObject)->Kind;
    HANDLE process = NULL;
    WDFREQUEST request;

    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (kind == OPEN_CONTROL && ctx->Owner == FileObject) {
        ctx->Owner = NULL;
        ctx->OwnerProcessId = 0;
        ctx->Broken = FALSE;
        CloseClaimLocked(ctx);
        CompleteHoldsLocked(ctx, STATUS_SUCCESS);
        BumpGenerationLocked(ctx);
    } else if (kind == OPEN_HID && ctx->Hid == FileObject) {
        while (NT_SUCCESS(WdfIoQueueRetrieveRequestByFileObject(ctx->AttachQueue, FileObject, &request))) {
            WdfRequestComplete(request, STATUS_CANCELLED);
        }
        process = ctx->HidProcess;
        ctx->Hid = NULL;
        ctx->HidProcess = NULL;
        ctx->HidProcessId = 0;
        if (ctx->Owner != NULL) {
            ctx->Broken = TRUE;
            CloseClaimLocked(ctx);
            CompleteHoldsLocked(ctx, STATUS_DEVICE_NOT_CONNECTED);
        }
        ctx->Generation++;
    }
    WdfWaitLockRelease(ctx->Lock);
    if (process != NULL) CloseHandle(process);
}

static VOID CompleteWithData(WDFREQUEST Request, const VOID *data, size_t len)
{
    PVOID out;
    NTSTATUS status = WdfRequestRetrieveOutputBuffer(Request, len, &out, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    RtlCopyMemory(out, data, len);
    WdfRequestCompleteWithInformation(Request, STATUS_SUCCESS, len);
}

static BOOLEAN IsOwnerLocked(CTL_CTX *ctx, WDFREQUEST Request)
{
    return ctx->Owner != NULL && ctx->Owner == WdfRequestGetFileObject(Request);
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

/* A fresh section per claim: a former owner that still has its view writes
 * into a section the HID side no longer reads. Backed by the paging file so a
 * reader cannot fault on it; its DACL lets no one open it by other means. */
static NTSTATUS CreateClaim(HANDLE *section, HANDLE *event)
{
    SECURITY_ATTRIBUTES sa = { sizeof(sa), NULL, FALSE };
    NTSTATUS status = STATUS_INSUFFICIENT_RESOURCES;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(
            L"D:P(A;;GA;;;SY)", SDDL_REVISION_1, &sa.lpSecurityDescriptor, NULL)) {
        return status;
    }
    *section = CreateFileMappingW(INVALID_HANDLE_VALUE, &sa, PAGE_READWRITE, 0, sizeof(HIDSHIM_KBD_SHARED), NULL);
    *event = CreateEventW(&sa, FALSE, FALSE, NULL);
    LocalFree(sa.lpSecurityDescriptor);
    if (*section != NULL && *event != NULL) return STATUS_SUCCESS;
    if (*section != NULL) CloseHandle(*section);
    if (*event != NULL) CloseHandle(*event);
    *section = *event = NULL;
    return status;
}

static NTSTATUS GiveClaim(WDFREQUEST Request, HANDLE section, HANDLE event, HIDSHIM_KBD_MAPPING *given)
{
    HANDLE ownerSection, ownerEvent;
    OPEN_OWNER owner = { WdfRequestGetRequestorProcessId(Request), NULL };
    NTSTATUS status = WdfRequestImpersonate(Request, SecurityImpersonation, OpenOwner, &owner);
    if (!NT_SUCCESS(status)) return status;
    if (owner.Process == NULL) return STATUS_ACCESS_DENIED;
    status = STATUS_INSUFFICIENT_RESOURCES;
    if (DuplicateHandle(GetCurrentProcess(), section, owner.Process, &ownerSection,
                        FILE_MAP_READ | FILE_MAP_WRITE, FALSE, 0)) {
        /* The owner may only set the event, not wait on it or reset it. */
        if (DuplicateHandle(GetCurrentProcess(), event, owner.Process, &ownerEvent,
                            EVENT_MODIFY_STATE, FALSE, 0)) {
            given->Section = (ULONG64)(ULONG_PTR)ownerSection;
            given->Event = (ULONG64)(ULONG_PTR)ownerEvent;
            status = STATUS_SUCCESS;
        } else {
            DuplicateHandle(owner.Process, ownerSection, NULL, NULL, 0, FALSE, DUPLICATE_CLOSE_SOURCE);
        }
    }
    CloseHandle(owner.Process);
    return status;
}

/* Caller holds ctx->Lock. */
static NTSTATUS CanMapLocked(CTL_CTX *ctx, WDFREQUEST Request)
{
    if (!IsOwnerLocked(ctx, Request)) return STATUS_ACCESS_DENIED;
    if (ctx->Broken || ctx->Hid == NULL) return STATUS_DEVICE_NOT_CONNECTED;
    if (ctx->Section != NULL) return STATUS_DEVICE_BUSY;
    return STATUS_SUCCESS;
}

static VOID HandleMapReport(CTL_CTX *ctx, WDFREQUEST Request)
{
    HIDSHIM_KBD_MAPPING *given;
    HANDLE section = NULL, event = NULL;
    NTSTATUS status = WdfRequestRetrieveOutputBuffer(Request, sizeof(*given), (PVOID *)&given, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_BUFFER_TOO_SMALL);
        return;
    }
    WdfWaitLockAcquire(ctx->Lock, NULL);
    status = CanMapLocked(ctx, Request);
    WdfWaitLockRelease(ctx->Lock);
    if (NT_SUCCESS(status)) status = CreateClaim(&section, &event);
    if (NT_SUCCESS(status)) status = GiveClaim(Request, section, event, given);
    if (NT_SUCCESS(status)) {
        WdfWaitLockAcquire(ctx->Lock, NULL);
        status = CanMapLocked(ctx, Request);
        if (NT_SUCCESS(status)) {
            ctx->Section = section;
            ctx->Event = event;
            section = event = NULL;
            BumpGenerationLocked(ctx);
        }
        WdfWaitLockRelease(ctx->Lock);
    }
    /* Handles already given to an owner that lost the race stay with it,
     * pointing at a section no one reads. */
    if (section != NULL) CloseHandle(section);
    if (event != NULL) CloseHandle(event);
    if (NT_SUCCESS(status)) {
        WdfRequestCompleteWithInformation(Request, status, sizeof(*given));
    } else {
        WdfRequestComplete(Request, status);
    }
}

static VOID HandleHold(CTL_CTX *ctx, WDFREQUEST Request)
{
    NTSTATUS status;
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (!IsOwnerLocked(ctx, Request)) {
        status = STATUS_ACCESS_DENIED;
    } else if (ctx->Broken || ctx->Hid == NULL) {
        status = STATUS_DEVICE_NOT_CONNECTED;
    } else {
        status = WdfRequestForwardToIoQueue(Request, ctx->HoldQueue);
    }
    WdfWaitLockRelease(ctx->Lock);
    if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
}

static VOID HandleAttach(CTL_CTX *ctx, WDFREQUEST Request)
{
    ULONG *seen;
    NTSTATUS status = WdfRequestRetrieveInputBuffer(Request, sizeof(*seen), (PVOID *)&seen, NULL);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Request, STATUS_INVALID_PARAMETER);
        return;
    }
    WdfWaitLockAcquire(ctx->Lock, NULL);
    if (ctx->Hid == NULL || ctx->Hid != WdfRequestGetFileObject(Request)) {
        status = STATUS_ACCESS_DENIED;
    } else if (*seen != ctx->Generation) {
        AnswerAttachLocked(ctx, Request);
    } else {
        status = WdfRequestForwardToIoQueue(Request, ctx->AttachQueue);
    }
    WdfWaitLockRelease(ctx->Lock);
    if (!NT_SUCCESS(status)) WdfRequestComplete(Request, status);
}

/* Uninstall. The HID side hears of it through ATTACH and fails its device;
 * then this device fails too. Failed devices are removed as unplugged, so
 * open handles cannot veto the removal. */
static VOID HandleRemove(CTL_CTX *ctx, WDFDEVICE Device, WDFREQUEST Request)
{
    if (!IsAdmin(Request)) {
        WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
        return;
    }
    WdfWaitLockAcquire(ctx->Lock, NULL);
    ctx->Removing = TRUE;
    CloseClaimLocked(ctx);
    CompleteHoldsLocked(ctx, STATUS_DEVICE_REMOVED);
    BumpGenerationLocked(ctx);
    WdfWaitLockRelease(ctx->Lock);
    WdfRequestComplete(Request, STATUS_SUCCESS);
    WdfDeviceSetFailed(Device, WdfDeviceFailedNoRestart);
}

VOID CtlIoControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode)
{
    WDFDEVICE device = WdfIoQueueGetDevice(Queue);
    CTL_CTX *ctx = GetCtlCtx(device);
    WDFFILEOBJECT fileObject = WdfRequestGetFileObject(Request);
    OPEN_KIND kind = fileObject != NULL ? GetFileCtx(fileObject)->Kind : 0;
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    if (kind == 0) {
        WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
        return;
    }

    switch (IoControlCode) {
    case IOCTL_HIDSHIM_KBD_GET_VERSION: {
        HIDSHIM_KBD_VERSION version = { HIDSHIM_KBD_PROTOCOL_VERSION, 0 };
        CompleteWithData(Request, &version, sizeof(version));
        break;
    }

    case IOCTL_HIDSHIM_KBD_GET_INFO: {
        HIDSHIM_KBD_INFO info;
        if (kind != OPEN_ADMIN) {
            WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
            break;
        }
        WdfWaitLockAcquire(ctx->Lock, NULL);
        info.Owned = ctx->Owner != NULL;
        info.OwnerProcessId = ctx->OwnerProcessId;
        info.HidConnected = ctx->Hid != NULL;
        info.HidProcessId = ctx->HidProcessId;
        WdfWaitLockRelease(ctx->Lock);
        CompleteWithData(Request, &info, sizeof(info));
        break;
    }

    case IOCTL_HIDSHIM_KBD_MAP_REPORT:
        HandleMapReport(ctx, Request);
        break;

    case IOCTL_HIDSHIM_KBD_HOLD:
        HandleHold(ctx, Request);
        break;

    case IOCTL_HIDSHIM_KBD_ATTACH:
        HandleAttach(ctx, Request);
        break;

    case IOCTL_HIDSHIM_KBD_REMOVE:
        if (kind != OPEN_ADMIN) {
            WdfRequestComplete(Request, STATUS_ACCESS_DENIED);
            break;
        }
        HandleRemove(ctx, device, Request);
        break;

    default:
        WdfRequestComplete(Request, STATUS_INVALID_DEVICE_REQUEST);
        break;
    }
}
