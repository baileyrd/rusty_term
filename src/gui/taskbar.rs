//! Windows taskbar progress (G01 stretch): mirrors the active tab's OSC 9;4
//! progress state onto the window's taskbar button via `ITaskbarList3`.
//! windows-sys 0.59 doesn't generate this interface (absent even with the
//! `Win32_UI_Shell` feature enabled), so the handful of vtable slots this
//! needs are hand-rolled here — the interface itself is a stable, widely
//! documented part of the Win32 shell ABI, unchanged since Windows 7. No
//! Unix equivalent: there's no cross-desktop taskbar-progress protocol to
//! target.
#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoInitializeEx, COINIT_APARTMENTTHREADED,
};
use windows_sys::Win32::UI::Shell::{
    TaskbarList, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED, TBPFLAG,
};
use windows_sys::core::{GUID, HRESULT};

/// `IID_ITaskbarList3`: `{EA1AFB91-9E28-4B86-90E9-9E9F8A5EEFAF}`.
const IID_TASKBAR_LIST3: GUID = GUID::from_u128(0xea1afb91_9e28_4b86_90e9_9e9f8a5eefaf);

/// The `ITaskbarList3` vtable, truncated to the slots this module calls.
/// Field order matches the real interface (`IUnknown` + `ITaskbarList` +
/// `ITaskbarList2` + the first two `ITaskbarList3` methods) — later
/// `ITaskbarList3` methods (`RegisterTab`, thumbnail buttons, …) are never
/// called, so their slots don't need representing.
#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    hr_init: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    add_tab: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    delete_tab: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    activate_tab: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    set_active_alt: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    mark_fullscreen_window: unsafe extern "system" fn(*mut c_void, HWND, i32) -> HRESULT,
    set_progress_value: unsafe extern "system" fn(*mut c_void, HWND, u64, u64) -> HRESULT,
    set_progress_state: unsafe extern "system" fn(*mut c_void, HWND, TBPFLAG) -> HRESULT,
}

#[repr(C)]
struct RawInterface {
    vtbl: *const Vtbl,
}

/// An owned `ITaskbarList3` reference; `Drop` releases it like any COM
/// interface pointer.
struct TaskbarList3(*mut RawInterface);

impl TaskbarList3 {
    fn hr_init(&self) -> HRESULT {
        // SAFETY: `self.0` is a live `ITaskbarList3` for the lifetime of `self`.
        unsafe { ((*(*self.0).vtbl).hr_init)(self.0.cast()) }
    }

    fn set_progress_value(&self, hwnd: HWND, completed: u64, total: u64) -> HRESULT {
        // SAFETY: same as above; `hwnd` is a valid window handle owned by
        // the caller for the duration of this call.
        unsafe { ((*(*self.0).vtbl).set_progress_value)(self.0.cast(), hwnd, completed, total) }
    }

    fn set_progress_state(&self, hwnd: HWND, flags: TBPFLAG) -> HRESULT {
        // SAFETY: same as `set_progress_value`.
        unsafe { ((*(*self.0).vtbl).set_progress_state)(self.0.cast(), hwnd, flags) }
    }
}

impl Drop for TaskbarList3 {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live COM reference this instance owns.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0.cast());
        }
    }
}

thread_local! {
    // Apartment-threaded COM object: must stay on the thread that created
    // it, which is exactly what `thread_local!` gives us — the GUI event
    // loop runs entirely on one thread.
    static TASKBAR: RefCell<Option<TaskbarList3>> = const { RefCell::new(None) };
}

fn with_taskbar<R>(f: impl FnOnce(&TaskbarList3) -> R) -> Option<R> {
    TASKBAR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = create();
        }
        slot.as_ref().map(f)
    })
}

fn create() -> Option<TaskbarList3> {
    // SAFETY: `CoInitializeEx`/`CoCreateInstance` are plain COM bootstrap
    // calls; `raw` starts null and is only written to a valid interface
    // pointer by `CoCreateInstance` on success (checked via `hr`).
    unsafe {
        // RPC_E_CHANGED_MODE (already init'd in a different mode) and
        // S_FALSE (already init'd the same way) are both fine to ignore —
        // COM is usable on this thread afterward either way.
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &TaskbarList,
            std::ptr::null_mut(),
            CLSCTX_INPROC_SERVER,
            &IID_TASKBAR_LIST3,
            &mut raw,
        );
        if hr < 0 || raw.is_null() {
            return None;
        }
        let tb = TaskbarList3(raw.cast());
        if tb.hr_init() < 0 {
            return None;
        }
        Some(tb)
    }
}

/// Mirror `progress` (OSC 9;4's `(state, percent)`; `state` 0 = none, 1 =
/// normal, 2 = error, 3 = indeterminate, 4 = paused) onto `hwnd`'s taskbar
/// button. Cheap to call every frame like `Window::set_title` — a no-op
/// once taskbar integration is known unavailable.
pub(crate) fn sync(hwnd: HWND, progress: Option<(u8, u8)>) {
    with_taskbar(|tb| match progress {
        None | Some((0, _)) => {
            tb.set_progress_state(hwnd, TBPF_NOPROGRESS);
        }
        Some((state, percent)) => {
            let flag = match state {
                2 => TBPF_ERROR,
                3 => TBPF_INDETERMINATE,
                4 => TBPF_PAUSED,
                _ => TBPF_NORMAL,
            };
            tb.set_progress_state(hwnd, flag);
            if flag != TBPF_INDETERMINATE {
                tb.set_progress_value(hwnd, percent.min(100) as u64, 100);
            }
        }
    });
}
