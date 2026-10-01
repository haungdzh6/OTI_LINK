use super::*;

const CROSS_TIMEOUT: Duration = Duration::from_millis(900);
const CROSS_COOLDOWN: Duration = Duration::from_millis(200);
const CROSS_PUSH_PX: i32 = 8;
const CROSS_INSET_PX: i32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Display {
    pub rect: RECT,
    pub width_mm: u32,
    pub height_mm: u32,
}

impl Display {
    fn width(self) -> i32 { self.rect.right - self.rect.left }
    fn height(self) -> i32 { self.rect.bottom - self.rect.top }
    fn valid(self) -> bool {
        self.width() > 0 && self.height() > 0 && self.width_mm > 0 && self.height_mm > 0
    }
    fn encode(self) -> Vec<u8> {
        let mut p = Vec::with_capacity(25);
        for v in [self.rect.left, self.rect.top, self.rect.right, self.rect.bottom] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(&self.width_mm.to_le_bytes());
        p.extend_from_slice(&self.height_mm.to_le_bytes());
        p.push(1); // FIX10 primary-display capability
        p
    }
    fn decode(p: &[u8]) -> Option<Self> {
        if p.len() != 25 || p[24] != 1 { return None; }
        let i = |at| Some(i32::from_le_bytes(p[at..at + 4].try_into().ok()?));
        let u = |at| Some(u32::from_le_bytes(p[at..at + 4].try_into().ok()?));
        let d = Self { rect: RECT { left: i(0)?, top: i(4)?, right: i(8)?, bottom: i(12)? },
            width_mm: u(16)?, height_mm: u(20)? };
        d.valid().then_some(d)
    }
}

#[repr(C)]
struct MonitorInfo { size: u32, rect: RECT, work: RECT, flags: u32 }
#[repr(C)]
struct MonitorInfoEx { size: u32, rect: RECT, work: RECT, flags: u32, device: [u16; 32] }
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplayMonitors(dc: HANDLE, clip: *const RECT,
        proc: Option<unsafe extern "system" fn(HANDLE, HANDLE, *mut RECT, isize) -> BOOL>,
        data: isize) -> BOOL;
    fn GetMonitorInfoW(monitor: HANDLE, info: *mut c_void) -> BOOL;
    fn GetPhysicalCursorPos(point: *mut POINT) -> BOOL;
    fn SetPhysicalCursorPos(x: i32, y: i32) -> BOOL;
    fn MonitorFromPoint(point: POINT, flags: u32) -> HANDLE;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateDCW(driver: *const u16, device: *const u16, output: *const u16,
        init: *const c_void) -> HANDLE;
    fn GetDeviceCaps(dc: HANDLE, index: i32) -> i32;
    fn DeleteDC(dc: HANDLE) -> BOOL;
}

unsafe extern "system" fn monitor_cb(monitor: HANDLE, _: HANDLE, _: *mut RECT, data: LPARAM) -> BOOL {
    let out = unsafe { &mut *(data as *mut Vec<(RECT, bool, u32, u32)>) };
    let mut info = MonitorInfoEx { size: std::mem::size_of::<MonitorInfoEx>() as u32,
        rect: RECT::default(), work: RECT::default(), flags: 0, device: [0; 32] };
    if unsafe { GetMonitorInfoW(monitor, (&mut info as *mut MonitorInfoEx).cast::<c_void>()) } != 0 {
        let dc = unsafe { CreateDCW(null(), info.device.as_ptr(), null(), null()) };
        let (w, h) = if dc.is_null() { (0, 0) } else {
            let w = unsafe { GetDeviceCaps(dc, 4) };
            let h = unsafe { GetDeviceCaps(dc, 6) };
            unsafe { DeleteDC(dc); }
            (w, h)
        };
        out.push((info.rect, info.flags & 1 != 0, w.max(0) as u32, h.max(0) as u32));
    }
    1
}

static DISPLAY_CACHE: OnceLock<Mutex<Option<(Instant, u32, Option<Display>)>>> = OnceLock::new();
pub(super) fn invalidate_display() {
    if let Some(cache) = DISPLAY_CACHE.get() { if let Ok(mut c) = cache.lock() { *c = None; } }
}
pub(super) fn local_display() -> Option<Display> {
    let chosen = CROSS_DIAGONAL_TENTHS.load(Ordering::Acquire);
    let cache = DISPLAY_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(c) = cache.lock() {
        if let Some((at, old_chosen, result)) = *c {
            if old_chosen == chosen && at.elapsed() < Duration::from_secs(2) { return result; }
        }
    }
    let result = scan_display(chosen);
    if let Ok(mut c) = cache.lock() { *c = Some((Instant::now(), chosen, result)); }
    result
}
fn scan_display(chosen: u32) -> Option<Display> {
    let mut monitors = Vec::<(RECT, bool, u32, u32)>::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(monitor_cb), &mut monitors as *mut _ as isize); }
    let (rect, _, auto_w, auto_h) = monitors.into_iter().find(|(_, primary, _, _)| *primary)?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 { return None; }
    if chosen == 0 && (100..=3000).contains(&auto_w) && (100..=2000).contains(&auto_h) {
        let aspect = auto_w as f64 / auto_h as f64;
        if (aspect / (width as f64 / height as f64) - 1.0).abs() < 0.15 {
            return Some(Display { rect, width_mm: auto_w, height_mm: auto_h });
        }
    }
    let diag = (if chosen == 0 { 240 } else { chosen.clamp(100, 800) }) as f64 / 10.0 * 25.4;
    let ratio = width as f64 / height as f64;
    let h = (diag / (ratio * ratio + 1.0).sqrt()).round() as u32;
    let w = (h as f64 * ratio).round() as u32;
    Some(Display { rect, width_mm: w.max(1), height_mm: h.max(1) })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase { Local, Entering(u64, Instant), Remote(u64), Returning(u64, Instant) }
struct State {
    phase: Phase,
    peer: Option<Display>,
    reported_local: Option<Display>,
    incoming: Option<(u64, u8)>,
    incoming_since: Instant,
    incoming_active: bool,
    return_pending: bool,
    push: i32,
    last_switch: Instant,
    next_id: u64,
}
impl Default for State {
    fn default() -> Self { Self { phase: Phase::Local, peer: None, reported_local: None, incoming: None,
        incoming_since: Instant::now(), incoming_active: false,
        return_pending: false, push: 0, last_switch: Instant::now() - CROSS_COOLDOWN,
        next_id: 0 } }
}
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
fn state() -> &'static Mutex<State> { STATE.get_or_init(|| Mutex::new(State::default())) }

pub(super) fn reset() { if let Ok(mut s) = state().lock() { *s = State::default(); } }
pub(super) fn cancel() {
    if let Ok(mut s) = state().lock() {
        if let Phase::Entering(id, _) = s.phase {
            if let Some(slot) = hook_slot().lock().ok().and_then(|g| g.clone()) {
                let _ = slot.tx.send(CtrlFrame::new(CTRL_CROSS_ABORT, slot.session, 0, id, 0, vec![]));
            }
        }
        s.phase = Phase::Local; s.push = 0; s.incoming = None; s.incoming_active = false; s.return_pending = false;
    }
}
pub(super) fn abort(id: u64) {
    if let Ok(mut s) = state().lock() {
        if matches!(s.phase, Phase::Remote(active) | Phase::Entering(active, _) if active == id) {
            kvm_set_target(false); s.phase = Phase::Local; s.last_switch = Instant::now();
           
        }
        if s.incoming.is_some_and(|(incoming, _)| incoming == id) {
            s.incoming = None; s.incoming_active = false; s.return_pending = false; s.push = 0;
            inject_reset();
        }
    }
}
pub(super) fn injection_failed(session: u64, tx: &mpsc::Sender<CtrlFrame>) {
    if let Ok(mut s) = state().lock() {
        if let Some((id, _)) = s.incoming.take() {
            s.incoming_active = false; s.return_pending = false; s.push = 0;
            inject_reset();
            let _ = tx.send(CtrlFrame::new(CTRL_CROSS_ABORT, session, 0, id, 0, vec![]));
        }
    }
}
pub(super) fn peer_info(p: &[u8]) {
    let new_info = Display::decode(p);
    if let Ok(mut s) = state().lock() {
        if s.peer == new_info { return; }
        s.peer = new_info;
        if s.phase != Phase::Local { kvm_set_target(false); }
        s.phase = Phase::Local; s.push = 0;
    }
}
pub(super) fn peer_target(remote: bool) {
    if let Ok(mut s) = state().lock() {
        if remote && s.incoming.is_some() { s.incoming_active = true; s.last_switch = Instant::now(); }
        if !remote { s.incoming = None; s.incoming_active = false; s.return_pending = false; }
    }
}
pub(super) fn display_frame(session: u64) -> Option<CtrlFrame> {
    Some(CtrlFrame::new(CTRL_DISPLAY_INFO, session, 0, 0, 0, local_display()?.encode()))
}
pub(super) fn peer_display() -> Option<Display> { state().lock().ok().and_then(|s| s.peer) }

// The OLE edge target occupies only the physical overlap of the two screens.
pub(super) fn local_motion(dx: i32, dy: i32, slot: &HookState) {
    if !workflow_cross_screen_enabled() || !slot.connected.load(Ordering::Acquire)
        || slot.remote.load(Ordering::Acquire) || dx == 0 && dy == 0 { return; }
    if KEY_ROUTE.iter().any(|r| r.load(Ordering::Acquire) != ROUTE_NONE)
        || PHYSICAL_MOUSE_BUTTONS.load(Ordering::Acquire) != 0 { return; }
    let Some(local) = local_display() else { return; };
    let mut cursor = POINT::default();
    if unsafe { GetPhysicalCursorPos(&mut cursor) } == 0 { return; }
    let Ok(mut s) = state().lock() else { return; };
    if s.phase != Phase::Local || s.incoming.is_some() || s.last_switch.elapsed() < CROSS_COOLDOWN { return; }
    let Some(peer) = s.peer else { return; };
    if let Some(target) = edge_point(local, peer, cursor, dx, dy) {
        let outward = if CROSS_SIDE.load(Ordering::Acquire) < 2 { dx.saturating_abs() } else { dy.saturating_abs() };
        s.push = s.push.saturating_add(outward);
        if s.push >= CROSS_PUSH_PX {
            s.next_id = s.next_id.wrapping_add(1).max(1);
            let id = s.next_id;
            let mut payload = Vec::with_capacity(17);
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&target.x.to_le_bytes());
            payload.extend_from_slice(&target.y.to_le_bytes());
            payload.push(CROSS_SIDE.load(Ordering::Acquire) as u8);
            if slot.tx.send(CtrlFrame::new(CTRL_CROSS_ENTER, slot.session, 0, 0, 0, payload)).is_ok() {
               
                s.phase = Phase::Entering(id, Instant::now());
            }
            s.push = 0;
        }
    } else { s.push = 0; }
}

pub(super) fn enter_request(p: &[u8], session: u64, tx: &mpsc::Sender<CtrlFrame>) {
    if p.len() != 17 { return; }
    let id = u64::from_le_bytes(p[0..8].try_into().unwrap());
    let x = i32::from_le_bytes(p[8..12].try_into().unwrap());
    let y = i32::from_le_bytes(p[12..16].try_into().unwrap());
    let side = p[16];
    let mut ok = false;
    if workflow_cross_screen_enabled() && side < 4 {
        if let Some(local) = local_display() {
            if x >= local.rect.left && x < local.rect.right && y >= local.rect.top && y < local.rect.bottom {
                if let Ok(mut s) = state().lock() {
                    if s.incoming.is_none() && s.phase == Phase::Local && !KVM_REMOTE_NOW.load(Ordering::Acquire)
                        && PHYSICAL_MOUSE_BUTTONS.load(Ordering::Acquire) == 0
                        && KEY_ROUTE.iter().all(|r| r.load(Ordering::Acquire) == ROUTE_NONE)
                        && unsafe { SetPhysicalCursorPos(x, y) } != 0 {
                        s.incoming = Some((id, side)); s.incoming_since = Instant::now();
                        s.incoming_active = false; s.return_pending = false; s.push = 0; ok = true;
                    }
                }
            }
        }
    }
    let _ = tx.send(CtrlFrame::new(CTRL_CROSS_ENTER_ACK, session, 0, id, ok as u64, vec![]));
}

pub(super) fn enter_ack(id: u64, ok: bool) {
    let Ok(mut s) = state().lock() else { return; };
    if let Phase::Entering(expected, _) = s.phase {
        if id == expected {
            s.phase = if ok { Phase::Remote(id) } else { Phase::Local };
            s.last_switch = Instant::now();
            if ok { kvm_set_target(true); }
        }
    }
}

fn geometry(local: Display, peer: Display) -> (f64, f64) {
    let offset = CROSS_OFFSET_MM.load(Ordering::Acquire) as f64;
    match CROSS_SIDE.load(Ordering::Acquire) {
        1 => (-(peer.width_mm as f64), offset),
        2 => (offset, local.height_mm as f64),
        3 => (offset, -(peer.height_mm as f64)),
        _ => (local.width_mm as f64, offset),
    }
}
fn edge_point(local: Display, peer: Display, cursor: POINT, dx: i32, dy: i32) -> Option<POINT> {
    let (px, py) = geometry(local, peer);
    let local_w = local.width_mm as f64;
    let local_h = local.height_mm as f64;
    let peer_w = peer.width_mm as f64;
    let peer_h = peer.height_mm as f64;
    let cx = (cursor.x - local.rect.left) as f64 / local.width() as f64 * local_w;
    let cy = (cursor.y - local.rect.top) as f64 / local.height() as f64 * local_h;
    let side = CROSS_SIDE.load(Ordering::Acquire);
    let near = match side {
        1 => cursor.x <= local.rect.left + 1 && dx < 0 && cy >= py && cy < py + peer_h,
        2 => cursor.y >= local.rect.bottom - 2 && dy > 0 && cx >= px && cx < px + peer_w,
        3 => cursor.y <= local.rect.top + 1 && dy < 0 && cx >= px && cx < px + peer_w,
        _ => cursor.x >= local.rect.right - 2 && dx > 0 && cy >= py && cy < py + peer_h,
    };
    if !near { return None; }
    let outside = match side {
        1 => POINT { x: local.rect.left - 1, y: cursor.y },
        2 => POINT { x: cursor.x, y: local.rect.bottom },
        3 => POINT { x: cursor.x, y: local.rect.top - 1 },
        _ => POINT { x: local.rect.right, y: cursor.y },
    };
    if !unsafe { MonitorFromPoint(outside, 0) }.is_null() { return None; }
    let x = match side { 0 => peer.rect.left + CROSS_INSET_PX, 1 => peer.rect.right - 1 - CROSS_INSET_PX,
        _ => peer.rect.left + ((cx - px) / peer_w * peer.width() as f64).round() as i32 };
    let y = match side { 2 => peer.rect.top + CROSS_INSET_PX, 3 => peer.rect.bottom - 1 - CROSS_INSET_PX,
        _ => peer.rect.top + ((cy - py) / peer_h * peer.height() as f64).round() as i32 };
    Some(POINT { x: x.clamp(peer.rect.left, peer.rect.right - 1),
        y: y.clamp(peer.rect.top, peer.rect.bottom - 1) })
}

pub(super) fn allow_incoming_mouse(p: &[u8]) -> bool {
    let Ok(s) = state().lock() else { return true; };
    if !s.return_pending { return true; }
    p.len() >= 4 && matches!(u32::from_le_bytes(p[0..4].try_into().unwrap()),
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP)
}
pub(super) fn allow_incoming_key(p: &[u8]) -> bool {
    let Ok(s) = state().lock() else { return true; };
    !s.return_pending || p.len() >= 13 && p[12] == 0
}

pub(super) fn remote_motion(dx: i32, dy: i32, session: u64, tx: &mpsc::Sender<CtrlFrame>) {
    let Ok(mut s) = state().lock() else { return; };
    let Some((id, side)) = s.incoming else { return; };
    if !s.incoming_active { return; }
    if s.return_pending || s.last_switch.elapsed() < CROSS_COOLDOWN || dx == 0 && dy == 0
        || KVM_MOUSE_BUTTONS.load(Ordering::Acquire) != 0
        || INJECTED_KEYS.lock().map(|keys| keys.iter().any(|k| *k != 0)).unwrap_or(true) { return; }
    let Some(local) = local_display() else { return; };
    let mut pos = POINT::default();
    if unsafe { GetPhysicalCursorPos(&mut pos) } == 0 { return; }
    let out = match side {
        1 => pos.x >= local.rect.right - 2 && dx > 0,
        2 => pos.y <= local.rect.top + 1 && dy < 0,
        3 => pos.y >= local.rect.bottom - 2 && dy > 0,
        _ => pos.x <= local.rect.left + 1 && dx < 0,
    };
    if !out { s.push = 0; return; }
    let beyond = match side {
        1 => POINT { x: local.rect.right, y: pos.y },
        2 => POINT { x: pos.x, y: local.rect.top - 1 },
        3 => POINT { x: pos.x, y: local.rect.bottom },
        _ => POINT { x: local.rect.left - 1, y: pos.y },
    };
    if !unsafe { MonitorFromPoint(beyond, 0) }.is_null() { s.push = 0; return; }
    s.push = s.push.saturating_add(if side < 2 { dx.saturating_abs() } else { dy.saturating_abs() });
    if s.push >= CROSS_PUSH_PX {
        let mut p = Vec::with_capacity(16);
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&pos.x.to_le_bytes());
        p.extend_from_slice(&pos.y.to_le_bytes());
        if tx.send(CtrlFrame::new(CTRL_CROSS_RETURN, session, 0, 0, 0, p)).is_ok() {
            s.return_pending = true;
            s.last_switch = Instant::now();
        }
        s.push = 0;
    }
}

pub(super) fn return_request(p: &[u8], session: u64, tx: &mpsc::Sender<CtrlFrame>) {
    if p.len() != 16 { return; }
    let id = u64::from_le_bytes(p[0..8].try_into().unwrap());
    let x = i32::from_le_bytes(p[8..12].try_into().unwrap());
    let y = i32::from_le_bytes(p[12..16].try_into().unwrap());
    let Ok(mut s) = state().lock() else { return; };
    if s.phase != Phase::Remote(id) { return; }
    let (Some(local), Some(peer)) = (local_display(), s.peer) else { return; };
    if x < peer.rect.left || x >= peer.rect.right || y < peer.rect.top || y >= peer.rect.bottom { return; }
    let (px, py) = geometry(local, peer);
    let physical_x = px + (x - peer.rect.left) as f64 / peer.width() as f64 * peer.width_mm as f64;
    let physical_y = py + (y - peer.rect.top) as f64 / peer.height() as f64 * peer.height_mm as f64;
    let side = CROSS_SIDE.load(Ordering::Acquire);
    let local_x = match side { 0 => local.rect.right - 1 - CROSS_INSET_PX,
        1 => local.rect.left + CROSS_INSET_PX,
        _ => local.rect.left + (physical_x / local.width_mm as f64 * local.width() as f64).round() as i32 };
    let local_y = match side { 2 => local.rect.bottom - 1 - CROSS_INSET_PX,
        3 => local.rect.top + CROSS_INSET_PX,
        _ => local.rect.top + (physical_y / local.height_mm as f64 * local.height() as f64).round() as i32 };
    kvm_set_target(false);
    unsafe { SetPhysicalCursorPos(local_x.clamp(local.rect.left, local.rect.right - 1),
        local_y.clamp(local.rect.top, local.rect.bottom - 1)); }
    s.phase = Phase::Local;
    s.last_switch = Instant::now();
    let _ = tx.send(CtrlFrame::new(CTRL_CROSS_RETURN_ACK, session, 0, id, 0, vec![]));
}

pub(super) fn return_ack(id: u64) {
    if let Ok(mut s) = state().lock() {
        if s.incoming.is_some_and(|(incoming_id, _)| incoming_id == id) {
            s.incoming = None; s.incoming_active = false; s.return_pending = false; s.push = 0; s.last_switch = Instant::now();
        }
    }
}

pub(super) fn tick(session: u64, tx: &mpsc::Sender<CtrlFrame>, connected: bool) {
    let current = if connected { local_display() } else { None };
    if let Ok(mut s) = state().lock() {
        if connected && s.reported_local != current {
            s.reported_local = current;
            if s.phase != Phase::Local { kvm_set_target(false); s.phase = Phase::Local; }
            if let Some((id, _)) = s.incoming.take() {
                s.incoming_active = false; s.return_pending = false;
                inject_reset();
                let _ = tx.send(CtrlFrame::new(CTRL_CROSS_ABORT, session, 0, id, 0, vec![]));
            }
            if let Some(display) = current {
                let _ = tx.send(CtrlFrame::new(CTRL_DISPLAY_INFO, session, 0, 0, 0, display.encode()));
            }
        }
        if let Phase::Entering(_, start) = s.phase {
            if start.elapsed() >= CROSS_TIMEOUT {
                if let Phase::Entering(id, _) = s.phase {
                    let _ = tx.send(CtrlFrame::new(CTRL_CROSS_ABORT, session, 0, id, 0, vec![]));
                }
                s.phase = Phase::Local; s.push = 0;
            }
        }
        if s.return_pending && s.last_switch.elapsed() >= CROSS_TIMEOUT {
            s.incoming = None; s.incoming_active = false; s.return_pending = false; s.push = 0;
        }
        if s.incoming.is_some() && !s.incoming_active && s.incoming_since.elapsed() >= CROSS_TIMEOUT {
            s.incoming = None; s.push = 0;
        }
    }
}
