// FIX15: Moonlight secondary-screen mode <-> OTI mode.
// Design: FIX15_Moonlight_OTI_状态机设计.md
//
// A (controller, "本机是 A 电脑") owns one worker thread that reconciles the
// system towards DESIRED: VDD device, Windows topology, and B's Moonlight via
// CTRL_WORKFLOW_MODE/ACK. B (viewer) owns one worker thread that starts/quits
// Moonlight and reports when a stream ends (CTRL_WORKFLOW_EVENT). Hooks and the
// USB event loop only post messages; nothing here blocks them.
//
// The OTI gate (KVM, cross-screen, clipboard) is closed whenever this PC is not
// in stable OTI mode, including while a switch is in progress.

use super::*;
use super::display_ctl::{display_extend, display_primary_only, display_snapshot, ensure_vdd_enabled};
use super::moonlight::{self, MoonProc, QuitResult};
use super::workflow_logic::*;
use std::sync::Condvar;

pub(super) const WORKFLOW_HOTKEY_VK: u16 = 0x7A; // Ctrl+Alt+F11 (fixed)
pub(super) const WORKFLOW_HOTKEY_TEXT: &str = "Ctrl+Alt+F11";

// ============================================================
// OTI gate
// ============================================================

const GATE_CONTROLLER: u8 = 1;
const GATE_VIEWER: u8 = 2;
static GATE_CLOSED: AtomicU8 = AtomicU8::new(0);

pub(super) fn workflow_kvm_enabled() -> bool { GATE_CLOSED.load(Ordering::Acquire) == 0 }
// In OTI mode the user's own settings apply again (FIX14 forced them all on).
pub(super) fn workflow_cross_screen_enabled() -> bool { workflow_kvm_enabled() && CROSS_SCREEN_ON.load(Ordering::Acquire) }
pub(super) fn workflow_clip_text_enabled() -> bool { workflow_kvm_enabled() && CLIP_TEXT_ON.load(Ordering::Acquire) }
pub(super) fn workflow_clip_image_enabled() -> bool { workflow_kvm_enabled() && CLIP_IMAGE_ON.load(Ordering::Acquire) }
pub(super) fn workflow_clip_files_enabled() -> bool { workflow_kvm_enabled() && CLIP_FILES_ON.load(Ordering::Acquire) }

fn set_gate(bit: u8, closed: bool, logger: &Logger) {
    let before = if closed { GATE_CLOSED.fetch_or(bit, Ordering::AcqRel) } else { GATE_CLOSED.fetch_and(!bit, Ordering::AcqRel) };
    let after = if closed { before | bit } else { before & !bit };
    if (before == 0) == (after == 0) { return; }
    // Release everything OTI may hold, so no stuck key, remote mouse owner or
    // half-finished edge crossing survives the switch in either direction.
    kvm_set_target(false);
    cross_screen::cancel();
    inject_reset();
    if after == 0 { cross_screen::invalidate_display(); }
    logln!(logger, "WORKFLOW_GATE={}", if after == 0 { "OPEN" } else { "CLOSED" });
    notify_ui();
}

// ============================================================
// Status shown in the settings window / tray
// ============================================================

static STATUS: Mutex<String> = Mutex::new(String::new());
static BUSY: AtomicBool = AtomicBool::new(false);
// 0 = OTI, 1 = Moonlight, 2 = switching; what the tray shows for this PC.
static MODE_LABEL: AtomicU8 = AtomicU8::new(0);

fn notify_ui() {
    let hwnd = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() { unsafe { let _ = PostMessageW(hwnd, WM_WORKFLOW_UPDATED, 0, 0); } }
}
fn set_status(text: impl Into<String>, busy: bool) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = text.into();
    BUSY.store(busy, Ordering::Release);
    notify_ui();
}
/// Status plus a tray balloon: the hotkey is usually pressed with no settings window open.
fn announce(text: String, error: bool) {
    tray_balloon(if error { "OTI-Link 模式切换失败" } else { "OTI-Link 模式切换" }, &text, error);
    set_status(text, false);
}
pub(super) fn workflow_status_text() -> String {
    let s = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if s.is_empty() { "OTI 模式".into() } else { s }
}
pub(super) fn workflow_busy() -> bool { BUSY.load(Ordering::Acquire) }
pub(super) fn workflow_mode_label() -> &'static str {
    match MODE_LABEL.load(Ordering::Acquire) { 1 => "Moonlight 副屏模式", 2 => "切换中…", _ => "OTI 模式" }
}

// ============================================================
// Link to the current USB session
// ============================================================

struct Link { session: u64, tx: mpsc::Sender<CtrlFrame>, connected: Arc<AtomicBool> }
static LINK: Mutex<Option<Link>> = Mutex::new(None);

/// Held by run_connected for the lifetime of one USB session.
pub(super) struct WorkflowLinkGuard(u64);
impl Drop for WorkflowLinkGuard {
    fn drop(&mut self) {
        let mut l = LINK.lock().unwrap_or_else(|e| e.into_inner());
        if l.as_ref().map(|l| l.session == self.0).unwrap_or(false) { *l = None; }
    }
}
pub(super) fn workflow_link_attach(session: u64, tx: mpsc::Sender<CtrlFrame>, connected: Arc<AtomicBool>) -> WorkflowLinkGuard {
    *LINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Link { session, tx, connected });
    WorkflowLinkGuard(session)
}
fn link_up() -> bool {
    LINK.lock().ok().and_then(|l| l.as_ref().map(|l| l.connected.load(Ordering::Acquire))).unwrap_or(false)
}
fn link_send(build: impl FnOnce(u64) -> CtrlFrame) -> Result<(), String> {
    let l = LINK.lock().map_err(|_| "link lock poisoned")?;
    let l = l.as_ref().filter(|l| l.connected.load(Ordering::Acquire)).ok_or("B 电脑未连接（OTI USB）")?;
    l.tx.send(build(l.session)).map_err(|_| "OTI 控制通道已关闭".to_string())
}

// ============================================================
// Controller (A)
// ============================================================

enum CtlMsg {
    Wake,
    /// An explicit mode button: re-run every step of that mode as a repair.
    Force(Mode),
    StartVdd,
    PeerConnected,
    PeerMoonlightEnded { generation: u64, message: String },
}

static CTL_TX: OnceLock<Mutex<mpsc::Sender<CtlMsg>>> = OnceLock::new();
static DESIRED: AtomicU8 = AtomicU8::new(WIRE_OTI);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_TOGGLE_MS: AtomicU64 = AtomicU64::new(0);
static CLOCK: OnceLock<Instant> = OnceLock::new();

fn ctl_send(m: CtlMsg) {
    if let Some(tx) = CTL_TX.get() { let _ = tx.lock().map(|t| t.send(m)); }
}
fn desired() -> Mode { Mode::from_wire(DESIRED.load(Ordering::Acquire) as u64).unwrap_or(Mode::Oti) }
fn set_desired(m: Mode) { DESIRED.store(m.wire(), Ordering::Release); }

/// Global hotkey / tray menu. Runs on the keyboard-hook thread: never blocks.
pub(super) fn workflow_toggle() {
    if !moon_controller_enabled() { return; }
    let now = CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1;
    let last = LAST_TOGGLE_MS.load(Ordering::Acquire);
    if !debounce_ok((last != 0).then_some(last), now) { return; }
    LAST_TOGGLE_MS.store(now, Ordering::Release);
    let _ = DESIRED.fetch_update(Ordering::AcqRel, Ordering::Acquire,
        |d| Some(toggled(Mode::from_wire(d as u64).unwrap_or(Mode::Oti)).wire()));
    ctl_send(CtlMsg::Wake);
}
pub(super) fn workflow_request_moonlight() {
    if !moon_controller_enabled() { set_status("请先勾选“本机是 A 电脑”", false); return; }
    set_desired(Mode::Moonlight);
    ctl_send(CtlMsg::Force(Mode::Moonlight));
}
/// Always allowed (it is the safe direction), even if the controller box was unticked.
pub(super) fn workflow_request_oti() {
    set_desired(Mode::Oti);
    ctl_send(CtlMsg::Force(Mode::Oti));
}
pub(super) fn workflow_start_vdd() { ctl_send(CtlMsg::StartVdd); }
/// Called when an OTI session finished its handshake (SYNC_COMPLETE).
pub(super) fn workflow_peer_connected() { ctl_send(CtlMsg::PeerConnected); }

#[derive(Clone, Debug)]
struct Ack { mode: Mode, ok: bool, moonlight_running: bool, message: String }
struct AckSlot { generation: u64, mode: Mode, result: Option<Ack> }
static ACK: Mutex<AckSlot> = Mutex::new(AckSlot { generation: 0, mode: Mode::Oti, result: None });
static ACK_CV: Condvar = Condvar::new();

pub(super) fn workflow_peer_ack(flags: u64, mode: u64, generation: u64, message: String, logger: &Logger) {
    let Some(mode) = Mode::from_wire(mode) else { return };
    let mut a = ACK.lock().unwrap_or_else(|e| e.into_inner());
    if a.generation != generation || a.mode != mode || a.result.is_some() {
        logln!(logger, "WORKFLOW_ACK_STALE mode={} gen={generation} waiting={}", mode.name(), a.generation);
        return;
    }
    let ack = Ack { mode, ok: flags & ACK_FLAG_OK != 0, moonlight_running: flags & ACK_FLAG_MOONLIGHT_RUNNING != 0, message };
    logln!(logger, "WORKFLOW_ACK mode={} gen={generation} ok={} running={} msg={:?}", mode.name(), ack.ok, ack.moonlight_running, ack.message);
    a.result = Some(ack);
    ACK_CV.notify_all();
}

pub(super) fn workflow_peer_event(kind: u64, generation: u64, message: String, logger: &Logger) {
    logln!(logger, "WORKFLOW_EVENT kind={kind} gen={generation} msg={message:?}");
    if kind == EVENT_MOONLIGHT_ENDED { ctl_send(CtlMsg::PeerMoonlightEnded { generation, message }); }
}

enum PeerErr { Rejected(String), NoReply(String) }
impl PeerErr { fn text(&self) -> &str { match self { PeerErr::Rejected(s) | PeerErr::NoReply(s) => s } } }

enum StepErr { Aborted, Failed(String) }

struct Controller {
    logger: Logger,
    phase: Phase,
    force: bool,
    peer_synced: bool,
    /// After a sync attempt B did not answer, don't retry before this.
    sync_retry_at: Instant,
    /// Generation of the command that B's current Moonlight belongs to.
    moon_gen: u64,
    /// Why the next Leave happens when B (not the user) ended the stream.
    leave_note: Option<String>,
    /// B was asked for Moonlight and has not yet confirmed OTI.
    peer_may_stream: bool,
}

impl Controller {
    fn set_phase(&mut self, p: Phase) {
        self.phase = p;
        MODE_LABEL.store(match p { Phase::Stable(Mode::Oti) => 0, Phase::Stable(Mode::Moonlight) => 1, _ => 2 }, Ordering::Release);
        set_gate(GATE_CONTROLLER, !p.gate_open(), &self.logger);
        logln!(self.logger, "WORKFLOW_PHASE={p:?} desired={}", desired().name());
    }

    /// Start-up: if the VDD display is extended, we are (or were) streaming.
    /// Treat it as Moonlight mode and let the peer sync decide; a dead stream
    /// then collapses back to "display 1 only" by itself.
    fn adopt_reality(&mut self) {
        if !moon_controller_enabled() { return; }
        let instance = vdd_instance_id();
        if !valid_instance_id(&instance) { return; }
        match display_snapshot(&instance) {
            Ok(s) if s.vdd_active => {
                set_desired(Mode::Moonlight);
                self.peer_may_stream = true;
                self.set_phase(Phase::Stable(Mode::Moonlight));
                set_status("启动时发现虚拟屏处于扩展状态：等待 B 电脑连接后校验 Moonlight 串流…", false);
            }
            Ok(_) => {}
            Err(e) => logln!(self.logger, "WORKFLOW_STARTUP_SNAPSHOT_FAILED {e}"),
        }
    }

    fn command_peer(&mut self, mode: Mode, launch: bool, timeout: Duration) -> Result<(u64, Ack), PeerErr> {
        let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut a = ACK.lock().unwrap_or_else(|e| e.into_inner());
            *a = AckSlot { generation, mode, result: None };
        }
        let host = env::var("COMPUTERNAME").unwrap_or_default();
        let flags = if launch { MODE_FLAG_LAUNCH } else { 0 };
        link_send(|session| CtrlFrame::new(CTRL_WORKFLOW_MODE, session, flags, mode.wire() as u64, generation, host.into_bytes()))
            .map_err(PeerErr::NoReply)?;
        if mode == Mode::Moonlight { self.peer_may_stream = true; }
        logln!(self.logger, "WORKFLOW_TX mode={} launch={launch} gen={generation}", mode.name());
        let deadline = Instant::now() + timeout;
        let mut a = ACK.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(ack) = a.result.clone() {
                return if ack.ok { Ok((generation, ack)) }
                    else { Err(PeerErr::Rejected(if ack.message.is_empty() { "B 电脑拒绝了切换".into() } else { ack.message })) };
            }
            if !link_up() { return Err(PeerErr::NoReply("等待 B 电脑响应时 USB 连接断开".into())); }
            let now = Instant::now();
            if now >= deadline { return Err(PeerErr::NoReply(format!("B 电脑 {} 秒内没有响应", timeout.as_secs()))); }
            a = ACK_CV.wait_timeout(a, (deadline - now).min(Duration::from_millis(200))).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn checkpoint(&self) -> Result<(), StepErr> {
        if desired() == Mode::Moonlight { Ok(()) } else { Err(StepErr::Aborted) }
    }

    fn enter_steps(&mut self) -> Result<(), StepErr> {
        let fail = StepErr::Failed;
        if !moon_controller_enabled() { return Err(fail("本机未勾选“本机是 A 电脑”".into())); }
        // Before any UAC prompt: without B there is nobody to start Moonlight.
        if !link_up() { return Err(fail("B 电脑未连接（OTI USB），无法启动 Moonlight".into())); }
        let instance = vdd_instance_id();
        set_status("检查虚拟显示器（VDD）…（如果被禁用会弹出 UAC）", true);
        if ensure_vdd_enabled(&instance, &self.logger).map_err(fail)? {
            self.logger.line("WORKFLOW_VDD_ENABLED_VIA_UAC");
        }
        self.checkpoint()?;
        set_status("切换到扩展显示（显示器 1 + 2）…", true);
        display_extend(&instance, &self.logger).map_err(fail)?;
        cross_screen::invalidate_display();
        self.checkpoint()?;
        set_status("通知 B 电脑启动 Moonlight…", true);
        match self.command_peer(Mode::Moonlight, true, Duration::from_secs(12)) {
            Ok((generation, _)) => { self.moon_gen = generation; self.peer_synced = true; }
            Err(e) => return Err(fail(format!("B 电脑：{}", e.text()))),
        }
        self.checkpoint()
    }

    fn enter(&mut self) {
        self.set_phase(Phase::Entering);
        match self.enter_steps() {
            Ok(()) => {
                self.set_phase(Phase::Stable(Mode::Moonlight));
                set_status(format!("Moonlight 副屏模式：扩展显示 + Moonlight 运行，OTI KVM/剪贴板已关闭（{WORKFLOW_HOTKEY_TEXT} 切回）"), false);
            }
            // The user toggled back while we were on the way; reconcile() runs Leave next.
            Err(StepErr::Aborted) => self.logger.line("WORKFLOW_ENTER_ABORTED desired=OTI"),
            Err(StepErr::Failed(e)) => {
                logln!(self.logger, "WORKFLOW_ENTER_FAILED {e}");
                set_desired(Mode::Oti);
                self.leave(Some((format!("进入 Moonlight 副屏模式失败：{e}"), true)));
            }
        }
    }

    /// Always runs to the end: OTI is the safe direction.
    fn leave(&mut self, note: Option<(String, bool)>) {
        let note = note.or_else(|| self.leave_note.take().map(|n| (n, false)));
        self.set_phase(Phase::Leaving);
        let mut warnings = Vec::new();
        if link_up() {
            set_status("通知 B 电脑退出 Moonlight…", true);
            match self.command_peer(Mode::Oti, false, moonlight::QUIT_GRACE + Duration::from_secs(8)) {
                Ok((_, ack)) => {
                    self.peer_synced = true;
                    self.peer_may_stream = false;
                    if !ack.message.is_empty() { warnings.push(format!("B 电脑：{}", ack.message)); }
                }
                // A rejection is final until the user presses a mode button again;
                // only silence is retried (so B never gets the quit shortcut on a loop).
                Err(e) => {
                    self.peer_synced = matches!(e, PeerErr::Rejected(_));
                    self.sync_retry_at = Instant::now() + Duration::from_secs(10);
                    warnings.push(format!("B 电脑：{}", e.text()));
                }
            }
        } else {
            self.peer_synced = false;
            if self.peer_may_stream { warnings.push("B 电脑未连接，重连后会自动让它退出 Moonlight".into()); }
        }
        set_status("切回仅显示器 1…", true);
        let instance = vdd_instance_id();
        if valid_instance_id(&instance) {
            if let Err(e) = display_primary_only(&instance, &self.logger) { warnings.push(e); }
        } else {
            warnings.push(format!("VDD 实例 ID 无效（{instance:?}），未改动显示"));
        }
        cross_screen::invalidate_display();
        self.set_phase(Phase::Stable(Mode::Oti));

        let mut text = String::from("OTI 模式：仅显示器 1，OTI KVM/剪贴板已恢复");
        let mut error = false;
        let noteworthy = note.is_some();
        if let Some((n, is_error)) = note { text = format!("{n}\n{text}"); error |= is_error; }
        if !warnings.is_empty() {
            text.push_str("\n注意：");
            text.push_str(&warnings.join("；"));
            error = true;
        }
        logln!(self.logger, "WORKFLOW_OTI_DONE warnings={}", warnings.len());
        if error || noteworthy { announce(text, error); } else { set_status(text, false); }
    }

    /// Reconnected while in Moonlight mode: ask B, never relaunch.
    fn verify_moonlight(&mut self) {
        set_status("校验 B 电脑的 Moonlight 串流…", true);
        match self.command_peer(Mode::Moonlight, false, Duration::from_secs(8)) {
            Ok((generation, _)) => {
                self.moon_gen = generation;
                self.peer_synced = true;
                set_status(format!("Moonlight 副屏模式（已与 B 电脑同步，{WORKFLOW_HOTKEY_TEXT} 切回）"), false);
            }
            Err(PeerErr::Rejected(e)) => {
                logln!(self.logger, "WORKFLOW_VERIFY_NOT_STREAMING {e}");
                self.leave_note = Some(format!("Moonlight 已不在串流（{e}），自动切回 OTI 模式"));
                set_desired(Mode::Oti);
            }
            Err(PeerErr::NoReply(e)) => {
                self.sync_retry_at = Instant::now() + Duration::from_secs(10);
                set_status(format!("Moonlight 副屏模式（与 B 电脑同步失败：{e}）"), false);
            }
        }
    }

    fn assert_oti(&mut self) {
        match self.command_peer(Mode::Oti, false, moonlight::QUIT_GRACE + Duration::from_secs(8)) {
            Ok(_) => { self.peer_synced = true; self.peer_may_stream = false; self.logger.line("WORKFLOW_SYNC_OTI ok"); }
            Err(PeerErr::Rejected(e)) => {
                self.peer_synced = true;
                logln!(self.logger, "WORKFLOW_SYNC_OTI_REJECTED {e}");
                set_status(format!("OTI 模式（B 电脑：{e}）"), false);
            }
            Err(PeerErr::NoReply(e)) => {
                self.sync_retry_at = Instant::now() + Duration::from_secs(10);
                logln!(self.logger, "WORKFLOW_SYNC_OTI_FAILED {e}");
            }
        }
    }

    fn start_vdd(&mut self) {
        set_status("正在启动虚拟显示器（VDD）…（如果被禁用会弹出 UAC）", true);
        match ensure_vdd_enabled(&vdd_instance_id(), &self.logger) {
            Ok(true) => announce("虚拟显示器已启动（已通过 UAC 执行 pnputil /enable-device）".into(), false),
            Ok(false) => set_status("虚拟显示器已经是启用状态", false),
            Err(e) => { logln!(self.logger, "VDD_START_FAILED {e}"); announce(format!("启动虚拟显示器失败：{e}"), true); }
        }
    }

    fn reconcile(&mut self) {
        // Bounded: every pass either reaches a stable phase or changes DESIRED.
        for _ in 0..6 {
            let synced = self.peer_synced || Instant::now() < self.sync_retry_at;
            let action = plan(self.phase, desired(), std::mem::take(&mut self.force), synced, link_up());
            match action {
                Action::Idle => return,
                Action::Enter => self.enter(),
                Action::Leave => self.leave(None),
                Action::VerifyMoonlight => self.verify_moonlight(),
                Action::AssertOti => { self.assert_oti(); return; }
            }
        }
    }

    fn handle(&mut self, msg: CtlMsg) {
        match msg {
            CtlMsg::Wake => {}
            CtlMsg::Force(m) => { if desired() == m { self.force = true; } }
            CtlMsg::StartVdd => self.start_vdd(),
            CtlMsg::PeerConnected => { self.peer_synced = false; self.sync_retry_at = Instant::now(); }
            CtlMsg::PeerMoonlightEnded { generation, message } => {
                let streaming = matches!(self.phase, Phase::Stable(Mode::Moonlight) | Phase::Entering);
                if streaming && desired() == Mode::Moonlight && generation >= self.moon_gen {
                    self.leave_note = Some(format!("Moonlight 串流已结束（{message}），自动切回 OTI 模式"));
                    set_desired(Mode::Oti);
                } else {
                    logln!(self.logger, "WORKFLOW_PEER_ENDED_IGNORED gen={generation} moon_gen={} phase={:?}", self.moon_gen, self.phase);
                }
            }
        }
    }
}

fn controller_main(rx: mpsc::Receiver<CtlMsg>, logger: Logger) {
    let mut c = Controller {
        logger, phase: Phase::Stable(Mode::Oti), force: false, peer_synced: false,
        sync_retry_at: Instant::now(), moon_gen: 0, leave_note: None, peer_may_stream: false,
    };
    c.adopt_reality();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => {
                c.handle(m);
                // Coalesce a burst (e.g. hotkey presses) into one decision.
                while let Ok(m) = rx.try_recv() { c.handle(m); }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        c.reconcile();
    }
}

// ============================================================
// Viewer (B)
// ============================================================

enum ViewMsg {
    Apply { local_session: u64, tx: mpsc::Sender<CtrlFrame>, controller_session: u64, flags: u64, mode: Mode, generation: u64, host: String },
}
static VIEW_TX: OnceLock<Mutex<mpsc::Sender<ViewMsg>>> = OnceLock::new();

/// CTRL_WORKFLOW_MODE from the controller. Called on the USB event loop: never blocks.
pub(super) fn workflow_on_remote_mode(controller_session: u64, local_session: u64, tx: &mpsc::Sender<CtrlFrame>,
                                      flags: u64, mode: u64, generation: u64, host: String, logger: &Logger) {
    let reject = |msg: &str| {
        logln!(logger, "WORKFLOW_REMOTE_REJECTED mode={mode} gen={generation} {msg}");
        let _ = tx.send(CtrlFrame::new(CTRL_WORKFLOW_ACK, local_session, 0, mode, generation, msg.as_bytes().to_vec()));
    };
    let Some(m) = Mode::from_wire(mode) else { return reject("unsupported workflow mode") };
    if moon_controller_enabled() {
        return reject("两台电脑都勾选了“本机是 A 电脑”，请在 B 电脑上取消勾选");
    }
    let msg = ViewMsg::Apply { local_session, tx: tx.clone(), controller_session, flags, mode: m, generation, host };
    let sent = VIEW_TX.get().map(|v| v.lock().map(|t| t.send(msg).is_ok()).unwrap_or(false)).unwrap_or(false);
    if !sent { reject("B 端工作线程未运行"); }
}

struct Viewer {
    logger: Logger,
    mode: Mode,
    moon: Option<MoonProc>,
    moon_gen: u64,
    last: Option<(u64, u64)>,
    reply: Option<(u64, mpsc::Sender<CtrlFrame>)>,
}

impl Viewer {
    fn set_mode(&mut self, m: Mode) {
        if self.mode != m { logln!(self.logger, "WORKFLOW_VIEWER_MODE={}", m.name()); }
        self.mode = m;
        MODE_LABEL.store(if m == Mode::Moonlight { 1 } else { 0 }, Ordering::Release);
        set_gate(GATE_VIEWER, m == Mode::Moonlight, &self.logger);
    }

    fn apply(&mut self, flags: u64, mode: Mode, generation: u64, host: &str) -> (bool, String) {
        let launch = flags & MODE_FLAG_LAUNCH != 0;
        if self.moon.as_ref().map(|p| !p.alive()).unwrap_or(false) { self.moon = None; }
        // Lost track (B's OTI-Link restarted mid-stream): adopt a running Moonlight
        // for a verify, or when we believe a stream of ours is still up.
        if self.moon.is_none() && ((mode == Mode::Moonlight && !launch) || self.mode == Mode::Moonlight) {
            self.moon = moonlight::find_running(&moonlight_start_command());
            if let Some(p) = &self.moon { logln!(self.logger, "MOONLIGHT_ADOPTED pid={}", p.pid); }
        }
        match viewer_plan(mode, launch, self.moon.is_some()) {
            ViewerAction::Keep => {
                self.set_mode(Mode::Moonlight);
                self.moon_gen = generation;
                set_status("Moonlight 副屏模式（由 A 电脑控制）：OTI KVM/剪贴板已关闭", false);
                (true, String::new())
            }
            ViewerAction::Launch => {
                // Gate first: the two control planes are never active together.
                self.set_mode(Mode::Moonlight);
                set_status("正在启动 Moonlight…", true);
                let p = match moonlight::launch(&moonlight_start_command(), host, &self.logger) {
                    Ok(p) => p,
                    Err(e) => {
                        self.set_mode(Mode::Oti);
                        set_status(format!("Moonlight 启动失败：{e}"), false);
                        return (false, e);
                    }
                };
                // A bad host name / unpaired host usually exits right away.
                if p.wait_exit(Duration::from_secs(2)) {
                    let code = p.exit_code().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                    self.set_mode(Mode::Oti);
                    let e = format!("Moonlight 启动后立即退出（退出码 {code}）：请检查主机名/配对/启动命令");
                    set_status(e.clone(), false);
                    return (false, e);
                }
                self.moon = Some(p);
                self.moon_gen = generation;
                set_status(format!("Moonlight 副屏模式：正在串流 {host}，OTI KVM/剪贴板已关闭"), false);
                (true, String::new())
            }
            ViewerAction::NotStreaming => {
                self.set_mode(Mode::Oti);
                set_status("OTI 模式", false);
                (false, "B 电脑上的 Moonlight 没有在运行".into())
            }
            ViewerAction::Quit => {
                set_status("正在退出 Moonlight…", true);
                let p = self.moon.take().expect("viewer_plan(Quit) implies a process");
                match moonlight::quit(&p, &self.logger) {
                    Ok(r) => {
                        self.set_mode(Mode::Oti);
                        set_status("OTI 模式（由 A 电脑控制）：Moonlight 已退出，OTI KVM/剪贴板已恢复", false);
                        match r {
                            QuitResult::Exited => (true, String::new()),
                            QuitResult::Killed => (true, "Moonlight 未响应退出快捷键，已结束进程".into()),
                            QuitResult::ShortcutSentStillRunning =>
                                (true, "已向非 OTI 启动的 Moonlight 发送退出快捷键（未强制结束）".into()),
                        }
                    }
                    Err(e) => {
                        // Still streaming: keep OTI gated rather than run both planes.
                        self.moon = Some(p);
                        set_status(format!("Moonlight 退出失败：{e}"), false);
                        (false, e)
                    }
                }
            }
            ViewerAction::Nothing => {
                self.set_mode(Mode::Oti);
                set_status("OTI 模式", false);
                (true, String::new())
            }
        }
    }

    /// Stream ended without OTI asking (quit on B, network loss, host ended it).
    fn watch(&mut self) {
        if self.mode != Mode::Moonlight { return; }
        let Some(p) = &self.moon else { return };
        if p.alive() { return; }
        let code = p.exit_code().map(|c| c.to_string()).unwrap_or_else(|| "?".into());
        self.moon = None;
        self.set_mode(Mode::Oti);
        logln!(self.logger, "MOONLIGHT_ENDED exit={code} gen={}", self.moon_gen);
        set_status("Moonlight 已结束，OTI KVM/剪贴板已恢复", false);
        if let Some((session, tx)) = &self.reply {
            // If the USB link is down this send goes nowhere; A's verify on
            // reconnect learns the same thing.
            let _ = tx.send(CtrlFrame::new(CTRL_WORKFLOW_EVENT, *session, 0, EVENT_MOONLIGHT_ENDED, self.moon_gen,
                format!("B 电脑上的 Moonlight 已退出，退出码 {code}").into_bytes()));
        }
    }
}

fn viewer_main(rx: mpsc::Receiver<ViewMsg>, logger: Logger) {
    let mut v = Viewer { logger, mode: Mode::Oti, moon: None, moon_gen: 0, last: None, reply: None };
    loop {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(ViewMsg::Apply { local_session, tx, controller_session, flags, mode, generation, host }) => {
                if !accept_generation(v.last, controller_session, generation) {
                    logln!(v.logger, "WORKFLOW_REMOTE_STALE mode={} gen={generation}", mode.name());
                    continue;
                }
                v.last = Some((controller_session, generation));
                v.reply = Some((local_session, tx.clone()));
                logln!(v.logger, "WORKFLOW_RX mode={} flags={flags} gen={generation} host={host}", mode.name());
                let (ok, message) = v.apply(flags, mode, generation, &host);
                let running = v.moon.as_ref().map(|p| p.alive()).unwrap_or(false);
                let ack = (if ok { ACK_FLAG_OK } else { 0 }) | (if running { ACK_FLAG_MOONLIGHT_RUNNING } else { 0 });
                let _ = tx.send(CtrlFrame::new(CTRL_WORKFLOW_ACK, local_session, ack, mode.wire() as u64, generation, message.into_bytes()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        v.watch();
    }
}

// ============================================================
// Start-up
// ============================================================

pub(super) fn workflow_start(logger: &Logger) {
    CLOCK.get_or_init(Instant::now);
    let (ctl_tx, ctl_rx) = mpsc::channel();
    let (view_tx, view_rx) = mpsc::channel();
    if CTL_TX.set(Mutex::new(ctl_tx)).is_err() || VIEW_TX.set(Mutex::new(view_tx)).is_err() { return; }
    let l = logger.clone();
    thread::Builder::new().name("workflow-controller".into()).spawn(move || controller_main(ctl_rx, l)).ok();
    let l = logger.clone();
    thread::Builder::new().name("workflow-viewer".into()).spawn(move || viewer_main(view_rx, l)).ok();
}
