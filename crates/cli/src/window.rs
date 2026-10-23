//! A plain top-level window. Windows.Gaming.Input delivers readings only to
//! the process that owns the foreground window, which a console app does not.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::w;

pub struct Window(HWND);

impl Window {
    pub fn open() -> Option<Self> {
        // SAFETY: plain window creation with static strings and the default
        // window procedure.
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(DefWindowProcW),
                hInstance: instance,
                lpszClassName: w!("hidshim-watch"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST,
                w!("hidshim-watch"),
                w!("hidshim watch: keep focused"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                360,
                120,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            SetForegroundWindow(hwnd);
            Some(Self(hwnd))
        }
    }

    pub fn pump(&self) {
        // SAFETY: standard message loop step for this thread's windows.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn focused(&self) -> bool {
        // SAFETY: reads the current foreground window.
        unsafe { GetForegroundWindow() == self.0 }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the window was created by this thread.
        unsafe { DestroyWindow(self.0) };
    }
}
