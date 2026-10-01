// FIX15: A-side control of the Virtual Display Driver device and the Windows
// display topology.
//
// * Device state is read with cfgmgr32 (no elevation needed). Only when the
//   device is actually disabled do we ask UAC to run
//   `pnputil /enable-device "<instance>"`, and we wait for pnputil's exit code.
// * Topology uses the CCD API (QueryDisplayConfig/SetDisplayConfig). That is
//   what DisplaySwitch.exe /extend and /internal call, but it returns an error
//   code synchronously and lets us verify the result. "Display 1 only"
//   deactivates just the VDD path, so other physical monitors stay as they are.

use super::*;
use super::workflow_logic::{adapter_path_matches, valid_instance_id};

// ---- cfgmgr32: device node state ------------------------------------------------

const CR_SUCCESS: u32 = 0;
const CR_NO_SUCH_DEVNODE: u32 = 0x0D;
const DN_STARTED: u32 = 0x0000_0008;
const DN_HAS_PROBLEM: u32 = 0x0000_0400;
const CM_PROB_DISABLED: u32 = 22;

#[link(name = "cfgmgr32")]
unsafe extern "system" {
    fn CM_Locate_DevNodeW(devinst: *mut u32, device_id: *const u16, flags: u32) -> u32;
    fn CM_Get_DevNode_Status(status: *mut u32, problem: *mut u32, devinst: u32, flags: u32) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VddDevice {
    Started,
    Disabled,
    /// Present but not started; carries the CM_PROB_* code (0 = still starting).
    Problem(u32),
    NotFound,
}

pub(super) fn vdd_device_state(instance: &str) -> Result<VddDevice, String> {
    let id = wide_null(instance.trim());
    let mut devinst = 0u32;
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, id.as_ptr(), 0) };
    if cr == CR_NO_SUCH_DEVNODE { return Ok(VddDevice::NotFound); }
    if cr != CR_SUCCESS { return Err(format!("CM_Locate_DevNode 失败 CR=0x{cr:X}")); }
    let (mut status, mut problem) = (0u32, 0u32);
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS { return Err(format!("CM_Get_DevNode_Status 失败 CR=0x{cr:X}")); }
    Ok(if status & DN_HAS_PROBLEM != 0 {
        if problem == CM_PROB_DISABLED { VddDevice::Disabled } else { VddDevice::Problem(problem) }
    } else if status & DN_STARTED != 0 {
        VddDevice::Started
    } else {
        VddDevice::Problem(0)
    })
}

unsafe extern "system" {
    fn WaitForSingleObject(handle: HANDLE, ms: u32) -> u32;
    fn GetExitCodeProcess(process: HANDLE, code: *mut u32) -> BOOL;
}
const WAIT_OBJECT_0: u32 = 0;
const PNPUTIL_REBOOT_REQUIRED: u32 = 3010;

/// UAC → `pnputil /enable-device "<instance>"`, then wait for its exit code.
fn enable_vdd_elevated(instance: &str, logger: &Logger) -> Result<(), String> {
    let root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let pnputil = PathBuf::from(root).join("System32").join("pnputil.exe");
    let params = format!("/enable-device \"{}\"", instance.trim());
    logln!(logger, "VDD_ENABLE_ELEVATED exe={} params={params}", pnputil.display());
    let process = launch_elevated(&pnputil, &params).map_err(|e| e.to_string())? as HANDLE;
    let waited = unsafe { WaitForSingleObject(process, 60_000) };
    let mut code = u32::MAX;
    unsafe { GetExitCodeProcess(process, &mut code); CloseHandle(process); }
    if waited != WAIT_OBJECT_0 { return Err("pnputil 60 秒内没有结束".into()); }
    logln!(logger, "VDD_PNPUTIL_EXIT code={code}");
    match code {
        0 => Ok(()),
        PNPUTIL_REBOOT_REQUIRED => Err("pnputil 要求重启后才能启用该设备".into()),
        c => Err(format!("pnputil /enable-device 失败，退出码 {c}（实例 ID 是否正确？）")),
    }
}

/// Makes sure the VDD device is enabled and started. Returns true if UAC was used.
pub(super) fn ensure_vdd_enabled(instance: &str, logger: &Logger) -> Result<bool, String> {
    if !valid_instance_id(instance) {
        return Err(format!("VDD 实例 ID 格式无效：{instance:?}"));
    }
    let mut elevated = false;
    match vdd_device_state(instance)? {
        VddDevice::Started => return Ok(false),
        VddDevice::NotFound => return Err(format!("找不到设备 {instance}（请在设备管理器→详细信息→设备实例路径核对 VDD 实例 ID）")),
        VddDevice::Problem(p) if p != 0 => {
            logln!(logger, "VDD_DEVICE_PROBLEM code={p}; trying pnputil /enable-device");
            enable_vdd_elevated(instance, logger)?;
            elevated = true;
        }
        VddDevice::Problem(_) => {}
        VddDevice::Disabled => {
            enable_vdd_elevated(instance, logger)?;
            elevated = true;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match vdd_device_state(instance)? {
            VddDevice::Started => { logln!(logger, "VDD_DEVICE_STARTED elevated={elevated}"); return Ok(elevated); }
            s if Instant::now() >= deadline => return Err(format!("VDD 设备 15 秒内未启动（状态 {s:?}）")),
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

// ---- CCD display topology --------------------------------------------------------

#[repr(C)] #[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Debug)]
struct Luid { low: u32, high: i32 }
#[repr(C)] #[derive(Clone, Copy, Default)]
struct PathSourceInfo { adapter_id: Luid, id: u32, mode_info_idx: u32, status_flags: u32 }
#[repr(C)] #[derive(Clone, Copy, Default)]
struct Rational { numerator: u32, denominator: u32 }
#[repr(C)] #[derive(Clone, Copy, Default)]
struct PathTargetInfo {
    adapter_id: Luid, id: u32, mode_info_idx: u32, output_technology: u32, rotation: u32, scaling: u32,
    refresh_rate: Rational, scan_line_ordering: u32, target_available: BOOL, status_flags: u32,
}
#[repr(C)] #[derive(Clone, Copy, Default)]
struct PathInfo { source: PathSourceInfo, target: PathTargetInfo, flags: u32 }
/// DISPLAYCONFIG_MODE_INFO; the 48-byte union is 8-byte aligned (pixelRate is UINT64).
#[repr(C)] #[derive(Clone, Copy, Default)]
struct ModeInfo { info_type: u32, id: u32, adapter_id: Luid, data: [u64; 6] }
#[repr(C)]
struct DeviceInfoHeader { kind: u32, size: u32, adapter_id: Luid, id: u32 }
#[repr(C)]
struct AdapterName { header: DeviceInfoHeader, device_path: [u16; 128] }

const _: () = assert!(std::mem::size_of::<PathInfo>() == 72);
const _: () = assert!(std::mem::size_of::<ModeInfo>() == 64);
const _: () = assert!(std::mem::size_of::<AdapterName>() == 276);

const QDC_ALL_PATHS: u32 = 0x1;
const QDC_ONLY_ACTIVE_PATHS: u32 = 0x2;
const SDC_TOPOLOGY_INTERNAL: u32 = 0x1;
const SDC_TOPOLOGY_EXTEND: u32 = 0x4;
const SDC_USE_SUPPLIED_DISPLAY_CONFIG: u32 = 0x20;
const SDC_APPLY: u32 = 0x80;
const SDC_SAVE_TO_DATABASE: u32 = 0x200;
const SDC_ALLOW_CHANGES: u32 = 0x400;
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x1;
const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xFFFF_FFFF;
const DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME: u32 = 4;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, num_paths: *mut u32, num_modes: *mut u32) -> i32;
    fn QueryDisplayConfig(flags: u32, num_paths: *mut u32, paths: *mut PathInfo, num_modes: *mut u32,
        modes: *mut ModeInfo, topology: *mut u32) -> i32;
    fn SetDisplayConfig(num_paths: u32, paths: *const PathInfo, num_modes: u32, modes: *const ModeInfo, flags: u32) -> i32;
    fn DisplayConfigGetDeviceInfo(packet: *mut DeviceInfoHeader) -> i32;
}

fn win32_text(rc: i32) -> String {
    match rc {
        5 => "拒绝访问（锁屏或 UAC 安全桌面时无法改显示设置）".into(),
        31 => "显卡驱动拒绝（ERROR_GEN_FAILURE）".into(),
        87 => "参数无效".into(),
        1610 => "配置无效（ERROR_BAD_CONFIGURATION）".into(),
        _ => format!("win32={rc}"),
    }
}

fn query(flags: u32) -> Result<(Vec<PathInfo>, Vec<ModeInfo>), String> {
    for _ in 0..4 {
        let (mut np, mut nm) = (0u32, 0u32);
        let rc = unsafe { GetDisplayConfigBufferSizes(flags, &mut np, &mut nm) };
        if rc != 0 { return Err(format!("GetDisplayConfigBufferSizes：{}", win32_text(rc))); }
        let mut paths = vec![PathInfo::default(); np as usize];
        let mut modes = vec![ModeInfo::default(); nm as usize];
        let rc = unsafe { QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), null_mut()) };
        if rc == ERROR_INSUFFICIENT_BUFFER { continue; } // topology changed between the two calls
        if rc != 0 { return Err(format!("QueryDisplayConfig：{}", win32_text(rc))); }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        return Ok((paths, modes));
    }
    Err("QueryDisplayConfig：显示拓扑持续变化".into())
}

/// Caches adapter LUID -> "is the VDD adapter" for one decision.
struct VddMatcher<'a> { instance: &'a str, cache: HashMap<Luid, bool> }
impl<'a> VddMatcher<'a> {
    fn new(instance: &'a str) -> Self { Self { instance, cache: HashMap::new() } }
    fn is_vdd(&mut self, adapter: Luid) -> bool {
        if let Some(v) = self.cache.get(&adapter) { return *v; }
        let mut n = AdapterName {
            header: DeviceInfoHeader { kind: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                size: std::mem::size_of::<AdapterName>() as u32, adapter_id: adapter, id: 0 },
            device_path: [0; 128],
        };
        let v = unsafe { DisplayConfigGetDeviceInfo(&mut n.header) } == 0 && {
            let len = n.device_path.iter().position(|c| *c == 0).unwrap_or(n.device_path.len());
            adapter_path_matches(&String::from_utf16_lossy(&n.device_path[..len]), self.instance)
        };
        self.cache.insert(adapter, v);
        v
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct DisplaySnapshot {
    pub vdd_active: bool,
    pub other_active: usize,
    pub vdd_available: bool,
}

pub(super) fn display_snapshot(instance: &str) -> Result<DisplaySnapshot, String> {
    let mut m = VddMatcher::new(instance);
    let mut snap = DisplaySnapshot::default();
    let (active, _) = query(QDC_ONLY_ACTIVE_PATHS)?;
    for p in active.iter().filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0) {
        if m.is_vdd(p.target.adapter_id) { snap.vdd_active = true; } else { snap.other_active += 1; }
    }
    let (all, _) = query(QDC_ALL_PATHS)?;
    snap.vdd_available = snap.vdd_active
        || all.iter().any(|p| p.target.target_available != 0 && m.is_vdd(p.target.adapter_id));
    Ok(snap)
}

fn set_config(paths: &[PathInfo], modes: &[ModeInfo], flags: u32) -> Result<(), String> {
    let (pp, mp) = if paths.is_empty() { (null(), null()) } else { (paths.as_ptr(), modes.as_ptr()) };
    let mut last = 0;
    for attempt in 0..3 {
        let rc = unsafe { SetDisplayConfig(paths.len() as u32, pp, modes.len() as u32, mp, flags) };
        if rc == 0 { return Ok(()); }
        last = rc;
        // Access denied while the secure desktop (UAC/lock) is up is transient.
        if rc != 5 { break; }
        thread::sleep(Duration::from_millis(500 * (attempt + 1)));
    }
    Err(format!("SetDisplayConfig：{}", win32_text(last)))
}

fn wait_for(instance: &str, timeout: Duration, ok: impl Fn(&DisplaySnapshot) -> bool) -> Result<DisplaySnapshot, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let snap = display_snapshot(instance)?;
        if ok(&snap) || Instant::now() >= deadline { return Ok(snap); }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Activates one available VDD path on top of the current active configuration.
fn attach_vdd_path(instance: &str) -> Result<(), String> {
    let mut m = VddMatcher::new(instance);
    let (mut paths, modes) = query(QDC_ONLY_ACTIVE_PATHS)?;
    let used: HashSet<(Luid, u32)> = paths.iter().map(|p| (p.source.adapter_id, p.source.id)).collect();
    let (all, _) = query(QDC_ALL_PATHS)?;
    let mut candidate = all.into_iter().find(|p| p.target.target_available != 0
        && m.is_vdd(p.target.adapter_id)
        && !used.contains(&(p.source.adapter_id, p.source.id)))
        .ok_or("没有可用的 VDD 显示路径")?;
    candidate.flags = DISPLAYCONFIG_PATH_ACTIVE;
    candidate.source.mode_info_idx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    candidate.target.mode_info_idx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    paths.push(candidate);
    set_config(&paths, &modes, SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE)
}

/// Deactivates only the VDD path(s). Ok(false) if none was active.
fn detach_vdd_paths(instance: &str) -> Result<bool, String> {
    let mut m = VddMatcher::new(instance);
    let (mut paths, modes) = query(QDC_ONLY_ACTIVE_PATHS)?;
    let (mut changed, mut others) = (false, 0usize);
    for p in paths.iter_mut().filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0) {
        if m.is_vdd(p.target.adapter_id) {
            p.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
            p.source.mode_info_idx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            p.target.mode_info_idx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
            changed = true;
        } else {
            others += 1;
        }
    }
    if !changed { return Ok(false); }
    if others == 0 { return Err("VDD 是唯一的活动显示器，拒绝关闭（会导致没有屏幕）".into()); }
    set_config(&paths, &modes, SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE)?;
    Ok(true)
}

const VERIFY_TIMEOUT: Duration = Duration::from_secs(4);

/// "Extend display": display 1 + VDD display. Verified with QueryDisplayConfig.
pub(super) fn display_extend(instance: &str, logger: &Logger) -> Result<(), String> {
    let snap = wait_for(instance, Duration::from_secs(8), |s| s.vdd_available)?;
    if snap.vdd_active { logger.line("DISPLAY_EXTEND already_active"); return Ok(()); }
    if !snap.vdd_available { return Err("Windows 中没有出现 VDD 显示器（驱动已启用但没有虚拟屏？）".into()); }

    let mut errors = Vec::new();
    match set_config(&[], &[], SDC_APPLY | SDC_TOPOLOGY_EXTEND) {
        Ok(()) => {
            if wait_for(instance, VERIFY_TIMEOUT, |s| s.vdd_active)?.vdd_active {
                logger.line("DISPLAY_EXTEND ok method=topology_extend");
                return Ok(());
            }
            errors.push("扩展拓扑未包含 VDD".to_string());
        }
        Err(e) => errors.push(e),
    }
    match attach_vdd_path(instance) {
        Ok(()) => {
            if wait_for(instance, VERIFY_TIMEOUT, |s| s.vdd_active)?.vdd_active {
                logger.line("DISPLAY_EXTEND ok method=attach_path");
                return Ok(());
            }
            errors.push("激活 VDD 路径后校验失败".to_string());
        }
        Err(e) => errors.push(e),
    }
    let e = errors.join("；");
    logln!(logger, "DISPLAY_EXTEND_FAILED {e}");
    Err(format!("扩展显示失败：{e}"))
}

/// "Display 1 only": the VDD display is no longer part of the desktop, so the
/// cursor can't wander into it. Verified with QueryDisplayConfig.
pub(super) fn display_primary_only(instance: &str, logger: &Logger) -> Result<(), String> {
    let snap = display_snapshot(instance)?;
    if !snap.vdd_active { logger.line("DISPLAY_PRIMARY_ONLY already"); return Ok(()); }

    let mut errors = Vec::new();
    match detach_vdd_paths(instance) {
        Ok(_) => {
            let s = wait_for(instance, VERIFY_TIMEOUT, |s| !s.vdd_active)?;
            if !s.vdd_active { logger.line("DISPLAY_PRIMARY_ONLY ok method=detach_path"); return Ok(()); }
            errors.push("停用 VDD 路径后校验失败".to_string());
        }
        // Never fall back to "internal" when VDD is the only screen: that could blank A.
        Err(e) if snap.other_active == 0 => return Err(e),
        Err(e) => errors.push(e),
    }
    match set_config(&[], &[], SDC_APPLY | SDC_TOPOLOGY_INTERNAL) {
        Ok(()) => {
            let s = wait_for(instance, VERIFY_TIMEOUT, |s| !s.vdd_active)?;
            if !s.vdd_active && s.other_active > 0 {
                logger.line("DISPLAY_PRIMARY_ONLY ok method=topology_internal");
                return Ok(());
            }
            if s.other_active == 0 {
                // "PC screen only" picked nothing usable; put the desktop back.
                let _ = set_config(&[], &[], SDC_APPLY | SDC_TOPOLOGY_EXTEND);
            }
            errors.push("“仅电脑屏幕”拓扑校验失败".to_string());
        }
        Err(e) => errors.push(e),
    }
    let e = errors.join("；");
    logln!(logger, "DISPLAY_PRIMARY_ONLY_FAILED {e}");
    Err(format!("切回仅显示器 1 失败：{e}"))
}
