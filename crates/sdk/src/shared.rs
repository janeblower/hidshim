//! Reports through a section the driver reads, without a call into it.

use crate::device::{self, Device};
use crate::gamepad::State;
use crate::proto::{IOCTL_GET_VERSION, IOCTL_HOLD, IOCTL_MAP_REPORT, Mapping, Shared, Version};
use std::io;
use std::marker::PhantomData;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{Ordering, fence};
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, GetLastError, HANDLE, STATUS_CANCELLED, STATUS_PENDING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Memory::{
    FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::SetEvent;

/// State of the IOCTL_HIDSHIM_HOLD request.
pub(crate) enum Hold {
    Pending,
    /// By the device going away, or by Windows when the thread that sent
    /// the request exits.
    Cancelled,
    Ended,
}

/// The pad's section, which holds the last report.
pub(crate) type SharedReport = Section<Shared>;

/// A section of type `T` mapped from the driver, with the request that keeps
/// the claim alive.
pub(crate) struct Section<T> {
    view: *mut T,
    wake: OwnedHandle,
    /// IOCTL_HIDSHIM_HOLD, pending until the pad is lost. Boxed and freed by
    /// hand: the kernel writes it while this value moves around.
    hold: *mut OVERLAPPED,
    /// The event of hold, closed only after it.
    _hold_done: OwnedHandle,
    /// Borrowed from the owner of the claim, which drops this value before its
    /// Device.
    device: HANDLE,
    _marker: PhantomData<T>,
}

impl<T> std::fmt::Debug for Section<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Section").finish_non_exhaustive()
    }
}

// SAFETY: the view and the OVERLAPPED belong to this value alone; the handles
// work from any thread. Every method takes &mut self.
unsafe impl<T> Send for Section<T> {}
unsafe impl<T> Sync for Section<T> {}

impl<T> Section<T> {
    pub(crate) fn new(device: &mut Device) -> io::Result<Self> {
        let mapping = device.query(IOCTL_MAP_REPORT)?;
        Self::with_mapping(device, mapping)
    }

    /// Takes over the handles of an IOCTL_HIDSHIM_MAP_REPORT reply.
    pub(crate) fn with_mapping(device: &mut Device, mapping: Mapping) -> io::Result<Self> {
        let hold_done = device::event()?;
        // SAFETY: the driver put these handles into this process for us alone.
        let (section, wake) = unsafe {
            (
                OwnedHandle::from_raw_handle(mapping.section as HANDLE),
                OwnedHandle::from_raw_handle(mapping.event as HANDLE),
            )
        };
        // SAFETY: a section handle with write access; the view outlives it.
        let view = unsafe {
            MapViewOfFile(
                section.as_raw_handle(),
                FILE_MAP_WRITE,
                0,
                0,
                size_of::<T>(),
            )
        };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut shared = Self {
            view: view.Value.cast(),
            wake,
            hold: Box::into_raw(Box::new(device::overlapped(&hold_done))),
            _hold_done: hold_done,
            device: device.raw(),
            _marker: PhantomData,
        };
        if !shared.send_hold() {
            return Err(io::Error::other(
                "the hidshim driver did not hold the claim",
            ));
        }
        Ok(shared)
    }

    /// Sends IOCTL_HIDSHIM_HOLD; true while it stays pending. Only once the
    /// previous one has completed, so hold is free to reuse.
    pub(crate) fn send_hold(&mut self) -> bool {
        // SAFETY: hold is not in use; resetting it keeps its event.
        unsafe {
            let event = (*self.hold).hEvent;
            *self.hold = std::mem::zeroed();
            (*self.hold).hEvent = event;
        }
        // SAFETY: hold stays allocated until Drop has waited for the request.
        let sent = unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_HOLD,
                null(),
                0,
                null_mut(),
                0,
                null_mut(),
                self.hold,
            )
        };
        // SAFETY: plain thread-local read.
        sent == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING
    }

    pub(crate) fn hold(&mut self) -> Hold {
        // SAFETY: hold is live; the kernel updates Internal when the request
        // completes, so it is read as volatile.
        let status = unsafe { std::ptr::read_volatile(&raw const (*self.hold).Internal) };
        match status as i32 {
            STATUS_PENDING => Hold::Pending,
            STATUS_CANCELLED => Hold::Cancelled,
            _ => Hold::Ended,
        }
    }

    /// The mapped section.
    pub(crate) fn view(&self) -> *mut T {
        self.view
    }

    /// Wakes the driver, which waits for a report.
    pub(crate) fn wake(&self) {
        // SAFETY: an event handle with EVENT_MODIFY_STATE. A failure only
        // delays the report until the next poll.
        unsafe { SetEvent(self.wake.as_raw_handle()) };
    }

    /// Whether the claim is alive, renewing the hold when Windows cancelled
    /// it. False once the device is gone for good.
    pub(crate) fn alive(&mut self, device: &mut Device) -> bool {
        match self.hold() {
            Hold::Pending => true,
            // A device that is gone also fails a plain request.
            Hold::Cancelled => {
                device.query::<Version>(IOCTL_GET_VERSION).is_ok() && self.send_hold()
            }
            Hold::Ended => false,
        }
    }
}

impl Section<Shared> {
    pub(crate) fn write(&mut self, state: &State) {
        // SAFETY: view maps a whole Shared for as long as self lives. Only
        // the atomics are borrowed; the report is written through a pointer.
        let (sequence, waiting) = unsafe { (&(*self.view).sequence, &(*self.view).waiting) };
        let odd = sequence.load(Ordering::Relaxed).wrapping_add(1);
        sequence.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: as above; this process is the section's only writer.
        unsafe { std::ptr::write_volatile(&raw mut (*self.view).report, *state) };
        sequence.store(odd.wrapping_add(1), Ordering::Release);
        // Pairs with the driver's barrier after it sets waiting.
        fence(Ordering::SeqCst);
        if waiting.load(Ordering::Relaxed) != 0 {
            self.wake();
        }
    }
}

impl<T> Drop for Section<T> {
    fn drop(&mut self) {
        let mut returned = 0;
        // SAFETY: device is still open (see the field); hold is the request
        // sent on it and is freed only after the wait shows it complete.
        unsafe {
            CancelIoEx(self.device, self.hold);
            GetOverlappedResult(self.device, self.hold, &mut returned, 1);
            drop(Box::from_raw(self.hold));
        }
        // SAFETY: view came from MapViewOfFile and is not used after this.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            })
        };
    }
}
