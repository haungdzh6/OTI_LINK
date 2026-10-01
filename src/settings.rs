// FIX8: user settings (tray menu → 设置…).
//
// Everything the hot paths need (KVM hotkey, clipboard toggles, display power)
// lives in atomics so the low-level hooks and worker threads never take a lock
// to read it. The settings window runs on the tray thread and only writes the
// atomics when the user presses 保存.

use super::*;
use std::cell::RefCell;

// ------------------------------------------------------------
// Hotkey model
// ------------------------------------------------------------

pub(super) const MOD_CTRL: u8 = 1;
pub(super) const MOD_ALT: u8 = 2;
pub(super) const MOD_SHIFT: u8 = 4;
pub(super) const MOD_WIN: u8 = 8;

pub(super) const HK_KEY: u8 = 0;
pub(super) const HK_MOUSE: u8 = 1;
pub(super) const HK_MOUSE_MIDDLE: u16 = 3;
pub(super) const HK_MOUSE_X1: u16 = 4;
pub(super) const HK_MOUSE_X2: u16 = 5;

const VK_ESCAPE: u16 = 0x1B;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hotkey {
    pub mods: u8,
    pub kind: u8,
    pub code: u16,
}

impl Hotkey {
    pub const DEFAULT: Hotkey = Hotkey { mods: MOD_CTRL | MOD_ALT, kind: HK_KEY, code: 0x7B /* F12 */ };

    pub const fn pack(self) -> u32 {
        self.mods as u32 | (self.kind as u32) << 8 | (self.code as u32) << 16
    }
    pub const fn unpack(v: u32) -> Self {
        Self { mods: (v & 0xFF) as u8, kind: ((v >> 8) & 0xFF) as u8, code: (v >> 16) as u16 }
    }

    pub fn validate(self) -> Result<(), &'static str> {
        if self.mods & !(MOD_CTRL | MOD_ALT | MOD_SHIFT | MOD_WIN) != 0 {
            return Err("无效的修饰键组合");
        }
        match self.kind {
            HK_MOUSE => match self.code {
                HK_MOUSE_MIDDLE | HK_MOUSE_X1 | HK_MOUSE_X2 => Ok(()),
                _ => Err("鼠标只支持中键和两个侧键（左右键不能用作快捷键）"),
            },
            HK_KEY => {
                let vk = self.code;
                if vk == 0 || vk > 0xFE {
                    return Err("无效按键");
                }
                if is_modifier_vk(vk) {
                    return Err("请在修饰键之外再按一个按键");
                }
                if vk == VK_ESCAPE && self.mods == 0 {
                    return Err("单独的 Esc 用于取消录制，不能作为快捷键");
                }
                let strong = self.mods & (MOD_CTRL | MOD_ALT | MOD_WIN) != 0;
                if !strong && is_typing_vk(vk) {
                    return Err("字母、数字、空格、回车、方向键等普通输入键必须搭配 Ctrl、Alt 或 Win，\n否则会影响正常打字");
                }
                Ok(())
            }
            _ => Err("无效快捷键"),
        }
    }

    pub fn display(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.mods & MOD_CTRL != 0 { parts.push("Ctrl".into()); }
        if self.mods & MOD_ALT != 0 { parts.push("Alt".into()); }
        if self.mods & MOD_SHIFT != 0 { parts.push("Shift".into()); }
        if self.mods & MOD_WIN != 0 { parts.push("Win".into()); }
        parts.push(match self.kind {
            HK_MOUSE => match self.code {
                HK_MOUSE_MIDDLE => "鼠标中键".into(),
                HK_MOUSE_X1 => "鼠标侧键1(后退)".into(),
                HK_MOUSE_X2 => "鼠标侧键2(前进)".into(),
                c => format!("鼠标按键{c}"),
            },
            _ => vk_name(self.code),
        });
        parts.join(" + ")
    }
}

pub(super) fn is_modifier_vk(vk: u16) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

fn is_typing_vk(vk: u16) -> bool {
    matches!(vk,
        0x08 | 0x09 | 0x0D | 0x1B | 0x20 |   // Backspace Tab Enter Esc Space
        0x21..=0x28 | 0x2E |                 // PgUp PgDn End Home arrows Delete
        0x30..=0x39 | 0x41..=0x5A |          // digits letters
        0x60..=0x6F |                        // numpad
        0xBA..=0xC0 | 0xDB..=0xDF | 0xE2)    // OEM punctuation
}

fn vk_name(vk: u16) -> String {
    match vk {
        0x70..=0x87 => return format!("F{}", vk - 0x6F),
        0x13 => return "Pause".into(),
        0x91 => return "Scroll Lock".into(),
        0x2C => return "Print Screen".into(),
        0x5D => return "Menu".into(),
        _ => {}
    }
    let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x6F | 0x90);
    unsafe {
        let scan = MapVirtualKeyW(vk as u32, 0);
        if scan != 0 {
            let mut buf = [0u16; 64];
            let lparam = ((scan as i32) << 16) | if extended { 1 << 24 } else { 0 };
            let n = GetKeyNameTextW(lparam, buf.as_mut_ptr(), buf.len() as i32);
            if n > 0 {
                return String::from_utf16_lossy(&buf[..n as usize]);
            }
        }
    }
    format!("VK_{vk:02X}")
}

// ------------------------------------------------------------
// Live settings (read lock-free by hooks and workers)
// ------------------------------------------------------------

pub(super) static KVM_HOTKEY: AtomicU32 = AtomicU32::new(Hotkey::DEFAULT.pack());
pub(super) static CROSS_SCREEN_ON: AtomicBool = AtomicBool::new(false);
pub(super) static CROSS_SIDE: AtomicU32 = AtomicU32::new(0);
pub(super) static CROSS_OFFSET_MM: AtomicI32 = AtomicI32::new(0);
pub(super) static CROSS_DIAGONAL_TENTHS: AtomicU32 = AtomicU32::new(0);
pub(super) static SHARED_REVISION: AtomicU64 = AtomicU64::new(0);
pub(super) static SHARED_ORIGIN: AtomicU64 = AtomicU64::new(0);
static SHARED_SETTINGS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn shared_settings_lock() -> &'static Mutex<()> { SHARED_SETTINGS_LOCK.get_or_init(|| Mutex::new(())) }
pub(super) static CLIP_TEXT_ON: AtomicBool = AtomicBool::new(true);
pub(super) static CLIP_IMAGE_ON: AtomicBool = AtomicBool::new(true);
pub(super) static CLIP_FILES_ON: AtomicBool = AtomicBool::new(true);
pub(super) static KEEP_DISPLAY_ON: AtomicBool = AtomicBool::new(false);
pub(super) static NET_ENABLED: AtomicBool = AtomicBool::new(false);
pub(super) static NET_FIREWALL_OPEN: AtomicBool = AtomicBool::new(false);
pub(super) static NET_SUBNET: AtomicU32 = AtomicU32::new(pack_subnet(DEFAULT_SUBNET));
pub(super) const DEFAULT_SUBNET: [u8; 3] = [10, 77, 77];
// Internet sharing role of THIS PC over the virtual NIC. Each PC sets its own; not auto-mirrored.
pub(super) const NET_SHARE_OFF: u8 = 0;    // do not share, do not route through peer
pub(super) const NET_SHARE_PROVIDE: u8 = 1; // NAT the peer's traffic to my internet
pub(super) const NET_SHARE_CLIENT: u8 = 2;  // route my internet through the peer
pub(super) static NET_SHARE: AtomicU8 = AtomicU8::new(NET_SHARE_OFF);
pub(super) fn net_share_role() -> u8 { NET_SHARE.load(Ordering::Acquire).min(2) }

// FIX14: local-only Moonlight/VDD integration settings.
pub(super) static MOON_CONTROLLER_ON: AtomicBool = AtomicBool::new(false);
static VDD_INSTANCE_ID: OnceLock<Mutex<String>> = OnceLock::new();
static MOONLIGHT_START_CMD: OnceLock<Mutex<String>> = OnceLock::new();
const DEFAULT_VDD_INSTANCE: &str = r"ROOT\DISPLAY\0001";
const DEFAULT_MOONLIGHT_CMD: &str = r#""%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe" stream "{peer}" "Desktop""#;
fn vdd_instance_lock() -> &'static Mutex<String> { VDD_INSTANCE_ID.get_or_init(|| Mutex::new(DEFAULT_VDD_INSTANCE.to_string())) }
fn moonlight_cmd_lock() -> &'static Mutex<String> { MOONLIGHT_START_CMD.get_or_init(|| Mutex::new(DEFAULT_MOONLIGHT_CMD.to_string())) }
pub(super) fn moon_controller_enabled() -> bool { MOON_CONTROLLER_ON.load(Ordering::Acquire) }
pub(super) fn vdd_instance_id() -> String { vdd_instance_lock().lock().map(|x| x.clone()).unwrap_or_else(|e| e.into_inner().clone()) }
pub(super) fn moonlight_start_command() -> String { moonlight_cmd_lock().lock().map(|x| x.clone()).unwrap_or_else(|e| e.into_inner().clone()) }

const fn pack_subnet(s: [u8; 3]) -> u32 { (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32 }
pub(super) fn net_subnet() -> [u8; 3] {
    let v = NET_SUBNET.load(Ordering::Acquire);
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}
fn parse_subnet(v: &str) -> Option<[u8; 3]> {
    let parts: Vec<u8> = v.trim().trim_end_matches(".0").split('.').map(|x| x.trim().parse::<u8>()).collect::<Result<_, _>>().ok()?;
    if parts.len() != 3 { return None; }
    let s = [parts[0], parts[1], parts[2]];
    // Private ranges only, so the link can never shadow a real network route.
    let private = s[0] == 10 || (s[0] == 172 && (16..=31).contains(&s[1])) || (s[0] == 192 && s[1] == 168);
    private.then_some(s)
}

pub(super) fn current_hotkey() -> Hotkey {
    Hotkey::unpack(KVM_HOTKEY.load(Ordering::Acquire))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UserSettings {
    pub hotkey: Hotkey,
    pub cross_screen: bool,
    pub cross_side: u32,
    pub cross_offset_mm: i32,
    pub cross_diagonal_tenths: u32,
    pub shared_revision: u64,
    pub shared_origin: u64,
    pub clip_text: bool,
    pub clip_image: bool,
    pub clip_files: bool,
    pub keep_display: bool,
    pub net_enabled: bool,
    pub net_firewall_open: bool,
    pub net_subnet: [u8; 3],
    pub net_share: u8,
    pub moon_controller: bool,
    pub vdd_instance: String,
    pub moonlight_start_cmd: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self { hotkey: Hotkey::DEFAULT, cross_screen: false, cross_side: 0, cross_offset_mm: 0,
               cross_diagonal_tenths: 0, shared_revision: 0, shared_origin: 0,
               clip_text: true, clip_image: true, clip_files: true, keep_display: false,
               net_enabled: false, net_firewall_open: false, net_subnet: DEFAULT_SUBNET, net_share: NET_SHARE_OFF,
               moon_controller: false, vdd_instance: DEFAULT_VDD_INSTANCE.to_string(), moonlight_start_cmd: DEFAULT_MOONLIGHT_CMD.to_string() }
    }
}

impl UserSettings {
    pub fn current() -> Self {
        Self {
            hotkey: current_hotkey(),
            cross_screen: CROSS_SCREEN_ON.load(Ordering::Acquire),
            cross_side: CROSS_SIDE.load(Ordering::Acquire),
            cross_offset_mm: CROSS_OFFSET_MM.load(Ordering::Acquire),
            cross_diagonal_tenths: CROSS_DIAGONAL_TENTHS.load(Ordering::Acquire),
            shared_revision: SHARED_REVISION.load(Ordering::Acquire),
            shared_origin: SHARED_ORIGIN.load(Ordering::Acquire),
            clip_text: CLIP_TEXT_ON.load(Ordering::Acquire),
            clip_image: CLIP_IMAGE_ON.load(Ordering::Acquire),
            clip_files: CLIP_FILES_ON.load(Ordering::Acquire),
            keep_display: KEEP_DISPLAY_ON.load(Ordering::Acquire),
            net_enabled: NET_ENABLED.load(Ordering::Acquire),
            net_firewall_open: NET_FIREWALL_OPEN.load(Ordering::Acquire),
            net_subnet: net_subnet(),
            net_share: net_share_role(),
            moon_controller: moon_controller_enabled(),
            vdd_instance: vdd_instance_id(),
            moonlight_start_cmd: moonlight_start_command(),
        }
    }

    pub fn apply(&self) {
        KVM_HOTKEY.store(self.hotkey.pack(), Ordering::Release);
        CROSS_SCREEN_ON.store(self.cross_screen, Ordering::Release);
        CROSS_SIDE.store(self.cross_side, Ordering::Release);
        CROSS_OFFSET_MM.store(self.cross_offset_mm, Ordering::Release);
        CROSS_DIAGONAL_TENTHS.store(self.cross_diagonal_tenths, Ordering::Release);
        CLIP_TEXT_ON.store(self.clip_text, Ordering::Release);
        CLIP_IMAGE_ON.store(self.clip_image, Ordering::Release);
        CLIP_FILES_ON.store(self.clip_files, Ordering::Release);
        KEEP_DISPLAY_ON.store(self.keep_display, Ordering::Release);
        NET_ENABLED.store(self.net_enabled, Ordering::Release);
        NET_FIREWALL_OPEN.store(self.net_firewall_open, Ordering::Release);
        NET_SUBNET.store(pack_subnet(self.net_subnet), Ordering::Release);
        NET_SHARE.store(self.net_share.min(2), Ordering::Release);
        MOON_CONTROLLER_ON.store(self.moon_controller, Ordering::Release);
        if let Ok(mut g) = vdd_instance_lock().lock() { *g = self.vdd_instance.clone(); }
        if let Ok(mut g) = moonlight_cmd_lock().lock() { *g = self.moonlight_start_cmd.clone(); }
        SHARED_ORIGIN.store(self.shared_origin, Ordering::Release);
        SHARED_REVISION.store(self.shared_revision, Ordering::Release);
    }

    pub fn path() -> PathBuf {
        state_dir().join("settings.ini")
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        let (mut mods, mut kind, mut code) = (None, None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let flag = matches!(v, "1" | "true" | "yes" | "on");
            match k {
                "kvm_hotkey_mods" => mods = v.parse::<u8>().ok(),
                "kvm_hotkey_kind" => kind = v.parse::<u8>().ok(),
                "kvm_hotkey_code" => code = v.parse::<u16>().ok(),
                "cross_screen_enabled" => s.cross_screen = flag,
                "cross_side" => if let Ok(n) = v.parse::<u32>() { if n < 4 { s.cross_side = n; } },
                "cross_offset_mm" => if let Ok(n) = v.parse::<i32>() { if (-2000..=2000).contains(&n) { s.cross_offset_mm = n; } },
                "cross_diagonal_tenths" => if let Ok(n) = v.parse::<u32>() { if n == 0 || (100..=800).contains(&n) { s.cross_diagonal_tenths = n; } },
                "shared_revision" => s.shared_revision = v.parse::<u64>().unwrap_or(0),
                "shared_origin" => s.shared_origin = v.parse::<u64>().unwrap_or(0),
                "clip_text" => s.clip_text = flag,
                "clip_image" => s.clip_image = flag,
                "clip_files" => s.clip_files = flag,
                "keep_display_on" => s.keep_display = flag,
                "net_enabled" => s.net_enabled = flag,
                "net_firewall_open" => s.net_firewall_open = flag,
                "net_subnet" => if let Some(x) = parse_subnet(v) { s.net_subnet = x; },
                "net_share" => s.net_share = match v.to_ascii_lowercase().as_str() {
                    "provide" | "share" | "1" => NET_SHARE_PROVIDE,
                    "client" | "use" | "2" => NET_SHARE_CLIENT,
                    _ => NET_SHARE_OFF,
                },
                "moon_controller" => s.moon_controller = flag,
                "vdd_instance" => if !v.is_empty() && v.len() <= 256 { s.vdd_instance = v.to_string(); },
                "moonlight_start_cmd" => if !v.is_empty() && v.len() <= 2048 { s.moonlight_start_cmd = v.to_string(); },
                _ => {}
            }
        }
        if let (Some(mods), Some(kind), Some(code)) = (mods, kind, code) {
            let hk = Hotkey { mods, kind, code };
            if hk.validate().is_ok() {
                s.hotkey = hk;
            }
        }
        s
    }

    pub fn serialize(&self) -> String {
        format!(
            "# OTI-Link settings (edited by tray → 设置)\n\
             # kvm_hotkey = {}\n\
             kvm_hotkey_mods={}\nkvm_hotkey_kind={}\nkvm_hotkey_code={}\n\
             cross_screen_enabled={}\ncross_side={}\ncross_offset_mm={}\ncross_diagonal_tenths={}\nshared_revision={}\nshared_origin={}\n\
             clip_text={}\nclip_image={}\nclip_files={}\nkeep_display_on={}\n\
             # 虚拟网卡：net_subnet 只能是私有网段的前三段（本机/对端分别为 .1 / .2）\n\
             # net_share: off=不共享  provide=本机把网络共享给对端  client=本机通过对端上网\n\
             net_enabled={}\nnet_firewall_open={}\nnet_subnet={}.{}.{}\nnet_share={}\n\
             # Moonlight 副屏 / OTI 模式（本机设置，不同步）；moonlight_start_cmd 只在 B 电脑使用\n\
             moon_controller={}\nvdd_instance={}\nmoonlight_start_cmd={}\n",
            self.hotkey.display(),
            self.hotkey.mods, self.hotkey.kind, self.hotkey.code,
            self.cross_screen as u8, self.cross_side, self.cross_offset_mm, self.cross_diagonal_tenths,
            self.shared_revision, self.shared_origin,
            self.clip_text as u8, self.clip_image as u8, self.clip_files as u8, self.keep_display as u8,
            self.net_enabled as u8, self.net_firewall_open as u8,
            self.net_subnet[0], self.net_subnet[1], self.net_subnet[2],
            match self.net_share { NET_SHARE_PROVIDE => "provide", NET_SHARE_CLIENT => "client", _ => "off" },
            self.moon_controller as u8,
            self.vdd_instance.replace('\r', "").replace('\n', ""),
            self.moonlight_start_cmd.replace('\r', " ").replace('\n', " "),
        )
    }

    pub fn load(logger: &Logger) -> Self {
        match fs::read_to_string(Self::path()) {
            Ok(text) => {
                let mut s = Self::parse(&text);
                if (s.shared_revision == 0) != (s.shared_origin == 0) {
                    s.shared_revision = 0;
                    s.shared_origin = 0;
                }
                if (s.cross_screen || s.net_enabled) && (s.shared_revision == 0 || s.shared_origin == 0) {
                    s.shared_revision = 1;
                    s.shared_origin = node_id();
                }
                logln!(logger, "SETTINGS_LOADED {}", s.summary());
                s
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let s = Self::default();
                logln!(logger, "SETTINGS_DEFAULT {}", s.summary());
                s
            }
            Err(e) => {
                logln!(logger, "SETTINGS_READ_FAILED={e}; using defaults");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> io::Result<()> {
        fs::create_dir_all(state_dir())?;
        let path = Self::path();
        let tmp = path.with_extension("ini.tmp");
        fs::write(&tmp, self.serialize())?;
        fs::rename(&tmp, &path)
    }

    pub fn summary(&self) -> String {
        format!(
            "hotkey='{}' clip_text={} clip_image={} clip_files={} keep_display_on={} net={} net_firewall_open={} net_subnet={}.{}.{}.0/24 moon_controller={}",
            self.hotkey.display(), self.clip_text, self.clip_image, self.clip_files, self.keep_display,
            self.net_enabled, self.net_firewall_open, self.net_subnet[0], self.net_subnet[1], self.net_subnet[2], self.moon_controller
        )
    }
}

pub(super) fn shared_settings_frame(session: u64) -> CtrlFrame {
    let s = UserSettings::current();
    let mut p = Vec::with_capacity(27);
    p.push(1);
    p.extend_from_slice(&s.shared_revision.to_le_bytes());
    p.extend_from_slice(&s.shared_origin.to_le_bytes());
    p.push(s.cross_screen as u8);
    p.push(s.cross_side as u8);
    p.extend_from_slice(&s.cross_offset_mm.to_le_bytes());
    p.push(s.net_enabled as u8);
    p.extend_from_slice(&s.net_subnet);
    CtrlFrame::new(CTRL_SHARED_SETTINGS, session, 0, 0, 0, p)
}

pub(super) fn apply_peer_shared_settings(p: &[u8], logger: &Logger) {
    if p.len() != 27 || p[0] != 1 || p[17] > 1 || p[18] > 3 || p[23] > 1 { return; }
    let revision = u64::from_le_bytes(p[1..9].try_into().unwrap());
    let origin = u64::from_le_bytes(p[9..17].try_into().unwrap());
    let offset = i32::from_le_bytes(p[19..23].try_into().unwrap());
    let subnet = [p[24], p[25], p[26]];
    if (origin == 0) != (revision == 0) || !(-2000..=2000).contains(&offset)
        || parse_subnet(&format!("{}.{}.{}", subnet[0], subnet[1], subnet[2])) != Some(subnet) { return; }
    let _guard = shared_settings_lock().lock().unwrap_or_else(|e| e.into_inner());
    let before = UserSettings::current();
    if (revision, origin) <= (before.shared_revision, before.shared_origin) { return; }
    let mut next = before.clone();
    next.shared_revision = revision;
    next.shared_origin = origin;
    next.cross_screen = p[17] == 1;
    next.cross_side = (p[18] ^ 1) as u32;
    next.cross_offset_mm = -offset;
    next.net_enabled = p[23] == 1;
    next.net_subnet = subnet;
    next.apply();
    if next.cross_screen != before.cross_screen || next.cross_side != before.cross_side
        || next.cross_offset_mm != before.cross_offset_mm {
        kvm_set_target(false);
        cross_screen::cancel();
    }
    if next.net_enabled && !before.net_enabled { net_start(); }
    if !next.net_enabled && before.net_enabled { net_stop(); }
    if next.net_subnet != before.net_subnet { net_config_refresh(); }
    match next.save() {
        Ok(()) => logln!(logger, "SHARED_SETTINGS_APPLIED revision={} origin={:016X} cross={} side={} offset_mm={} net={} subnet={}.{}.{}.0/24",
            revision, origin, next.cross_screen, next.cross_side, next.cross_offset_mm,
            next.net_enabled, subnet[0], subnet[1], subnet[2]),
        Err(e) => logln!(logger, "SHARED_SETTINGS_SAVE_FAILED={e}"),
    }
    let hwnd = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() { unsafe { PostMessageW(hwnd, WM_SHARED_SETTINGS_UPDATED, 0, 0); } }
}

// ------------------------------------------------------------
// Settings window (tray thread only)
// ------------------------------------------------------------

pub(super) const WM_KVM_RECORDED: u32 = WM_APP + 20;
pub(super) const WM_KVM_RECORD_CANCEL: u32 = WM_APP + 21;
pub(super) const WM_SHARED_SETTINGS_UPDATED: u32 = WM_APP + 22;
pub(super) const WM_WORKFLOW_UPDATED: u32 = WM_APP + 23;

/// Window that receives the next recorded key/button (0 = not recording).
/// Written by the settings window, consumed by the KVM hook thread.
pub(super) static RECORD_HWND: AtomicUsize = AtomicUsize::new(0);
pub(super) static SETTINGS_HWND: AtomicUsize = AtomicUsize::new(0);

const WS_CHILD: u32 = 0x4000_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_GROUP: u32 = 0x0002_0000;
const WS_EX_DLGMODALFRAME: u32 = 0x0000_0001;
const BS_DEFPUSHBUTTON: u32 = 1;
const BS_AUTOCHECKBOX: u32 = 3;
const BS_GROUPBOX: u32 = 7;
const BS_AUTORADIOBUTTON: u32 = 9;
const WS_BORDER: u32 = 0x0080_0000;
const WM_CLOSE: u32 = 0x0010;
const WM_COMMAND: u32 = 0x0111;
const WM_SETFONT: u32 = 0x0030;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: usize = 1;
const BN_CLICKED: u32 = 0;
const SW_SHOW: i32 = 5;
const COLOR_BTNFACE: usize = 15;
const IDC_ARROW: usize = 32512;
const MB_ICONWARNING: u32 = 0x30;
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

const IDOK: i32 = 1;
const IDCANCEL: i32 = 2;
const IDC_HK_TEXT: i32 = 101;
const IDC_RECORD: i32 = 102;
const IDC_DEFAULT: i32 = 103;
const IDC_CLIP_TEXT: i32 = 104;
const IDC_CLIP_IMAGE: i32 = 105;
const IDC_CLIP_FILES: i32 = 106;
const IDC_KEEP_DISPLAY: i32 = 107;
const IDC_NET_ENABLE: i32 = 108;
const IDC_NET_FIREWALL: i32 = 109;
const IDC_NET_STATUS: i32 = 110;
const IDC_NET_SHARE_OFF: i32 = 130;
const IDC_NET_SHARE_PROVIDE: i32 = 131;
const IDC_NET_SHARE_CLIENT: i32 = 132;
const IDC_CROSS_ENABLE: i32 = 111;
const IDC_CROSS_RIGHT: i32 = 112;
const IDC_CROSS_LEFT: i32 = 113;
const IDC_CROSS_DOWN: i32 = 114;
const IDC_CROSS_UP: i32 = 115;
const IDC_CROSS_DIAGONAL: i32 = 116;
const IDC_CROSS_OFFSET: i32 = 117;
const IDC_CROSS_LAYOUT: i32 = 118;
const IDC_MOON_CONTROLLER: i32 = 140;
const IDC_MODE_MOONLIGHT: i32 = 141;
const IDC_MODE_OTI: i32 = 142;
const IDC_VDD_START: i32 = 143;
const IDC_WORKFLOW_STATUS: i32 = 145;
const IDC_VDD_INSTANCE: i32 = 146;
const IDC_MOONLIGHT_CMD: i32 = 147;
const LAYOUT_W: i32 = 520;
const LAYOUT_H: i32 = 340;
const WM_PAINT: u32 = 0x000f;

const CLIENT_W: i32 = 460;
const CLIENT_H: i32 = 902;

struct WindowState {
    draft_hotkey: Hotkey,
    font: HANDLE,
}
thread_local! { static WINDOW_STATE: RefCell<Option<WindowState>> = const { RefCell::new(None) }; }
thread_local! { static LAYOUT_DRAG: RefCell<Option<(i32, i32)>> = const { RefCell::new(None) }; }
static LAYOUT_HWND: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct PAINTSTRUCT { hdc: HANDLE, erase: BOOL, paint: RECT, restore: BOOL, update: BOOL, reserved: [u8; 32] }
#[link(name = "user32")]
unsafe extern "system" {
    fn BeginPaint(hwnd: HWND, ps: *mut PAINTSTRUCT) -> HANDLE;
    fn EndPaint(hwnd: HWND, ps: *const PAINTSTRUCT) -> BOOL;
    fn InvalidateRect(hwnd: HWND, rect: *const RECT, erase: BOOL) -> BOOL;
    fn SetCapture(hwnd: HWND) -> HWND;
    fn ReleaseCapture() -> BOOL;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn Rectangle(dc: HANDLE, left: i32, top: i32, right: i32, bottom: i32) -> BOOL;
    fn TextOutW(dc: HANDLE, x: i32, y: i32, text: *const u16, len: i32) -> BOOL;
    fn SetBkMode(dc: HANDLE, mode: i32) -> i32;
}

fn layout_side(parent: HWND) -> u32 {
    unsafe { if is_checked(parent, IDC_CROSS_LEFT) { 1 } else if is_checked(parent, IDC_CROSS_DOWN) { 2 }
        else if is_checked(parent, IDC_CROSS_UP) { 3 } else { 0 } }
}
fn layout_offset(parent: HWND) -> i32 {
    unsafe {
        let mut buf = [0u16; 32];
        let n = GetWindowTextW(item(parent, IDC_CROSS_OFFSET), buf.as_mut_ptr(), 32).max(0) as usize;
        String::from_utf16_lossy(&buf[..n]).trim().parse().unwrap_or(0)
    }
}
fn layout_rects(parent: HWND) -> (RECT, RECT, f64) {
    let local = cross_screen::local_display();
    let peer = cross_screen::peer_display();
    let (lw, lh) = local.map(|d| (d.width_mm, d.height_mm)).unwrap_or((531, 299));
    let (rw, rh) = peer.map(|d| (d.width_mm, d.height_mm)).unwrap_or((598, 336));
    let scale = 100.0 / lw.max(1) as f64;
    let local = RECT { left: 190, top: 130, right: 290, bottom: 130 + (lh as f64 * scale).round() as i32 };
    let w = (rw as f64 * scale).round().max(20.0) as i32;
    let h = (rh as f64 * scale).round().max(20.0) as i32;
    let shift = (layout_offset(parent) as f64 * scale).round() as i32;
    let (x, y) = match layout_side(parent) {
        1 => (local.left - w, local.top + shift),
        2 => (local.left + shift, local.bottom),
        3 => (local.left + shift, local.top - h),
        _ => (local.right, local.top + shift),
    };
    (local, RECT { left: x, top: y, right: x + w, bottom: y + h }, scale)
}
fn layout_mouse(l: LPARAM) -> (i32, i32) { ((l as u32 as u16 as i16) as i32, ((l as u32 >> 16) as u16 as i16) as i32) }
unsafe extern "system" fn layout_wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let parent = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let dc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !dc.is_null() && !parent.is_null() {
                let (a, b, _) = layout_rects(parent);
                unsafe {
                    SetBkMode(dc, 1);
                    Rectangle(dc, a.left, a.top, a.right, a.bottom);
                    Rectangle(dc, b.left, b.top, b.right, b.bottom);
                }
                for (x, y, label) in [(a.left + 12, a.top + 12, "1 本机"), (b.left + 12, b.top + 12, "2 对端"),
                    (20, 20, "拖动对端显示器，使两块屏幕的相邻边贴合。") ] {
                    let t = wide_null(label);
                    unsafe { TextOutW(dc, x, y, t.as_ptr(), t.len() as i32 - 1); }
                }
            }
            unsafe { EndPaint(hwnd, &ps); }
            0
        }
        WM_LBUTTONDOWN => {
            if !parent.is_null() {
                let (_, b, _) = layout_rects(parent);
                let (x, y) = layout_mouse(l);
                if x >= b.left && x < b.right && y >= b.top && y < b.bottom {
                    LAYOUT_DRAG.with(|d| *d.borrow_mut() = Some((x - b.left, y - b.top)));
                    unsafe { SetCapture(hwnd); }
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            if !parent.is_null() {
                if let Some((grab_x, grab_y)) = LAYOUT_DRAG.with(|d| *d.borrow()) {
                    let (a, b, scale) = layout_rects(parent);
                    let (x, y) = layout_mouse(l);
                    let desired_x = x - grab_x;
                    let desired_y = y - grab_y;
                    let cx = desired_x + (b.right - b.left) / 2 - (a.left + a.right) / 2;
                    let cy = desired_y + (b.bottom - b.top) / 2 - (a.top + a.bottom) / 2;
                    let side = if cx.abs() >= cy.abs() { if cx >= 0 { 0 } else { 1 } }
                        else if cy >= 0 { 2 } else { 3 };
                    let offset = if side < 2 { desired_y - a.top } else { desired_x - a.left };
                    for i in 0..4 { unsafe { set_checked(parent, IDC_CROSS_RIGHT + i, side == i); } }
                    unsafe { set_item_text(parent, IDC_CROSS_OFFSET,
                        &((offset as f64 / scale).round() as i32).clamp(-2000, 2000).to_string());
                        InvalidateRect(hwnd, null(), 1); }
                }
            }
            0
        }
        WM_LBUTTONUP => { LAYOUT_DRAG.with(|d| *d.borrow_mut() = None); unsafe { ReleaseCapture(); } 0 }
        WM_CLOSE => { unsafe { DestroyWindow(hwnd); } 0 }
        WM_DESTROY => { LAYOUT_HWND.store(0, Ordering::Release); 0 }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

fn open_layout_window(parent: HWND) {
    unsafe {
        let old = LAYOUT_HWND.load(Ordering::Acquire) as HWND;
        if !old.is_null() && IsWindow(old) != 0 { SetForegroundWindow(old); return; }
        let class = wide_null("OTI_Link_Cross_Layout");
        let wc = WNDCLASSW { style: 0, lpfnWndProc: Some(layout_wnd_proc), cbClsExtra: 0, cbWndExtra: 0,
            hInstance: GetModuleHandleW(null()), hIcon: null_mut(), hCursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
            hbrBackground: (COLOR_BTNFACE + 1) as HANDLE, lpszMenuName: null(), lpszClassName: class.as_ptr() };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS { return; }
        let title = wide_null("跨屏显示器布局");
        let hwnd = CreateWindowExW(WS_EX_DLGMODALFRAME, class.as_ptr(), title.as_ptr(), WS_CAPTION | WS_SYSMENU,
            120, 120, LAYOUT_W, LAYOUT_H, parent, null_mut(), GetModuleHandleW(null()), null_mut());
        if !hwnd.is_null() { LAYOUT_HWND.store(hwnd as usize, Ordering::Release); ShowWindow(hwnd, SW_SHOW); }
    }
}

pub(super) fn settings_logger() -> Option<Logger> {
    tray_context().lock().ok().and_then(|g| g.as_ref().map(|c| c.logger.clone()))
}

unsafe fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(hwnd, id) }
}

unsafe fn set_item_text(hwnd: HWND, id: i32, text: &str) {
    let w = wide_null(text);
    unsafe { SetWindowTextW(item(hwnd, id), w.as_ptr()); }
}

unsafe fn is_checked(hwnd: HWND, id: i32) -> bool {
    unsafe { SendMessageW(item(hwnd, id), BM_GETCHECK, 0, 0) as usize == BST_CHECKED }
}

unsafe fn set_checked(hwnd: HWND, id: i32, on: bool) {
    unsafe { SendMessageW(item(hwnd, id), BM_SETCHECK, if on { BST_CHECKED } else { 0 }, 0); }
}

unsafe fn control(parent: HWND, font: HANDLE, class: &str, text: &str, style: u32,
                  x: i32, y: i32, w: i32, h: i32, id: i32) -> HWND {
    let c = wide_null(class);
    let t = wide_null(text);
    unsafe {
        let hwnd = CreateWindowExW(0, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style,
            x, y, w, h, parent, id as usize as HMENU, GetModuleHandleW(null()), null_mut());
        if !hwnd.is_null() && !font.is_null() {
            SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
        }
        hwnd
    }
}

fn show_hotkey(hwnd: HWND, hk: Hotkey) {
    unsafe { set_item_text(hwnd, IDC_HK_TEXT, &hk.display()); }
}

fn stop_recording(hwnd: HWND) {
    let _ = RECORD_HWND.compare_exchange(hwnd as usize, 0, Ordering::AcqRel, Ordering::Acquire);
    unsafe {
        EnableWindow(item(hwnd, IDC_RECORD), 1);
        EnableWindow(item(hwnd, IDC_DEFAULT), 1);
        EnableWindow(item(hwnd, IDOK), 1);
    }
}

fn draft_hotkey() -> Hotkey {
    WINDOW_STATE.with(|s| s.borrow().as_ref().map(|w| w.draft_hotkey).unwrap_or_else(current_hotkey))
}

fn set_draft_hotkey(hwnd: HWND, hk: Hotkey) {
    WINDOW_STATE.with(|s| if let Some(w) = s.borrow_mut().as_mut() { w.draft_hotkey = hk; });
    show_hotkey(hwnd, hk);
}

unsafe fn message_box(hwnd: HWND, text: &str, flags: u32) {
    let t = wide_null(text);
    let c = wide_null("OTI-Link 设置");
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), flags); }
}

fn read_item_text(hwnd: HWND, id: i32, max: usize) -> String {
    unsafe {
        let mut buf = vec![0u16; max.max(2)];
        let n = GetWindowTextW(item(hwnd, id), buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buf[..n]).trim().to_string()
    }
}

fn workflow_status_line() -> String {
    format!("{}：{}{}", workflow_mode_label(), workflow_status_text().replace('\n', " "), if workflow_busy() { "（处理中）" } else { "" })
}

fn apply_workflow_draft(hwnd: HWND) {
    unsafe { MOON_CONTROLLER_ON.store(is_checked(hwnd, IDC_MOON_CONTROLLER), Ordering::Release); }
    let vdd = read_item_text(hwnd, IDC_VDD_INSTANCE, 257);
    let moon = read_item_text(hwnd, IDC_MOONLIGHT_CMD, 2049);
    if !vdd.is_empty() { if let Ok(mut g) = vdd_instance_lock().lock() { *g = vdd; } }
    if !moon.is_empty() { if let Ok(mut g) = moonlight_cmd_lock().lock() { *g = moon; } }
}

fn save_and_close(hwnd: HWND) {
    let read_i32 = |id| unsafe {
        let mut buf = [0u16; 32];
        let n = GetWindowTextW(item(hwnd, id), buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        String::from_utf16_lossy(&buf[..n]).trim().parse::<i32>().ok()
    };
    let diagonal = read_i32(IDC_CROSS_DIAGONAL).unwrap_or(0);
    let offset = read_i32(IDC_CROSS_OFFSET).unwrap_or(0);
    if diagonal != 0 && !(100..=800).contains(&diagonal) || !(-2000..=2000).contains(&offset) {
        unsafe { message_box(hwnd, "尺寸填 0 自动检测，或 100–800（英寸×10）；错位须为 -2000–2000 毫米。", MB_OK | MB_ICONWARNING); }
        return;
    }
    let mut settings = unsafe {
        UserSettings {
            hotkey: draft_hotkey(),
            shared_revision: 0,
            shared_origin: 0,
            cross_screen: is_checked(hwnd, IDC_CROSS_ENABLE),
            cross_side: if is_checked(hwnd, IDC_CROSS_LEFT) { 1 } else if is_checked(hwnd, IDC_CROSS_DOWN) { 2 }
                else if is_checked(hwnd, IDC_CROSS_UP) { 3 } else { 0 },
            cross_offset_mm: offset,
            cross_diagonal_tenths: diagonal as u32,
            clip_text: is_checked(hwnd, IDC_CLIP_TEXT),
            clip_image: is_checked(hwnd, IDC_CLIP_IMAGE),
            clip_files: is_checked(hwnd, IDC_CLIP_FILES),
            keep_display: is_checked(hwnd, IDC_KEEP_DISPLAY),
            net_enabled: is_checked(hwnd, IDC_NET_ENABLE),
            net_firewall_open: is_checked(hwnd, IDC_NET_FIREWALL),
            net_subnet: net_subnet(),
            net_share: if is_checked(hwnd, IDC_NET_SHARE_PROVIDE) { NET_SHARE_PROVIDE }
                       else if is_checked(hwnd, IDC_NET_SHARE_CLIENT) { NET_SHARE_CLIENT }
                       else { NET_SHARE_OFF },
            moon_controller: is_checked(hwnd, IDC_MOON_CONTROLLER),
            vdd_instance: read_item_text(hwnd, IDC_VDD_INSTANCE, 257),
            moonlight_start_cmd: read_item_text(hwnd, IDC_MOONLIGHT_CMD, 2049),
        }
    };
    if settings.vdd_instance.is_empty() { settings.vdd_instance = DEFAULT_VDD_INSTANCE.to_string(); }
    if settings.moonlight_start_cmd.is_empty() { settings.moonlight_start_cmd = DEFAULT_MOONLIGHT_CMD.to_string(); }
    let _guard = shared_settings_lock().lock().unwrap_or_else(|e| e.into_inner());
    let before = UserSettings::current();
    if let Err(e) = settings.hotkey.validate() {
        unsafe { message_box(hwnd, e, MB_OK | MB_ICONWARNING); }
        return;
    }
    let shared_changed = settings.cross_screen != before.cross_screen || settings.cross_side != before.cross_side
        || settings.cross_offset_mm != before.cross_offset_mm || settings.net_enabled != before.net_enabled
        || settings.net_subnet != before.net_subnet;
    settings.shared_revision = before.shared_revision.saturating_add(shared_changed as u64);
    settings.shared_origin = if shared_changed { node_id() } else { before.shared_origin };
    settings.apply();
    cross_screen::invalidate_display();
    if settings.cross_screen != before.cross_screen || settings.cross_side != before.cross_side
        || settings.cross_offset_mm != before.cross_offset_mm
        || settings.cross_diagonal_tenths != before.cross_diagonal_tenths {
        kvm_set_target(false);
        cross_screen::cancel();
        if let Ok(slot) = hook_slot().lock() {
            if let Some(s) = slot.as_ref() {
                if let Some(frame) = cross_screen::display_frame(s.session) { let _ = s.tx.send(frame); }
            }
        }
    }
    // net_start() is idempotent; calling it on every save also retries a start
    // that failed earlier (e.g. the UAC prompt was declined).
    if settings.net_enabled { net_start(); }
    if !settings.net_enabled && before.net_enabled { net_stop(); }
    if settings.net_firewall_open != before.net_firewall_open { net_config_refresh(); }
    if settings.net_share != before.net_share { net_config_refresh(); net_share_role_changed(before.net_share); }
    let logger = settings_logger();
    match settings.save() {
        Ok(()) => { if let Some(l) = &logger { logln!(l, "SETTINGS_SAVED {}", settings.summary()); } }
        Err(e) => {
            if let Some(l) = &logger { logln!(l, "SETTINGS_SAVE_FAILED={e}"); }
            unsafe { message_box(hwnd, &format!("设置已生效，但保存到文件失败：\n{e}"), MB_OK | MB_ICONWARNING); }
        }
    }
    if shared_changed {
        if let Ok(slot) = hook_slot().lock() {
            if let Some(s) = slot.as_ref().filter(|s| s.connected.load(Ordering::Acquire)) {
                let _ = s.tx.send(shared_settings_frame(s.session));
            }
        }
    }
    unsafe { DestroyWindow(hwnd); }
}

unsafe extern "system" fn settings_wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (w & 0xFFFF) as i32;
            let code = ((w >> 16) & 0xFFFF) as u32;
            if code == BN_CLICKED {
                match id {
                    IDC_RECORD => {
                        if KVM_HOOK_THREAD_ID.load(Ordering::Acquire) == 0 {
                            unsafe { message_box(hwnd, "KVM 键盘鼠标钩子没有运行，无法录制快捷键。", MB_OK | MB_ICONWARNING); }
                        } else {
                            RECORD_HWND.store(hwnd as usize, Ordering::Release);
                            unsafe {
                                set_item_text(hwnd, IDC_HK_TEXT, "请按下新的快捷键或鼠标中键/侧键…（Esc 取消）");
                                EnableWindow(item(hwnd, IDC_RECORD), 0);
                                EnableWindow(item(hwnd, IDC_DEFAULT), 0);
                                EnableWindow(item(hwnd, IDOK), 0);
                            }
                        }
                    }
                    IDC_DEFAULT => set_draft_hotkey(hwnd, Hotkey::DEFAULT),
                    IDC_CROSS_LAYOUT => open_layout_window(hwnd),
                    IDC_MODE_MOONLIGHT => { apply_workflow_draft(hwnd); workflow_request_moonlight(); },
                    IDC_MODE_OTI => { apply_workflow_draft(hwnd); workflow_request_oti(); },
                    IDC_VDD_START => { apply_workflow_draft(hwnd); workflow_start_vdd(); },
                    IDOK => save_and_close(hwnd),
                    IDCANCEL => unsafe { DestroyWindow(hwnd); },
                    _ => {}
                }
            }
            0
        }
        WM_KVM_RECORDED => {
            stop_recording(hwnd);
            let hk = Hotkey::unpack(w as u32);
            match hk.validate() {
                Ok(()) => set_draft_hotkey(hwnd, hk),
                Err(e) => {
                    show_hotkey(hwnd, draft_hotkey());
                    unsafe { message_box(hwnd, &format!("“{}” 不能用作切换快捷键：\n{e}", hk.display()), MB_OK | MB_ICONWARNING); }
                }
            }
            0
        }
        WM_KVM_RECORD_CANCEL => {
            stop_recording(hwnd);
            show_hotkey(hwnd, draft_hotkey());
            0
        }
        WM_SHARED_SETTINGS_UPDATED => {
            let current = UserSettings::current();
            unsafe {
                set_checked(hwnd, IDC_CROSS_ENABLE, current.cross_screen);
                for i in 0..4 { set_checked(hwnd, IDC_CROSS_RIGHT + i, current.cross_side as i32 == i); }
                set_item_text(hwnd, IDC_CROSS_OFFSET, &current.cross_offset_mm.to_string());
                set_checked(hwnd, IDC_NET_ENABLE, current.net_enabled);
                let sn = current.net_subnet;
                set_item_text(hwnd, IDC_NET_STATUS, &format!("网段 {}.{}.{}.0/24　状态：{}",
                    sn[0], sn[1], sn[2], net_status_text()));
                let layout = LAYOUT_HWND.load(Ordering::Acquire) as HWND;
                if !layout.is_null() { InvalidateRect(layout, null(), 1); }
            }
            0
        }
        WM_WORKFLOW_UPDATED => {
            unsafe {
                set_item_text(hwnd, IDC_WORKFLOW_STATUS, &workflow_status_line());
            }
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd); }
            0
        }
        WM_DESTROY => {
            let layout = LAYOUT_HWND.swap(0, Ordering::AcqRel) as HWND;
            if !layout.is_null() { unsafe { DestroyWindow(layout); } }
            let _ = RECORD_HWND.compare_exchange(hwnd as usize, 0, Ordering::AcqRel, Ordering::Acquire);
            SETTINGS_HWND.store(0, Ordering::Release);
            WINDOW_STATE.with(|s| {
                if let Some(state) = s.borrow_mut().take() {
                    if !state.font.is_null() { unsafe { DeleteObject(state.font); } }
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// Opens (or focuses) the settings window. Must run on the tray thread, whose
/// message loop routes keyboard navigation through `IsDialogMessageW`.
pub(super) fn open_settings_window() {
    unsafe {
        let existing = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
        if !existing.is_null() && IsWindow(existing) != 0 {
            ShowWindow(existing, SW_SHOW);
            SetForegroundWindow(existing);
            return;
        }

        let class = wide_null("OTI_Link_Settings_Window");
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            style: 0, lpfnWndProc: Some(settings_wnd_proc), cbClsExtra: 0, cbWndExtra: 0,
            hInstance: instance, hIcon: load_embedded_icon(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW as *const u16),
            hbrBackground: (COLOR_BTNFACE + 1) as HANDLE,
            lpszMenuName: null(), lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            if let Some(l) = settings_logger() { logln!(l, "SETTINGS_WINDOW_CLASS_FAILED win32={}", GetLastError()); }
            return;
        }

        let style = WS_CAPTION | WS_SYSMENU;
        let mut rect = RECT { left: 0, top: 0, right: CLIENT_W, bottom: CLIENT_H };
        AdjustWindowRectEx(&mut rect, style, 0, WS_EX_DLGMODALFRAME);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let x = ((GetSystemMetrics(0) - w) / 2).max(0);
        let y = ((GetSystemMetrics(1) - h) / 2).max(0);
        let title = wide_null("OTI-Link 设置");
        let hwnd = CreateWindowExW(WS_EX_DLGMODALFRAME, class.as_ptr(), title.as_ptr(), style,
            x, y, w, h, null_mut(), null_mut(), instance, null_mut());
        if hwnd.is_null() {
            if let Some(l) = settings_logger() { logln!(l, "SETTINGS_WINDOW_CREATE_FAILED win32={}", GetLastError()); }
            return;
        }

        let face = wide_null("Microsoft YaHei UI");
        let font = CreateFontW(-13, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
        let current = UserSettings::current();
        WINDOW_STATE.with(|s| *s.borrow_mut() = Some(WindowState { draft_hotkey: current.hotkey, font }));
        SETTINGS_HWND.store(hwnd as usize, Ordering::Release);

        control(hwnd, font, "BUTTON", "KVM 键盘鼠标切换", BS_GROUPBOX, 12, 8, 436, 136, -1);
        control(hwnd, font, "STATIC", "切换快捷键：", 0, 28, 36, 90, 20, -1);
        control(hwnd, font, "STATIC", &current.hotkey.display(), 0, 118, 36, 316, 20, IDC_HK_TEXT);
        control(hwnd, font, "BUTTON", "录制新快捷键…", WS_TABSTOP | WS_GROUP, 28, 64, 150, 28, IDC_RECORD);
        control(hwnd, font, "BUTTON", "恢复默认", WS_TABSTOP, 188, 64, 110, 28, IDC_DEFAULT);
        control(hwnd, font, "STATIC",
            "录制时按下：修饰键(Ctrl/Alt/Shift/Win) + 任意键，或鼠标中键/侧键（可配合修饰键）。\n快捷键只在连接到对端时生效；未连接时按键照常传给本机程序。",
            0, 28, 100, 412, 38, -1);

        control(hwnd, font, "BUTTON", "鼠标跨屏切换", BS_GROUPBOX, 12, 152, 436, 190, -1);
        control(hwnd, font, "BUTTON", "启用鼠标跨屏切换（自动同步到对端）", BS_AUTOCHECKBOX | WS_TABSTOP | WS_GROUP,
            28, 176, 404, 22, IDC_CROSS_ENABLE);
        control(hwnd, font, "STATIC", "对端位于本机：", 0, 28, 204, 110, 20, -1);
        control(hwnd, font, "BUTTON", "右", BS_AUTORADIOBUTTON | WS_TABSTOP | WS_GROUP, 144, 203, 58, 22, IDC_CROSS_RIGHT);
        control(hwnd, font, "BUTTON", "左", BS_AUTORADIOBUTTON | WS_TABSTOP, 204, 203, 58, 22, IDC_CROSS_LEFT);
        control(hwnd, font, "BUTTON", "下", BS_AUTORADIOBUTTON | WS_TABSTOP, 264, 203, 58, 22, IDC_CROSS_DOWN);
        control(hwnd, font, "BUTTON", "上", BS_AUTORADIOBUTTON | WS_TABSTOP, 324, 203, 58, 22, IDC_CROSS_UP);
        control(hwnd, font, "STATIC", "本机尺寸（英寸×10）：", 0, 28, 234, 170, 20, -1);
        control(hwnd, font, "EDIT", &current.cross_diagonal_tenths.to_string(), WS_BORDER | WS_TABSTOP,
            208, 232, 66, 24, IDC_CROSS_DIAGONAL);
        control(hwnd, font, "STATIC", "0 自动；27 英寸填 270", 0, 285, 234, 145, 20, -1);
        control(hwnd, font, "STATIC", "对端错位（毫米）：", 0, 28, 266, 170, 20, -1);
        control(hwnd, font, "EDIT", &current.cross_offset_mm.to_string(), WS_BORDER | WS_TABSTOP,
            208, 264, 66, 24, IDC_CROSS_OFFSET);
        control(hwnd, font, "BUTTON", "拖动布局…", WS_TABSTOP, 290, 264, 130, 26, IDC_CROSS_LAYOUT);
        control(hwnd, font, "STATIC", "右/左为上下错位；上/下为左右错位。\n跨机复制文件请使用剪贴板复制/粘贴。", 0, 28, 298, 398, 34, -1);

        control(hwnd, font, "BUTTON", "剪贴板同步（本机收发都受此控制）", BS_GROUPBOX, 12, 350, 436, 58, -1);
        control(hwnd, font, "BUTTON", "文本", BS_AUTOCHECKBOX | WS_TABSTOP | WS_GROUP, 28, 374, 110, 22, IDC_CLIP_TEXT);
        control(hwnd, font, "BUTTON", "图片", BS_AUTOCHECKBOX | WS_TABSTOP, 158, 374, 110, 22, IDC_CLIP_IMAGE);
        control(hwnd, font, "BUTTON", "文件（资源管理器复制）", BS_AUTOCHECKBOX | WS_TABSTOP, 288, 374, 156, 22, IDC_CLIP_FILES);

        control(hwnd, font, "BUTTON", "电源", BS_GROUPBOX, 12, 416, 436, 52, -1);
        control(hwnd, font, "BUTTON", "始终保持显示器常亮（默认仅在 KVM 控制对端时常亮）",
            BS_AUTOCHECKBOX | WS_TABSTOP | WS_GROUP, 28, 438, 412, 22, IDC_KEEP_DISPLAY);

        control(hwnd, font, "BUTTON", "Moonlight 副屏 / OTI 模式", BS_GROUPBOX, 12, 476, 436, 200, -1);
        control(hwnd, font, "BUTTON", &format!("本机是 A 电脑（Sunshine + VDD；{WORKFLOW_HOTKEY_TEXT} 切换两种模式）"),
            BS_AUTOCHECKBOX | WS_TABSTOP | WS_GROUP, 28, 498, 412, 22, IDC_MOON_CONTROLLER);
        control(hwnd, font, "BUTTON", "Moonlight 副屏模式", WS_TABSTOP, 28, 526, 136, 28, IDC_MODE_MOONLIGHT);
        control(hwnd, font, "BUTTON", "OTI 模式", WS_TABSTOP, 172, 526, 104, 28, IDC_MODE_OTI);
        control(hwnd, font, "BUTTON", "启动虚拟显示器", WS_TABSTOP, 284, 526, 144, 28, IDC_VDD_START);
        control(hwnd, font, "STATIC", "VDD 实例 ID：", 0, 28, 562, 90, 20, -1);
        control(hwnd, font, "EDIT", &current.vdd_instance, WS_BORDER | WS_TABSTOP, 116, 558, 312, 24, IDC_VDD_INSTANCE);
        control(hwnd, font, "STATIC", "B 端 Moonlight 启动命令（{peer}=A电脑名）：", 0, 28, 590, 310, 20, -1);
        control(hwnd, font, "EDIT", &current.moonlight_start_cmd, WS_BORDER | WS_TABSTOP, 28, 610, 400, 24, IDC_MOONLIGHT_CMD);
        control(hwnd, font, "STATIC", &workflow_status_line(), 0, 28, 638, 412, 34, IDC_WORKFLOW_STATUS);

        control(hwnd, font, "BUTTON", "虚拟网卡（两台电脑之间的专用网络）", BS_GROUPBOX, 12, 684, 436, 168, -1);
        control(hwnd, font, "BUTTON", "启用虚拟网卡（两端同步；需管理员授权和 wintun.dll）",
            BS_AUTOCHECKBOX | WS_TABSTOP | WS_GROUP, 28, 706, 412, 22, IDC_NET_ENABLE);
        control(hwnd, font, "BUTTON", "允许对端访问本机所有端口（防火墙对虚拟网卡全部放行）",
            BS_AUTOCHECKBOX | WS_TABSTOP, 28, 730, 412, 22, IDC_NET_FIREWALL);
        control(hwnd, font, "STATIC", "网络共享（本机独立设置，不随对端同步）：", 0, 28, 758, 412, 18, -1);
        control(hwnd, font, "BUTTON", "不共享", BS_AUTORADIOBUTTON | WS_TABSTOP | WS_GROUP, 40, 778, 96, 22, IDC_NET_SHARE_OFF);
        control(hwnd, font, "BUTTON", "把本机网络共享给对端", BS_AUTORADIOBUTTON | WS_TABSTOP, 140, 778, 150, 22, IDC_NET_SHARE_PROVIDE);
        control(hwnd, font, "BUTTON", "通过对端上网", BS_AUTORADIOBUTTON | WS_TABSTOP, 300, 778, 130, 22, IDC_NET_SHARE_CLIENT);
        let sn = current.net_subnet;
        control(hwnd, font, "STATIC",
            &format!("网段 {}.{}.{}.0/24　状态：{}", sn[0], sn[1], sn[2], net_status_text()),
            0, 28, 808, 412, 34, IDC_NET_STATUS);

        control(hwnd, font, "BUTTON", "保存", BS_DEFPUSHBUTTON | WS_TABSTOP | WS_GROUP, 256, 862, 90, 30, IDOK);
        control(hwnd, font, "BUTTON", "取消", WS_TABSTOP, 358, 862, 90, 30, IDCANCEL);

        set_checked(hwnd, IDC_CROSS_ENABLE, current.cross_screen);
        set_checked(hwnd, IDC_CROSS_RIGHT + current.cross_side as i32, true);

        set_checked(hwnd, IDC_CLIP_TEXT, current.clip_text);
        set_checked(hwnd, IDC_CLIP_IMAGE, current.clip_image);
        set_checked(hwnd, IDC_CLIP_FILES, current.clip_files);
        set_checked(hwnd, IDC_KEEP_DISPLAY, current.keep_display);
        set_checked(hwnd, IDC_MOON_CONTROLLER, current.moon_controller);
        set_checked(hwnd, IDC_NET_ENABLE, current.net_enabled);
        set_checked(hwnd, IDC_NET_FIREWALL, current.net_firewall_open);
        set_checked(hwnd, IDC_NET_SHARE_OFF + current.net_share.min(2) as i32, true);

        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

/// Called by the tray message loop for keyboard navigation (Tab/Enter/Esc).
pub(super) fn settings_dialog_message(msg: &mut MSG) -> bool {
    let hwnd = SETTINGS_HWND.load(Ordering::Acquire) as HWND;
    !hwnd.is_null() && unsafe { IsDialogMessageW(hwnd, msg) } != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_pack_roundtrip() {
        for hk in [Hotkey::DEFAULT, Hotkey { mods: MOD_WIN | MOD_SHIFT, kind: HK_KEY, code: 0x4B },
                   Hotkey { mods: 0, kind: HK_MOUSE, code: HK_MOUSE_X2 }] {
            assert_eq!(Hotkey::unpack(hk.pack()), hk);
        }
    }

    #[test]
    fn hotkey_validation_protects_typing() {
        assert!(Hotkey::DEFAULT.validate().is_ok());
        assert!(Hotkey { mods: 0, kind: HK_KEY, code: 0x91 }.validate().is_ok()); // Scroll Lock
        assert!(Hotkey { mods: 0, kind: HK_KEY, code: 0x41 }.validate().is_err()); // A
        assert!(Hotkey { mods: MOD_SHIFT, kind: HK_KEY, code: 0x41 }.validate().is_err()); // Shift+A
        assert!(Hotkey { mods: MOD_CTRL, kind: HK_KEY, code: 0x41 }.validate().is_ok()); // Ctrl+A
        assert!(Hotkey { mods: 0, kind: HK_KEY, code: 0x1B }.validate().is_err()); // Esc
        assert!(Hotkey { mods: MOD_CTRL, kind: HK_KEY, code: 0xA2 }.validate().is_err()); // modifier only
        assert!(Hotkey { mods: 0, kind: HK_MOUSE, code: HK_MOUSE_X1 }.validate().is_ok());
        assert!(Hotkey { mods: 0, kind: HK_MOUSE, code: 1 }.validate().is_err()); // left button
    }

    #[test]
    fn settings_file_roundtrip_and_bad_hotkey_falls_back() {
        let s = UserSettings {
            hotkey: Hotkey { mods: MOD_CTRL | MOD_SHIFT, kind: HK_MOUSE, code: HK_MOUSE_MIDDLE },
            cross_screen: true, cross_side: 1, cross_offset_mm: -20, cross_diagonal_tenths: 270,
            shared_revision: 7, shared_origin: 123,
            clip_text: true, clip_image: false, clip_files: true, keep_display: true,
            net_enabled: true, net_firewall_open: false, net_subnet: [192, 168, 250], net_share: NET_SHARE_CLIENT,
            moon_controller: true, vdd_instance: r"ROOT\DISPLAY\0001".to_string(),
            moonlight_start_cmd: r#""C:\Moonlight\Moonlight.exe" stream "{peer}" "Desktop""#.to_string(),
        };
        assert_eq!(UserSettings::parse(&s.serialize()), s);
        let bad = "kvm_hotkey_mods=0\nkvm_hotkey_kind=0\nkvm_hotkey_code=65\nclip_image=0\n";
        let parsed = UserSettings::parse(bad);
        assert_eq!(parsed.hotkey, Hotkey::DEFAULT);
        assert!(!parsed.clip_image);
        assert_eq!(UserSettings::parse("net_subnet=8.8.8\n").net_subnet, DEFAULT_SUBNET); // public range rejected
        assert_eq!(UserSettings::parse("net_subnet=172.20.5.0\n").net_subnet, [172, 20, 5]);
        assert_eq!(UserSettings::parse("net_share=provide\n").net_share, NET_SHARE_PROVIDE);
        assert_eq!(UserSettings::parse("net_share=client\n").net_share, NET_SHARE_CLIENT);
        assert_eq!(UserSettings::parse("net_share=bogus\n").net_share, NET_SHARE_OFF);
    }
}
