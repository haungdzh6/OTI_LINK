// FIX15: B-side Moonlight process control.
//
// Moonlight is started with CreateProcess directly (not through `cmd /C`), so
// OTI-Link owns the real Moonlight process handle. With the `stream` command
// line the process lives exactly as long as the stream, which is how B notices
// that streaming ended for any reason.

use super::*;
use super::workflow_logic::{exe_file_name, render_moonlight_command, split_command_line};
use std::os::windows::{io::IntoRawHandle, process::CommandExt};

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_TIMEOUT: u32 = 0x102;
const STILL_ACTIVE: u32 = 259;
const TH32CS_SNAPPROCESS: u32 = 0x2;
const MAPVK_VK_TO_VSC: u32 = 0;

#[repr(C)]
struct ProcessEntry32W {
    size: u32, usage: u32, pid: u32, default_heap_id: usize, module_id: u32,
    threads: u32, parent_pid: u32, pri_class_base: i32, flags: u32, exe_file: [u16; 260],
}

unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    fn TerminateProcess(process: HANDLE, code: u32) -> BOOL;
    fn WaitForSingleObject(handle: HANDLE, ms: u32) -> u32;
    fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> BOOL;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    fn Process32FirstW(snap: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
    fn Process32NextW(snap: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
    fn ExpandEnvironmentStringsW(src: *const u16, dst: *mut u16, size: u32) -> u32;
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(cb: unsafe extern "system" fn(HWND, LPARAM) -> BOOL, l: LPARAM) -> BOOL;
    fn GetWindowThreadProcessId(hwnd: HWND, pid: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: HWND) -> BOOL;
    fn GetForegroundWindow() -> HWND;
    fn AttachThreadInput(attach: u32, to: u32, on: BOOL) -> BOOL;
    fn BringWindowToTop(hwnd: HWND) -> BOOL;
}

/// A Moonlight process OTI-Link either launched (`owned`) or adopted after a
/// restart/resync. Only owned processes are ever force-terminated.
pub(super) struct MoonProc {
    handle: HANDLE,
    pub pid: u32,
    pub owned: bool,
}
unsafe impl Send for MoonProc {}
impl Drop for MoonProc {
    fn drop(&mut self) { unsafe { CloseHandle(self.handle); } }
}
impl MoonProc {
    pub fn alive(&self) -> bool { unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT } }
    pub fn exit_code(&self) -> Option<u32> {
        let mut c = 0u32;
        (unsafe { GetExitCodeProcess(self.handle, &mut c) } != 0 && c != STILL_ACTIVE).then_some(c)
    }
    pub fn wait_exit(&self, timeout: Duration) -> bool {
        unsafe { WaitForSingleObject(self.handle, timeout.as_millis().min(u32::MAX as u128 - 1) as u32) != WAIT_TIMEOUT }
    }
}

fn expand_env(s: &str) -> String {
    let src = wide_null(s);
    let mut buf = vec![0u16; 1024];
    for _ in 0..2 {
        let n = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 { return s.to_string(); }
        if n <= buf.len() { return String::from_utf16_lossy(&buf[..n.saturating_sub(1)]); }
        buf.resize(n, 0);
    }
    s.to_string()
}

/// Expanded program path of the configured command (used for launch and adoption).
fn program_of(template: &str) -> Result<(String, String), String> {
    let (program, args) = split_command_line(template).ok_or("Moonlight 启动命令为空或引号不完整")?;
    Ok((expand_env(&program), args))
}

pub(super) fn launch(template: &str, peer_host: &str, logger: &Logger) -> Result<MoonProc, String> {
    let command = render_moonlight_command(template, peer_host);
    let (program, args) = program_of(&command)?;
    let args = expand_env(&args);
    let mut cmd = std::process::Command::new(&program);
    if !args.is_empty() { cmd.raw_arg(&args); }
    if let Some(dir) = Path::new(&program).parent().filter(|d| d.is_dir()) { cmd.current_dir(dir); }
    let child = cmd.spawn().map_err(|e| format!("无法启动 {program}：{e}"))?;
    let pid = child.id();
    let handle = child.into_raw_handle() as HANDLE;
    logln!(logger, "MOONLIGHT_LAUNCHED pid={pid} program={program} args={args}");
    Ok(MoonProc { handle, pid, owned: true })
}

/// Finds an already running Moonlight (same exe name as the configured command),
/// e.g. after B's OTI-Link restarted while a stream it started is still up.
pub(super) fn find_running(template: &str) -> Option<MoonProc> {
    let want = exe_file_name(&program_of(template).ok()?.0);
    let me = std::process::id();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 { return None; }
        let mut e: ProcessEntry32W = std::mem::zeroed();
        e.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut found = None;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e.exe_file.iter().position(|c| *c == 0).unwrap_or(e.exe_file.len());
            if e.pid != me && String::from_utf16_lossy(&e.exe_file[..len]).to_ascii_lowercase() == want {
                let h = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, e.pid);
                if !h.is_null() { found = Some(MoonProc { handle: h, pid: e.pid, owned: false }); break; }
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

struct FindWindow { pid: u32, hwnd: HWND }
unsafe extern "system" fn find_window_cb(hwnd: HWND, l: LPARAM) -> BOOL {
    let f = unsafe { &mut *(l as *mut FindWindow) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid); }
    if pid == f.pid && unsafe { IsWindowVisible(hwnd) } != 0 { f.hwnd = hwnd; return 0; }
    1
}

/// Best effort: the quit shortcut only works when Moonlight has keyboard focus.
fn focus_moonlight(pid: u32, logger: &Logger) {
    let mut f = FindWindow { pid, hwnd: null_mut() };
    unsafe { EnumWindows(find_window_cb, &mut f as *mut FindWindow as LPARAM); }
    if f.hwnd.is_null() { logger.line("MOONLIGHT_WINDOW_NOT_FOUND"); return; }
    unsafe {
        let fg = GetForegroundWindow();
        let mut fg_pid = 0u32;
        let fg_thread = if fg.is_null() { 0 } else { GetWindowThreadProcessId(fg, &mut fg_pid) };
        if fg_pid == pid { return; }
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, 1) != 0;
        BringWindowToTop(f.hwnd);
        let ok = SetForegroundWindow(f.hwnd) != 0;
        if attached { AttachThreadInput(me, fg_thread, 0); }
        logln!(logger, "MOONLIGHT_FOCUS ok={ok}");
    }
}

fn key(vk: u16, down: bool) -> INPUT {
    // SDL (Moonlight) identifies keys by scan code, so send a real one.
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
    INPUT { r#type: INPUT_KEYBOARD, u: INPUTUNION { ki: KEYBDINPUT {
        wVk: vk, wScan: scan, dwFlags: if down { 0 } else { KEYEVENTF_KEYUP }, time: 0, dwExtraInfo: KVM_TAG,
    } } }
}

fn send_quit_shortcut() -> Result<(), String> {
    // Moonlight PC: Ctrl+Alt+Shift+Q quits the stream (the host app keeps running).
    const LCTRL: u16 = 0xA2; const LALT: u16 = 0xA4; const LSHIFT: u16 = 0xA0; const Q: u16 = 0x51;
    let down = [key(LCTRL, true), key(LALT, true), key(LSHIFT, true)];
    let tap = [key(Q, true), key(Q, false)];
    let up = [key(LSHIFT, false), key(LALT, false), key(LCTRL, false)];
    let size = std::mem::size_of::<INPUT>() as i32;
    let mut sent = 0;
    for (batch, pause) in [(&down[..], 30), (&tap[..], 30), (&up[..], 0)] {
        sent += unsafe { SendInput(batch.len() as u32, batch.as_ptr(), size) };
        if pause > 0 { thread::sleep(Duration::from_millis(pause)); }
    }
    if sent != 8 {
        return Err(format!("SendInput 只发送了 {sent}/8 个按键事件 win32={}", unsafe { GetLastError() }));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum QuitResult { Exited, Killed, ShortcutSentStillRunning }

pub(super) const QUIT_GRACE: Duration = Duration::from_secs(5);

/// Ctrl+Alt+Shift+Q, wait for exit, then terminate if OTI-Link launched it.
pub(super) fn quit(p: &MoonProc, logger: &Logger) -> Result<QuitResult, String> {
    if !p.alive() { return Ok(QuitResult::Exited); }
    focus_moonlight(p.pid, logger);
    if let Err(e) = send_quit_shortcut() { logln!(logger, "MOONLIGHT_QUIT_SHORTCUT_FAILED {e}"); }
    else { logln!(logger, "MOONLIGHT_QUIT_SHORTCUT pid={}", p.pid); }
    if p.wait_exit(QUIT_GRACE) { return Ok(QuitResult::Exited); }
    if !p.owned {
        // Not ours (adopted GUI instance): the shortcut may have returned it to the
        // host list, which is fine; never kill a Moonlight we did not start.
        return Ok(QuitResult::ShortcutSentStillRunning);
    }
    // The CreateProcess handle has full access and pins the PID while we hold it.
    if unsafe { TerminateProcess(p.handle, 1) } == 0 {
        return Err(format!("无法结束 Moonlight 进程 pid={} win32={}", p.pid, unsafe { GetLastError() }));
    }
    if !p.wait_exit(Duration::from_secs(3)) {
        return Err(format!("Moonlight 进程 pid={} 无法结束", p.pid));
    }
    logln!(logger, "MOONLIGHT_TERMINATED pid={}", p.pid);
    Ok(QuitResult::Killed)
}
