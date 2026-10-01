// FIX9: virtual network adapter (虚拟网卡) over the OTi link.
//
//   main process (normal user) <── 2 named pipes ──> helper (same exe, elevated, --net-helper)
//         │ USB Lane0 DATA_NET_BATCH                          │ Wintun adapter "OTI-Link"
//         ▼                                                   ▼
//   peer main process                                   Windows TCP/IP stack
//
// Wintun needs Administrator rights. Running the whole app elevated would put the
// WinFsp drive letter into the elevated logon session, invisible to the normal
// Explorer, so only this small helper is elevated (one UAC prompt per start).
//
// Two one-directional pipes are used because concurrent ReadFile/WriteFile on one
// synchronous handle are serialized by Windows.

use super::*;
use std::net::Ipv4Addr;
use std::os::windows::io::FromRawHandle;
use std::os::windows::process::CommandExt;
use std::sync::mpsc::SyncSender;

pub(super) const DATA_NET_BATCH: u8 = 16;
pub(super) const CTRL_NET_STATE: u8 = 58;
/// A batch is a sequence of `[u16 len][IP packet]`, at most this many bytes.
pub(super) const NET_BATCH_MAX: usize = 256 * 1024;
const MAX_IP_PACKET: usize = 0xFFFF;
const NET_MTU: u32 = 9000;
const RING_CAPACITY: u32 = 0x40_0000; // 4 MiB Wintun ring
const ADAPTER_NAME: &str = "OTI-Link";
/// Fixed GUID so Windows keeps recognising the same network (profile, firewall scope).
const ADAPTER_GUID: u128 = 0x4F54494C_4C49_4E4B_9E2A_4F544E455431;

const P_CONFIG: u8 = 1;
const P_PACKETS: u8 = 2;
const P_SHUTDOWN: u8 = 3;
const P_LOG: u8 = 4;
const P_READY: u8 = 5;
const P_CONFIG_RESULT: u8 = 6;

const PIPE_ACCESS_INBOUND: u32 = 1;
const PIPE_ACCESS_OUTBOUND: u32 = 2;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_CANCELLED: u32 = 1223;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x40;
const SEE_MASK_NOASYNC: u32 = 0x100;
const INFINITE: u32 = 0xFFFF_FFFF;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[repr(C)]
struct SHELLEXECUTEINFOW {
    cb_size: u32,
    f_mask: u32,
    hwnd: HWND,
    lp_verb: *const u16,
    lp_file: *const u16,
    lp_parameters: *const u16,
    lp_directory: *const u16,
    n_show: i32,
    h_inst_app: HINSTANCE,
    lp_id_list: *mut c_void,
    lp_class: *const u16,
    hkey_class: HANDLE,
    dw_hot_key: u32,
    h_icon_or_monitor: HANDLE,
    h_process: HANDLE,
}

unsafe extern "system" {
    fn CreateNamedPipeW(name: *const u16, open_mode: u32, pipe_mode: u32, max_instances: u32,
        out_size: u32, in_size: u32, timeout: u32, sa: *const c_void) -> HANDLE;
    fn ConnectNamedPipe(pipe: HANDLE, overlapped: *mut c_void) -> BOOL;
    fn ShellExecuteExW(info: *mut SHELLEXECUTEINFOW) -> BOOL;
    fn WaitForSingleObject(handle: HANDLE, ms: u32) -> u32;
    fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> BOOL;
    fn GetNamedPipeClientProcessId(pipe: HANDLE, pid: *mut u32) -> BOOL;
    fn GetProcessId(process: HANDLE) -> u32;
}

fn invalid_handle(h: HANDLE) -> bool {
    h.is_null() || h as isize == -1
}

// ------------------------------------------------------------
// Wire helpers
// ------------------------------------------------------------

fn pipe_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&[kind, 0, 0, 0]);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn read_pipe_frame(r: &mut File) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 8];
    r.read_exact(&mut h)?;
    let n = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
    if n > NET_BATCH_MAX + 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pipe frame too large"));
    }
    let mut p = vec![0u8; n];
    r.read_exact(&mut p)?;
    Ok((h[0], p))
}

fn push_packet(batch: &mut Vec<u8>, packet: &[u8]) {
    batch.extend_from_slice(&(packet.len() as u16).to_le_bytes());
    batch.extend_from_slice(packet);
}

/// Calls `f` for every packet in a batch; returns false if the batch is malformed.
pub(super) fn for_each_packet(batch: &[u8], mut f: impl FnMut(&[u8])) -> bool {
    let mut d = batch;
    while !d.is_empty() {
        if d.len() < 2 {
            return false;
        }
        let n = u16::from_le_bytes([d[0], d[1]]) as usize;
        if n == 0 || d.len() < 2 + n {
            return false;
        }
        f(&d[2..2 + n]);
        d = &d[2 + n..];
    }
    true
}

pub(super) fn encode_net_state(ready: bool) -> Vec<u8> {
    // Version 3 adds provider_ready. A client must not install a default route until
    // the provider reports that its local WinNAT transaction actually succeeded.
    let mut p = Vec::with_capacity(18);
    p.extend_from_slice(&3u32.to_le_bytes());
    p.extend_from_slice(&(ready as u32).to_le_bytes());
    p.extend_from_slice(&node_id().to_le_bytes());
    p.push(net_share_role());
    p.push(net_provider_ready() as u8);
    p
}

/// Returns (ready, node_id, peer_share_role, provider_ready). Older peers decode
/// with provider_ready=false, so a FIX13 client never redirects its default route
/// through a peer that cannot positively confirm NAT readiness.
pub(super) fn decode_net_state(p: &[u8]) -> Option<(bool, u64, u8, bool)> {
    if p.len() < 16 { return None; }
    let ver = u32::from_le_bytes(p[0..4].try_into().ok()?);
    if !(1..=3).contains(&ver) { return None; }
    let ready = u32::from_le_bytes(p[4..8].try_into().ok()?) & 1 != 0;
    let node = u64::from_le_bytes(p[8..16].try_into().ok()?);
    let role = if ver >= 2 && p.len() >= 17 { p[16].min(2) } else { NET_SHARE_OFF };
    let provider_ready = ver >= 3 && p.len() >= 18 && p[17] != 0;
    Some((ready, node, role, provider_ready))
}

/// Stable per-installation id; decides which side gets .1 and which .2.
pub(super) fn node_id() -> u64 {
    static ID: OnceLock<u64> = OnceLock::new();
    *ID.get_or_init(|| {
        let path = state_dir().join("node_id");
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(v) = u64::from_str_radix(s.trim(), 16) {
                if v != 0 {
                    return v;
                }
            }
        }
        let host = env::var("COMPUTERNAME").unwrap_or_default();
        let v = (new_id() ^ crc32(host.as_bytes()) as u64).rotate_left(17) | 1;
        let _ = fs::create_dir_all(state_dir());
        let _ = fs::write(&path, format!("{v:016X}\n"));
        v
    })
}

/// (local, peer) addresses inside `subnet.0/24`. None only if both ids and hostnames collide.
pub(super) fn assign_ips(subnet: [u8; 3], local: (u64, &str), peer: (u64, &str)) -> Option<(Ipv4Addr, Ipv4Addr)> {
    let lower = match local.0.cmp(&peer.0) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => match local.1.cmp(peer.1) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => return None,
        },
    };
    let one = Ipv4Addr::new(subnet[0], subnet[1], subnet[2], 1);
    let two = Ipv4Addr::new(subnet[0], subnet[1], subnet[2], 2);
    Some(if lower { (one, two) } else { (two, one) })
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ------------------------------------------------------------
// Main-process side
// ------------------------------------------------------------

/// Where outgoing batches go while a USB session with a network-capable peer is up.
#[derive(Clone)]
pub(super) struct NetLink {
    pub session: u64,
    pub tx: SyncSender<Vec<u8>>,
    pub wake: mpsc::Sender<DataTxJob>,
    pub wake_pending: Arc<AtomicBool>,
}

impl NetLink {
    fn push(&self, batch: Vec<u8>) -> bool {
        if self.tx.try_send(batch).is_err() {
            return false;
        }
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            let _ = self.wake.send(DataTxJob::NetWake);
        }
        true
    }
}

struct Bridge {
    logger: Logger,
    down: Mutex<Option<SyncSender<Vec<u8>>>>,
    link: Mutex<Option<NetLink>>,
    ready: AtomicBool,
    starting: AtomicBool,
    generation: AtomicU64,
    config_serial: AtomicU64,
    pending_config: AtomicU64,
    provider_ready: AtomicBool,
    status: Mutex<String>,
    last_config: Mutex<Option<String>>,
    dropped_tx: AtomicU64,
    dropped_rx: AtomicU64,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

fn bridge() -> Option<&'static Bridge> {
    BRIDGE.get()
}

pub(super) fn net_init(logger: &Logger) {
    let _ = BRIDGE.set(Bridge {
        logger: logger.clone(),
        down: Mutex::new(None),
        link: Mutex::new(None),
        ready: AtomicBool::new(false),
        starting: AtomicBool::new(false),
        generation: AtomicU64::new(1),
        config_serial: AtomicU64::new(0),
        pending_config: AtomicU64::new(0),
        provider_ready: AtomicBool::new(false),
        status: Mutex::new("未启用".into()),
        last_config: Mutex::new(None),
        dropped_tx: AtomicU64::new(0),
        dropped_rx: AtomicU64::new(0),
    });
}

fn set_status(b: &Bridge, text: &str) {
    if let Ok(mut s) = b.status.lock() {
        *s = text.to_string();
    }
    logln!(b.logger, "NET_STATUS {text}");
}

fn set_ready(b: &Bridge, ready: bool) {
    if b.ready.swap(ready, Ordering::AcqRel) != ready {
        b.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn set_provider_ready(b: &Bridge, ready: bool) {
    if b.provider_ready.swap(ready, Ordering::AcqRel) != ready {
        b.generation.fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) fn net_provider_ready() -> bool {
    bridge().map(|b| b.provider_ready.load(Ordering::Acquire)).unwrap_or(false)
}

/// Helper connected and its adapter is up.
pub(super) fn net_ready() -> bool {
    bridge().map(|b| b.ready.load(Ordering::Acquire)).unwrap_or(false)
}

/// Changes whenever local readiness changes (used to re-announce CTRL_NET_STATE).
pub(super) fn net_generation() -> u64 {
    bridge().map(|b| b.generation.load(Ordering::Acquire)).unwrap_or(0)
}

pub(super) fn net_status_text() -> String {
    bridge().and_then(|b| b.status.lock().ok().map(|s| s.clone())).unwrap_or_else(|| "未启用".into())
}

pub(super) fn net_set_link(link: Option<NetLink>) {
    if let Some(b) = bridge() {
        if let Ok(mut l) = b.link.lock() {
            *l = link;
        }
    }
}

/// Clears the link only if it still belongs to `session` (a newer session may own it).
pub(super) fn net_clear_link_for(session: u64) {
    if let Some(b) = bridge() {
        if let Ok(mut l) = b.link.lock() {
            if l.as_ref().map(|x| x.session == session).unwrap_or(false) {
                *l = None;
            }
        }
    }
}

/// Batch received from the peer over USB → local adapter.
pub(super) fn net_deliver_from_usb(batch: &[u8]) {
    let Some(b) = bridge() else { return };
    let sent = b.down.lock().ok()
        .and_then(|d| d.as_ref().map(|tx| tx.try_send(pipe_frame(P_PACKETS, batch)).is_ok()))
        .unwrap_or(false);
    if !sent {
        let n = b.dropped_rx.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 1000 == 0 {
            logln!(b.logger, "NET_RX_DROP batches={n}");
        }
    }
}

/// Default DNS for a PC that reaches the internet through the peer (override: net_share_dns= in settings.ini).
pub(super) const DEFAULT_SHARE_DNS: &str = "223.5.5.5,119.29.29.29";

/// Assign addresses and the internet-sharing plan (idempotent: only sends when something changed).
///
/// Sharing only happens when the two roles match: this PC = provide and peer = client
/// (this PC runs NAT), or this PC = client and peer = provide (this PC routes through
/// the peer). Any other combination leaves both sides as a plain private link.
pub(super) fn net_configure(local: Ipv4Addr, peer: Ipv4Addr, peer_role: u8, peer_provider_ready: bool) {
    let Some(b) = bridge() else { return };
    if !b.ready.load(Ordering::Acquire) { return; }
    let local_role = net_share_role();
    let provide = local_role == NET_SHARE_PROVIDE && peer_role == NET_SHARE_CLIENT;
    let wants_client = local_role == NET_SHARE_CLIENT && peer_role == NET_SHARE_PROVIDE;
    let client = wants_client && peer_provider_ready;
    let sn = net_subnet();
    let dns = net_share_dns();
    let mut cfg = format!("ip={local}\npeer={peer}\nmtu={NET_MTU}\nfirewall_open={}\nprefix={}.{}.{}.0/24\n",
        NET_FIREWALL_OPEN.load(Ordering::Acquire) as u8, sn[0], sn[1], sn[2]);
    cfg += &format!("nat={}\n", provide as u8);
    if client { cfg += &format!("gateway={peer}\ndns={dns}\n"); }

    {
        let mut last = b.last_config.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() == Some(cfg.as_str()) {
            if wants_client && !peer_provider_ready { set_status(b, "等待对端 WinNAT 就绪；尚未切换默认路由"); }
            return;
        }
        *last = Some(cfg.clone());
    }

    // Every configuration is a transaction. Provider readiness is revoked before a
    // new provider transaction and is restored only by its matching success result.
    if provide || b.provider_ready.load(Ordering::Acquire) { set_provider_ready(b, false); }
    let serial = b.config_serial.fetch_add(1, Ordering::AcqRel).wrapping_add(1).max(1);
    b.pending_config.store(serial, Ordering::Release);
    cfg += &format!("generation={serial}\n");
    let sent = b.down.lock().ok()
        .and_then(|d| d.as_ref().map(|tx| tx.send(pipe_frame(P_CONFIG, cfg.as_bytes())).is_ok()))
        .unwrap_or(false);
    if !sent {
        b.pending_config.compare_exchange(serial, 0, Ordering::AcqRel, Ordering::Acquire).ok();
        set_provider_ready(b, false);
        set_status(b, "虚拟网卡配置发送失败");
        return;
    }
    if wants_client && !peer_provider_ready {
        set_status(b, "等待对端 WinNAT 就绪；OTI-Link 默认路由保持关闭");
    } else if provide {
        set_status(b, "正在配置 WinNAT；成功前不会向对端宣告可共享");
    } else if client {
        set_status(b, "对端 WinNAT 已就绪，正在安全切换 OTI-Link 默认路由…");
    } else {
        set_status(b, &format!("正在配置私有链路：本机 {local} ↔ 对端 {peer}"));
    }
}

/// DNS servers used on the client side of internet sharing.
pub(super) fn net_share_dns() -> String {
    static DNS: OnceLock<String> = OnceLock::new();
    DNS.get_or_init(|| {
        let from_ini = fs::read_to_string(state_dir().join("settings.ini")).ok().and_then(|t| {
            t.lines().find_map(|l| l.trim().strip_prefix("net_share_dns=").map(|v| v.trim().to_string()))
        });
        let list: Vec<String> = from_ini.unwrap_or_else(|| DEFAULT_SHARE_DNS.into())
            .split(',').map(str::trim).filter(|x| x.parse::<Ipv4Addr>().is_ok()).map(String::from).collect();
        if list.is_empty() { DEFAULT_SHARE_DNS.into() } else { list.join(",") }
    }).clone()
}

/// The local sharing role changed: resend config and re-announce our role to the peer.
pub(super) fn net_share_role_changed(_before: u8) {
    net_config_refresh();
    if let Some(b) = bridge() {
        set_provider_ready(b, false);
        b.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Forces the next net_configure() to resend (e.g. firewall option changed).
pub(super) fn net_config_refresh() {
    if let Some(b) = bridge() {
        if let Ok(mut last) = b.last_config.lock() {
            *last = None;
        }
    }
}

pub(super) fn net_start() {
    let Some(b) = bridge() else { return };
    if b.ready.load(Ordering::Acquire) || b.starting.swap(true, Ordering::AcqRel) {
        return;
    }
    let exe = match env::current_exe() {
        Ok(x) => x,
        Err(e) => {
            set_status(b, &format!("无法定位程序路径：{e}"));
            b.starting.store(false, Ordering::Release);
            return;
        }
    };
    if !exe.with_file_name("wintun.dll").exists() {
        set_status(b, "缺少 wintun.dll：请把 wintun.dll 放到 oti_link_v10.exe 同一目录");
        b.starting.store(false, Ordering::Release);
        return;
    }
    thread::spawn(move || {
        if let Err(e) = serve_helper(b, &exe) {
            set_status(b, &format!("虚拟网卡未启动：{e}"));
        }
        if let Ok(mut d) = b.down.lock() {
            *d = None;
        }
        b.pending_config.store(0, Ordering::Release);
        set_provider_ready(b, false);
        set_ready(b, false);
        b.starting.store(false, Ordering::Release);
    });
}

pub(super) fn net_stop() {
    let Some(b) = bridge() else { return };
    if let Ok(mut d) = b.down.lock() {
        if let Some(tx) = d.take() {
            let _ = tx.try_send(pipe_frame(P_SHUTDOWN, &[]));
        }
    }
    set_provider_ready(b, false);
    set_ready(b, false);
    set_status(b, "未启用");
}

fn create_pipe(name: &str, access: u32) -> AppResult<HANDLE> {
    let w = wide_null(name);
    let h = unsafe {
        CreateNamedPipeW(w.as_ptr(), access | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS, 1, 1 << 20, 1 << 20, 0, null())
    };
    if invalid_handle(h) {
        return Err(format!("CreateNamedPipeW failed win32={}", unsafe { GetLastError() }).into());
    }
    Ok(h)
}

fn connect_pipe(h: HANDLE) -> AppResult<()> {
    if unsafe { ConnectNamedPipe(h, null_mut()) } == 0 {
        let e = unsafe { GetLastError() };
        if e != ERROR_PIPE_CONNECTED {
            return Err(format!("ConnectNamedPipe failed win32={e}").into());
        }
    }
    Ok(())
}

fn open_pipe_client(name: &str, access: u32) -> io::Result<File> {
    let w = wide_null(name);
    for _ in 0..100 {
        let h = unsafe { CreateFileW(w.as_ptr(), access, 0, null(), OPEN_EXISTING, 0, null_mut()) };
        if !invalid_handle(h) {
            return Ok(unsafe { File::from_raw_handle(h) });
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::last_os_error())
}

pub(super) fn launch_elevated(exe: &Path, params: &str) -> AppResult<usize> {
    let verb = wide_null("runas");
    let file = wide_null(exe.as_os_str());
    let par = wide_null(params);
    let dir = wide_null(env::current_dir()?.as_os_str());
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cb_size = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.f_mask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lp_verb = verb.as_ptr();
    info.lp_file = file.as_ptr();
    info.lp_parameters = par.as_ptr();
    info.lp_directory = dir.as_ptr();
    info.n_show = 0;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = unsafe { GetLastError() };
        if e == ERROR_CANCELLED {
            return Err("用户取消了管理员授权（UAC）".into());
        }
        return Err(format!("ShellExecuteExW failed win32={e}").into());
    }
    if info.h_process.is_null() {
        return Err("helper process handle unavailable".into());
    }
    Ok(info.h_process as usize)
}

fn serve_helper(b: &'static Bridge, exe: &Path) -> AppResult<()> {
    let base = format!(r"\\.\pipe\OTI-Link-net-{}-{:016X}", std::process::id(), new_id());
    let up_name = format!("{base}-up");
    let down_name = format!("{base}-down");
    let up = create_pipe(&up_name, PIPE_ACCESS_INBOUND)?;
    let down = create_pipe(&down_name, PIPE_ACCESS_OUTBOUND)?;
    // Own the handles right away so every error path closes them.
    let mut up_file = unsafe { File::from_raw_handle(up) };
    let mut down_file = unsafe { File::from_raw_handle(down) };

    set_status(b, "正在请求管理员授权并启动虚拟网卡…");
    let process = launch_elevated(exe, &format!("--net-helper {base}"))?;
    let helper_pid = unsafe { GetProcessId(process as HANDLE) };
    logln!(b.logger, "NET_HELPER_LAUNCHED pid={helper_pid} pipes={base}");

    // If the helper dies before connecting, unblock ConnectNamedPipe by connecting ourselves.
    let connected = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    {
        let (connected, exited) = (connected.clone(), exited.clone());
        let (up_name, down_name) = (up_name.clone(), down_name.clone());
        let logger = b.logger.clone();
        thread::spawn(move || {
            let h = process as HANDLE;
            unsafe { WaitForSingleObject(h, INFINITE); }
            let mut code = 0u32;
            unsafe { GetExitCodeProcess(h, &mut code); CloseHandle(h); }
            exited.store(true, Ordering::Release);
            logln!(logger, "NET_HELPER_EXITED code={code}");
            if !connected.load(Ordering::Acquire) {
                let _ = open_pipe_client(&up_name, GENERIC_WRITE);
                let _ = open_pipe_client(&down_name, GENERIC_READ);
            }
        });
    }

    connect_pipe(up_file.as_raw_handle() as HANDLE)?;
    connect_pipe(down_file.as_raw_handle() as HANDLE)?;
    if exited.load(Ordering::Acquire) {
        return Err("虚拟网卡助手已退出（查看 logs 下的 net_helper 日志）".into());
    }
    // A pipe's default DACL grants read access to Everyone: only accept the helper we launched.
    for (name, file) in [("up", &up_file), ("down", &down_file)] {
        let mut pid = 0u32;
        let ok = unsafe { GetNamedPipeClientProcessId(file.as_raw_handle() as HANDLE, &mut pid) } != 0;
        if !ok || pid != helper_pid {
            return Err(format!("unexpected client on {name} pipe (pid={pid}, expected {helper_pid})").into());
        }
    }
    connected.store(true, Ordering::Release);

    let (dtx, drx) = mpsc::sync_channel::<Vec<u8>>(256);
    if let Ok(mut d) = b.down.lock() {
        *d = Some(dtx);
    }
    if let Ok(mut last) = b.last_config.lock() {
        *last = None;
    }
    b.pending_config.store(0, Ordering::Release);
    set_provider_ready(b, false);
    thread::spawn(move || {
        for frame in drx {
            if down_file.write_all(&frame).is_err() {
                break;
            }
        }
    });

    loop {
        match read_pipe_frame(&mut up_file) {
            Ok((P_PACKETS, batch)) => {
                let link = b.link.lock().ok().and_then(|l| l.clone());
                let ok = link.map(|l| l.push(batch)).unwrap_or(false);
                if !ok {
                    let n = b.dropped_tx.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n % 1000 == 0 {
                        logln!(b.logger, "NET_TX_DROP batches={n} (no peer link or USB busy)");
                    }
                }
            }
            Ok((P_LOG, text)) => {
                let text = String::from_utf8_lossy(&text);
                logln!(b.logger, "NET_HELPER {}", text);
                if text.contains("nat_error") {
                    set_status(b, "网络共享失败：WinNAT 创建失败（可能已有 Docker/Hyper-V NAT）");
                } else if text.contains("powershell failed") {
                    set_status(b, "虚拟网卡配置失败：PowerShell 执行失败");
                }
            }
            Ok((P_READY, text)) => {
                logln!(b.logger, "NET_HELPER_READY {}", String::from_utf8_lossy(&text).replace('\n', " "));
                set_ready(b, true);
                set_status(b, "虚拟网卡已就绪，等待对端…");
            }
            Ok((P_CONFIG_RESULT, payload)) => {
                let text = String::from_utf8_lossy(&payload);
                let get = |key: &str| text.lines().find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('='))).map(str::trim);
                let generation = get("generation").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                if generation == 0 || generation != b.pending_config.load(Ordering::Acquire) {
                    logln!(b.logger, "NET_CONFIG_RESULT_STALE generation={generation} pending={}", b.pending_config.load(Ordering::Acquire));
                    continue;
                }
                b.pending_config.store(0, Ordering::Release);
                let ok = get("ok") == Some("1");
                // A result can finish while the user is changing share role. Never let
                // an old provider success re-publish readiness after PROVIDE was revoked.
                let provider = ok && get("provider_ready") == Some("1") && net_share_role() == NET_SHARE_PROVIDE;
                set_provider_ready(b, provider);
                let message = get("message").unwrap_or(if ok { "配置成功" } else { "配置失败" }).replace('\r', " ").replace('\n', " ");
                if ok { set_status(b, &format!("虚拟网卡配置成功：{message}")); }
                else { set_status(b, &format!("虚拟网卡配置失败：{message}")); }
            }
            Ok(_) => {}
            Err(e) => {
                logln!(b.logger, "NET_HELPER_PIPE_CLOSED={e}");
                break;
            }
        }
    }
    set_provider_ready(b, false);
    set_status(b, "虚拟网卡助手已停止");
    Ok(())
}

// ------------------------------------------------------------
// Elevated helper process (oti_link_v10.exe --net-helper <pipe base>)
// ------------------------------------------------------------

#[derive(Clone)]
struct HelperOut {
    up: Arc<Mutex<File>>,
    logger: Logger,
}

impl HelperOut {
    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let frame = pipe_frame(kind, payload);
        self.up.lock().unwrap_or_else(|e| e.into_inner()).write_all(&frame)
    }
    fn log(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        self.logger.line(text);
        let _ = self.send(P_LOG, text.as_bytes());
    }
}

pub(super) fn run_net_helper(base: &str) {
    let logger = match Logger::new("net_helper") {
        Ok(l) => l,
        Err(_) => return,
    };
    logln!(logger, "NET_HELPER_START base={base}");
    if let Err(e) = helper_main(base, &logger) {
        logln!(logger, "NET_HELPER_FATAL={e}");
    }
    logger.line("NET_HELPER_EXIT");
}

fn helper_main(base: &str, logger: &Logger) -> AppResult<()> {
    if !base.starts_with(r"\\.\pipe\OTI-Link-net-") {
        return Err("invalid pipe name".into());
    }
    let up = open_pipe_client(&format!("{base}-up"), GENERIC_WRITE)?;
    let mut down = open_pipe_client(&format!("{base}-down"), GENERIC_READ)?;
    let out = HelperOut { up: Arc::new(Mutex::new(up)), logger: logger.clone() };

    let dll = env::current_exe()?.with_file_name("wintun.dll");
    let wintun = unsafe { wintun::load_from_path(&dll) }
        .map_err(|e| format!("加载 {} 失败：{e}", dll.display()))?;
    let adapter = match wintun::Adapter::create(&wintun, ADAPTER_NAME, "OTI-Link", Some(ADAPTER_GUID)) {
        Ok(a) => a,
        Err(e) => {
            out.log(format!("adapter create failed ({e}); trying to open an existing one"));
            wintun::Adapter::open(&wintun, ADAPTER_NAME)?
        }
    };
    let index = adapter.get_adapter_index()?;
    let alias = adapter.get_name().unwrap_or_else(|_| ADAPTER_NAME.into());
    let session = Arc::new(adapter.start_session(RING_CAPACITY)?);
    out.log(format!("adapter up alias='{alias}' index={index}"));
    out.send(P_READY, format!("alias={alias}\nindex={index}\n").as_bytes())?;

    // Adapter → pipe (→ USB → peer).
    let rx_session = session.clone();
    let rx_out = out.clone();
    let rx_thread = thread::spawn(move || {
        let mut batch = Vec::with_capacity(NET_BATCH_MAX);
        loop {
            let first = match rx_session.receive_blocking() {
                Ok(p) => p,
                Err(_) => break,
            };
            batch.clear();
            push_packet(&mut batch, first.bytes());
            drop(first);
            while batch.len() + 2 + MAX_IP_PACKET <= NET_BATCH_MAX {
                match rx_session.try_receive() {
                    Ok(Some(p)) => push_packet(&mut batch, p.bytes()),
                    _ => break,
                }
            }
            if rx_out.send(P_PACKETS, &batch).is_err() {
                break;
            }
        }
    });

    // Serialize PowerShell configuration. A new request may arrive while an older one is
    // still running; the worker coalesces queued requests so the newest configuration is
    // always the final state instead of racing detached PowerShell threads.
    let (config_tx, config_rx) = mpsc::channel::<Option<String>>();
    let config_out = out.clone();
    let config_thread = thread::spawn(move || {
        while let Ok(job) = config_rx.recv() {
            let Some(mut cfg) = job else { break };
            let mut stop = false;
            loop {
                match config_rx.try_recv() {
                    Ok(Some(newer)) => cfg = newer,
                    Ok(None) => { stop = true; break; }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => { stop = true; break; }
                }
            }
            if stop { break; }
            apply_config(&config_out, index, &cfg);
        }
    });

    // Pipe (← USB ← peer) → adapter.
    let mut bad_batches = 0u64;
    loop {
        match read_pipe_frame(&mut down) {
            Ok((P_PACKETS, batch)) => {
                let ok = for_each_packet(&batch, |packet| {
                    if let Ok(mut p) = session.allocate_send_packet(packet.len() as u16) {
                        p.bytes_mut().copy_from_slice(packet);
                        session.send_packet(p);
                    }
                    // Ring full: drop, exactly like a congested NIC; TCP recovers.
                });
                if !ok {
                    bad_batches += 1;
                    if bad_batches == 1 || bad_batches % 100 == 0 {
                        out.log(format!("malformed packet batch count={bad_batches}"));
                    }
                }
            }
            Ok((P_CONFIG, cfg)) => {
                let _ = config_tx.send(Some(String::from_utf8_lossy(&cfg).to_string()));
            }
            Ok((P_SHUTDOWN, _)) => {
                out.log("shutdown requested");
                break;
            }
            Ok(_) => {}
            Err(_) => break, // main process closed the pipe (exit or crash)
        }
    }
    let _ = config_tx.send(None);
    drop(config_tx);
    let _ = config_thread.join();
    let _ = session.shutdown();
    let _ = rx_thread.join();
    // The adapter (and its routes/DNS) disappears with it; the NAT object would not.
    let _ = run_powershell("if (Get-NetNat -Name 'OTI-Link-NAT') { Remove-NetNat -Name 'OTI-Link-NAT' -Confirm:$false }");
    drop(session);
    drop(adapter); // WintunCloseAdapter removes an adapter this process created
    Ok(())
}

/// Runs a PowerShell script hidden, via -EncodedCommand (no quoting pitfalls).
pub(super) fn run_powershell(script: &str) -> io::Result<String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let o = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&utf16)])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    Ok(format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

fn apply_config(out: &HelperOut, index: u32, cfg: &str) {
    let get = |k: &str| cfg.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('='))).map(str::trim);
    let generation = get("generation").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let send_result = |ok: bool, provider_ready: bool, message: &str| {
        let clean: String = message.replace('\r', " ").replace('\n', " ").chars().take(500).collect();
        let body = format!("generation={generation}\nok={}\nprovider_ready={}\nmessage={clean}\n", ok as u8, provider_ready as u8);
        let _ = out.send(P_CONFIG_RESULT, body.as_bytes());
    };
    let Some(ip) = get("ip").and_then(|v| v.parse::<Ipv4Addr>().ok()) else {
        out.log("config without valid ip rejected"); send_result(false, false, "invalid ip"); return;
    };
    let mtu = get("mtu").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1500).clamp(576, 65000);
    let open = get("firewall_open") == Some("1");
    let nat = get("nat") == Some("1");
    let prefix = get("prefix").filter(|p| {
        let mut it = p.split('/');
        it.next().and_then(|a| a.parse::<Ipv4Addr>().ok()).is_some() && it.next() == Some("24")
    }).unwrap_or("10.77.77.0/24").to_string();
    let gateway = get("gateway").and_then(|v| v.parse::<Ipv4Addr>().ok());
    let dns: Vec<String> = get("dns").unwrap_or("").split(',').map(str::trim)
        .filter(|x| x.parse::<Ipv4Addr>().is_ok()).map(|x| format!("'{x}'")).collect();
    let client = gateway.is_some() && !dns.is_empty();
    let target = prefix.split('/').next().unwrap_or("10.77.77.0");
    out.log(format!("configuring generation={generation} ip={ip}/24 mtu={mtu} firewall_open={open} nat={nat} gateway={gateway:?}"));
    // Strict transaction: expected-absence operations opt into SilentlyContinue one by one.
    // Before touching the adapter, reject any other interface/route that overlaps our /24.
    let script = format!(r#"
$ErrorActionPreference = 'Stop'
$i = {index}
$wantNat = {nat_ps}
$wantClient = {client_ps}
function U32([string]$s) {{
  $b = [System.Net.IPAddress]::Parse($s).GetAddressBytes()
  return [uint64]$b[0] * 16777216 + [uint64]$b[1] * 65536 + [uint64]$b[2] * 256 + [uint64]$b[3]
}}
function PrefixKey([uint64]$value, [int]$bits) {{
  if ($bits -le 0) {{ return [uint64]0 }}
  $div = [math]::Pow(2, 32-$bits)
  return [uint64][math]::Floor($value / $div)
}}
$targetBase = U32 '{target}'
$targetBits = 24
$conflicts = @()
foreach ($a in Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {{ $_.InterfaceIndex -ne $i }}) {{
  if ((PrefixKey (U32 $a.IPAddress) $targetBits) -eq (PrefixKey $targetBase $targetBits)) {{ $conflicts += "ip:$($a.IPAddress)@$($a.InterfaceAlias)" }}
}}
foreach ($r in Get-NetRoute -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {{ $_.InterfaceIndex -ne $i -and $_.DestinationPrefix -ne '0.0.0.0/0' }}) {{
  $q = $r.DestinationPrefix -split '/'; if ($q.Count -ne 2) {{ continue }}
  $bits = [int]$q[1]; $shared = [Math]::Min($targetBits, $bits); if ($shared -le 0) {{ continue }}
  if ((PrefixKey (U32 $q[0]) $shared) -eq (PrefixKey $targetBase $shared)) {{ $conflicts += "route:$($r.DestinationPrefix)@$($r.InterfaceAlias)" }}
}}
if ($conflicts.Count -gt 0) {{ throw ('subnet_conflict ' + (($conflicts | Select-Object -Unique) -join ',')) }}
try {{
  Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -Dhcp Disabled
  Get-NetIPAddress -InterfaceIndex $i -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {{ $_.IPAddress -ne '{ip}' }} | Remove-NetIPAddress -Confirm:$false -ErrorAction SilentlyContinue
  if (-not (Get-NetIPAddress -InterfaceIndex $i -IPAddress '{ip}' -ErrorAction SilentlyContinue)) {{
    New-NetIPAddress -InterfaceIndex $i -IPAddress '{ip}' -PrefixLength 24 -PolicyStore ActiveStore | Out-Null
  }}
  Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -NlMtuBytes {mtu} -PolicyStore ActiveStore
  Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv6 -NlMtuBytes {mtu} -PolicyStore ActiveStore
  for ($n = 0; $n -lt 30; $n++) {{
    $p = Get-NetConnectionProfile -InterfaceIndex $i -ErrorAction SilentlyContinue
    if ($p) {{ if ($p.NetworkCategory -ne 'Private') {{ Set-NetConnectionProfile -InterfaceIndex $i -NetworkCategory Private }}; break }}
    Start-Sleep -Milliseconds 500
  }}
  $a = (Get-NetAdapter -InterfaceIndex $i).Name
  if (-not (Get-NetFirewallRule -Name 'OTI-Link-Ping-In' -ErrorAction SilentlyContinue)) {{
    New-NetFirewallRule -Name 'OTI-Link-Ping-In' -DisplayName 'OTI-Link virtual network: ping' -Direction Inbound -Protocol ICMPv4 -IcmpType 8 -InterfaceAlias $a -Action Allow | Out-Null
  }}
  if ({open_ps}) {{
    if (-not (Get-NetFirewallRule -Name 'OTI-Link-All-In' -ErrorAction SilentlyContinue)) {{
      New-NetFirewallRule -Name 'OTI-Link-All-In' -DisplayName 'OTI-Link virtual network: allow all inbound' -Direction Inbound -InterfaceAlias $a -Action Allow | Out-Null
    }}
  }} else {{ Remove-NetFirewallRule -Name 'OTI-Link-All-In' -ErrorAction SilentlyContinue }}

  if ($wantNat) {{
    Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -Forwarding Enabled
    $nat = Get-NetNat -Name 'OTI-Link-NAT' -ErrorAction SilentlyContinue
    if ($nat -and $nat.InternalIPInterfaceAddressPrefix -ne '{prefix}') {{ Remove-NetNat -Name 'OTI-Link-NAT' -Confirm:$false; $nat = $null }}
    if (-not $nat) {{ New-NetNat -Name 'OTI-Link-NAT' -InternalIPInterfaceAddressPrefix '{prefix}' | Out-Null }}
    $nat = Get-NetNat -Name 'OTI-Link-NAT' -ErrorAction Stop
    if ($nat.InternalIPInterfaceAddressPrefix -ne '{prefix}') {{ throw 'nat_prefix_mismatch' }}
  }} else {{
    if (Get-NetNat -Name 'OTI-Link-NAT' -ErrorAction SilentlyContinue) {{ Remove-NetNat -Name 'OTI-Link-NAT' -Confirm:$false }}
    Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -Forwarding Disabled
  }}

  # Never leave a stale OTI default route. A FIX13 client reaches this branch only
  # after the peer has positively announced provider_ready from a successful NAT result.
  Get-NetRoute -InterfaceIndex $i -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue
  if ($wantClient) {{
    New-NetRoute -InterfaceIndex $i -DestinationPrefix '0.0.0.0/0' -NextHop '{gw}' -RouteMetric 1 -PolicyStore ActiveStore | Out-Null
    Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -InterfaceMetric 5
    Set-DnsClientServerAddress -InterfaceIndex $i -ServerAddresses @({dns_ps})
  }} else {{
    Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -AutomaticMetric Enabled
    Set-DnsClientServerAddress -InterfaceIndex $i -ResetServerAddresses
  }}
  $ips = (Get-NetIPAddress -InterfaceIndex $i -AddressFamily IPv4).IPAddress -join ','
  $gwNow = (Get-NetRoute -InterfaceIndex $i -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue).NextHop -join ','
  $nn = (Get-NetNat -Name 'OTI-Link-NAT' -ErrorAction SilentlyContinue).InternalIPInterfaceAddressPrefix
  Write-Output "OTI_OK ip=$ips gateway=$gwNow nat=$nn"
  exit 0
}} catch {{
  $err = $_.Exception.Message
  # Fail closed: a partial client transaction must never strand the PC behind an
  # unusable default route; a failed provider transaction must not advertise NAT.
  Get-NetRoute -InterfaceIndex $i -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue
  Set-DnsClientServerAddress -InterfaceIndex $i -ResetServerAddresses -ErrorAction SilentlyContinue
  Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -AutomaticMetric Enabled -ErrorAction SilentlyContinue
  if ($wantNat) {{
    if (Get-NetNat -Name 'OTI-Link-NAT' -ErrorAction SilentlyContinue) {{ Remove-NetNat -Name 'OTI-Link-NAT' -Confirm:$false -ErrorAction SilentlyContinue }}
    Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -Forwarding Disabled -ErrorAction SilentlyContinue
  }}
  Write-Error ("OTI_ERR " + $err)
  exit 2
}}
"#, open_ps = if open { "$true" } else { "$false" }, nat_ps = if nat { "$true" } else { "$false" },
        client_ps = if client { "$true" } else { "$false" }, gw = gateway.map(|g| g.to_string()).unwrap_or_default(),
        dns_ps = dns.join(","), prefix = prefix, target = target);
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&utf16)])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(o) => {
            let raw = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let text: String = raw.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(500).collect();
            let ok = o.status.success() && raw.contains("OTI_OK");
            let provider_ready = ok && nat;
            out.log(format!("configured generation={generation} exit={} {text}", o.status.code().unwrap_or(-1)));
            send_result(ok, provider_ready, if text.is_empty() { if ok { "ok" } else { "PowerShell failed" } } else { &text });
        }
        Err(e) => {
            out.log(format!("powershell failed: {e}"));
            send_result(false, false, &format!("PowerShell launch failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrip_and_malformed_detection() {
        let mut b = vec![];
        push_packet(&mut b, &[0x45, 1, 2, 3]);
        push_packet(&mut b, &[0x60; 40]);
        let mut seen = vec![];
        assert!(for_each_packet(&b, |p| seen.push(p.len())));
        assert_eq!(seen, [4, 40]);
        assert!(!for_each_packet(&b[..b.len() - 1], |_| {}));
        assert!(!for_each_packet(&[0, 0], |_| {}));
    }

    #[test]
    fn ip_assignment_is_symmetric() {
        let s = [10, 77, 77];
        let (a1, a2) = assign_ips(s, (5, "A"), (9, "B")).unwrap();
        let (b1, b2) = assign_ips(s, (9, "B"), (5, "A")).unwrap();
        assert_eq!((a1, a2), (b2, b1));
        assert_eq!(a1, Ipv4Addr::new(10, 77, 77, 1));
        assert!(assign_ips(s, (7, "X"), (7, "X")).is_none());
        assert_eq!(assign_ips(s, (7, "A"), (7, "B")).unwrap().0, Ipv4Addr::new(10, 77, 77, 1));
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn net_state_roundtrip() {
        let p = encode_net_state(true);
        assert_eq!(decode_net_state(&p), Some((true, node_id(), net_share_role(), net_provider_ready())));
        // A v1 peer (16 bytes, version 1) decodes with sharing off.
        let mut v1 = p[..16].to_vec();
        v1[0] = 1;
        assert_eq!(decode_net_state(&v1), Some((true, node_id(), NET_SHARE_OFF, false)));
        assert_eq!(decode_net_state(&p[..8]), None);
    }
}
