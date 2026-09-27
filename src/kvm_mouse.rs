// Mouse capture is independent of the desktop cursor. In REMOTE mode the
// low-level hook suppresses local input; WM_INPUT supplies device movement.
use super::*;
use std::cell::RefCell;

const WM_INPUT: u32 = 0x00FF;
const RID_INPUT: u32 = 0x10000003;
const RIDEV_INPUTSINK: u32 = 0x100;
const RIDEV_REMOVE: u32 = 1;
const MOUSE_MOVE_ABSOLUTE: u16 = 1;
const MOUSE_VIRTUAL_DESKTOP: u16 = 2;

#[repr(C)]
struct RawInputDevice { usage_page: u16, usage: u16, flags: u32, target: HWND }
#[repr(C)]
struct RawInputHeader { kind: u32, size: u32, device: HANDLE, wparam: WPARAM }
#[repr(C)]
#[derive(Default)]
struct RawMouse {
    flags: u16,
    buttons: u32, // Union: low word = button flags, high word = button data.
    raw_buttons: u32,
    x: i32,
    y: i32,
    extra: u32,
}
#[repr(C)]
struct RawMouseInput { header: RawInputHeader, mouse: RawMouse }

unsafe extern "system" {
    fn RegisterRawInputDevices(devices: *const RawInputDevice, count: u32, size: u32) -> BOOL;
    fn GetRawInputData(input: HANDLE, command: u32, data: *mut c_void, size: *mut u32, header_size: u32) -> u32;
}

#[derive(Default)]
struct MouseMotion {
    // Keep separate baselines for absolute devices (e.g. remote desktop/tablets).
    absolute: HashMap<usize, (i32, i32, i32, i32, bool)>,
}
impl MouseMotion {
    fn delta(&mut self, device: usize, mouse: &RawMouse, width: i32, height: i32) -> (i32, i32) {
        if mouse.flags & MOUSE_MOVE_ABSOLUTE == 0 {
            self.absolute.remove(&device);
            return (mouse.x, mouse.y);
        }
        let x = (mouse.x.clamp(0, 65535) as i64 * (width - 1).max(0) as i64 / 65535) as i32;
        let y = (mouse.y.clamp(0, 65535) as i64 * (height - 1).max(0) as i64 / 65535) as i32;
        let virtual_desktop = mouse.flags & MOUSE_VIRTUAL_DESKTOP != 0;
        match self.absolute.insert(device, (x, y, width, height, virtual_desktop)) {
            Some((px, py, pw, ph, pv)) if (pw, ph, pv) == (width, height, virtual_desktop) => (x - px, y - py),
            _ => (0, 0), // Seed/reset the baseline without a cursor jump.
        }
    }
}

#[derive(Default)]
struct CaptureState {
    target: Option<(u64, bool)>,
    motion: MouseMotion,
}
impl CaptureState {
    fn set_target(&mut self, target: Option<(u64, bool)>) {
        if self.target != target {
            self.motion.absolute.clear();
            self.target = target;
        }
    }
}
thread_local! { static CAPTURE: RefCell<CaptureState> = RefCell::new(CaptureState::default()); }

pub(super) fn reset_capture() {
    // Called on the hook/window thread when the hotkey changes target.
    CAPTURE.with(|state| *state.borrow_mut() = CaptureState::default());
}

fn mouse_payload(msg: u32, dx: i32, dy: i32, data: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity(16);
    p.extend_from_slice(&msg.to_le_bytes());
    p.extend_from_slice(&dx.to_le_bytes());
    p.extend_from_slice(&dy.to_le_bytes());
    p.extend_from_slice(&data.to_le_bytes());
    p
}

fn mouse_events(mouse: &RawMouse, delta: (i32, i32), mut emit: impl FnMut(Vec<u8>)) {
    if delta != (0, 0) { emit(mouse_payload(WM_MOUSEMOVE, delta.0, delta.1, 0)); }
    // Preserve movement before button transitions from the same device packet.
    for (bit, msg, data) in [
        (0x001, WM_LBUTTONDOWN, 0), (0x002, WM_LBUTTONUP, 0),
        (0x004, WM_RBUTTONDOWN, 0), (0x008, WM_RBUTTONUP, 0),
        (0x010, WM_MBUTTONDOWN, 0), (0x020, WM_MBUTTONUP, 0),
        (0x040, WM_XBUTTONDOWN, 1), (0x080, WM_XBUTTONUP, 1),
        (0x100, WM_XBUTTONDOWN, 2), (0x200, WM_XBUTTONUP, 2),
    ] {
        if mouse.buttons & bit != 0 { emit(mouse_payload(msg, 0, 0, data)); }
    }
    let wheel = (mouse.buttons >> 16) as u16 as i16 as i32 as u32;
    if mouse.buttons & 0x400 != 0 { emit(mouse_payload(WM_MOUSEWHEEL, 0, 0, wheel)); }
    if mouse.buttons & 0x800 != 0 { emit(mouse_payload(WM_MOUSEHWHEEL, 0, 0, wheel)); }
}

fn capture_mouse(input: &RawMouseInput) {
    // SendInput normally produces no raw packet; also reject our tag if a
    // device/driver preserves it, so peer input can never be sent back.
    if input.mouse.extra == KVM_TAG as u32 { return; }
    let slot = hook_slot().lock().ok().and_then(|g| g.clone());
    let target = slot.as_ref().map(|s| (s.session,
        s.remote.load(Ordering::Acquire) && s.connected.load(Ordering::Acquire)));
    CAPTURE.with(|state| {
        let mut state = state.borrow_mut();
        state.set_target(target);
        let Some(s) = slot.filter(|_| target.is_some_and(|(_, remote)| remote)) else { return; };
        KVM_RAW_MOUSE_COUNT.fetch_add(1, Ordering::Relaxed);
        let virtual_desktop = input.mouse.flags & MOUSE_VIRTUAL_DESKTOP != 0;
        let (width, height) = if input.mouse.flags & MOUSE_MOVE_ABSOLUTE != 0 {
            unsafe { (GetSystemMetrics(if virtual_desktop { SM_CXVIRTUALSCREEN } else { 0 }),
                      GetSystemMetrics(if virtual_desktop { SM_CYVIRTUALSCREEN } else { 1 })) }
        } else { (0, 0) };
        let delta = state.motion.delta(input.header.device as usize, &input.mouse, width, height);
        mouse_events(&input.mouse, delta, |payload| {
            if s.tx.send(CtrlFrame::new(CTRL_KVM_MOUSE, s.session, 0, 0, 0, payload)).is_ok() {
                KVM_MOUSE_TX_COUNT.fetch_add(1, Ordering::Relaxed);
            }
        });
    });
}

unsafe extern "system" fn raw_mouse_wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        let mut input: RawMouseInput = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<RawMouseInput>() as u32;
        let read = unsafe { GetRawInputData(l as HANDLE, RID_INPUT, (&mut input as *mut RawMouseInput).cast(),
            &mut size, std::mem::size_of::<RawInputHeader>() as u32) };
        if read == u32::MAX {
            KVM_RAW_READ_ERROR.store(unsafe { GetLastError() } as u64 + 1, Ordering::Relaxed);
        } else if read >= std::mem::size_of::<RawMouseInput>() as u32 && input.header.kind == 0 {
            capture_mouse(&input);
        }
        // DefWindowProc performs required foreground WM_INPUT cleanup.
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

pub(super) unsafe fn create_capture_window() -> Result<HWND, u32> {
    let name = wide_null("OTI_Link_KVM_Raw_Mouse");
    let instance = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSW { style: 0, lpfnWndProc: Some(raw_mouse_wnd_proc), cbClsExtra: 0, cbWndExtra: 0,
        hInstance: instance, hIcon: null_mut(), hCursor: null_mut(), hbrBackground: null_mut(),
        lpszMenuName: null(), lpszClassName: name.as_ptr() };
    if unsafe { RegisterClassW(&wc) } == 0 && unsafe { GetLastError() } != 1410 {
        return Err(unsafe { GetLastError() });
    }
    let hwnd = unsafe { CreateWindowExW(0, name.as_ptr(), name.as_ptr(), 0, 0, 0, 0, 0,
        -3isize as HWND, null_mut(), instance, null_mut()) };
    if hwnd.is_null() { return Err(unsafe { GetLastError() }); }
    let device = RawInputDevice { usage_page: 1, usage: 2, flags: RIDEV_INPUTSINK, target: hwnd };
    if unsafe { RegisterRawInputDevices(&device, 1, std::mem::size_of::<RawInputDevice>() as u32) } == 0 {
        let error = unsafe { GetLastError() };
        unsafe { DestroyWindow(hwnd); }
        return Err(error);
    }
    Ok(hwnd)
}

pub(super) unsafe fn destroy_capture_window(hwnd: HWND) {
    let device = RawInputDevice { usage_page: 1, usage: 2, flags: RIDEV_REMOVE, target: null_mut() };
    unsafe {
        RegisterRawInputDevices(&device, 1, std::mem::size_of::<RawInputDevice>() as u32);
        DestroyWindow(hwnd);
    }
    reset_capture();
}

#[cfg(test)]
#[path = "kvm_mouse_tests.rs"]
mod tests;
