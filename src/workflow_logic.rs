// FIX15: platform-free decisions for the Moonlight / OTI workflow.
//
// Everything here is pure so the state machine can be unit-tested without
// Windows, a USB peer, a VDD driver or Moonlight. workflow.rs owns the threads
// and side effects and asks these functions what to do next.

/// The two user-visible working modes. Wire values match FIX14.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Moonlight,
    Oti,
}

pub(crate) const WIRE_MOONLIGHT: u8 = 1;
pub(crate) const WIRE_OTI: u8 = 2;

impl Mode {
    pub(crate) fn wire(self) -> u8 {
        match self { Mode::Moonlight => WIRE_MOONLIGHT, Mode::Oti => WIRE_OTI }
    }
    pub(crate) fn from_wire(v: u64) -> Option<Self> {
        match v { 1 => Some(Mode::Moonlight), 2 => Some(Mode::Oti), _ => None }
    }
    pub(crate) fn other(self) -> Self {
        match self { Mode::Moonlight => Mode::Oti, Mode::Oti => Mode::Moonlight }
    }
    pub(crate) fn name(self) -> &'static str {
        match self { Mode::Moonlight => "MOONLIGHT", Mode::Oti => "OTI" }
    }
}

/// Where the controller (A) currently is. Only `Stable(Oti)` opens the OTI gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Stable(Mode),
    Entering,
    Leaving,
}

impl Phase {
    pub(crate) fn gate_open(self) -> bool { self == Phase::Stable(Mode::Oti) }
}

/// What the controller worker should do on its next pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Idle,
    /// Run every step towards Moonlight (VDD, extend, start Moonlight on B).
    Enter,
    /// Run every step towards OTI (quit Moonlight on B, display 1 only).
    Leave,
    /// Stable in Moonlight but B may have changed while unsynced: ask, never relaunch.
    VerifyMoonlight,
    /// Stable in OTI but B is unsynced: tell B to be in OTI mode.
    AssertOti,
}

/// `force` is set by an explicit mode button: re-run all (idempotent) steps as a repair.
pub(crate) fn plan(phase: Phase, desired: Mode, force: bool, peer_synced: bool, link_up: bool) -> Action {
    match (phase, desired) {
        (Phase::Stable(Mode::Moonlight), Mode::Moonlight) if !force => {
            if !peer_synced && link_up { Action::VerifyMoonlight } else { Action::Idle }
        }
        (Phase::Stable(Mode::Oti), Mode::Oti) if !force => {
            if !peer_synced && link_up { Action::AssertOti } else { Action::Idle }
        }
        (_, Mode::Moonlight) => Action::Enter,
        (_, Mode::Oti) => Action::Leave,
    }
}

/// Hotkey toggles flip the *desired* mode, not the current phase, so presses made
/// while a switch is running are not lost and an even number of presses cancels out.
pub(crate) fn toggled(desired: Mode) -> Mode { desired.other() }

/// Key bounce / accidental double press filter for the global hotkey.
pub(crate) const TOGGLE_DEBOUNCE_MS: u64 = 300;
pub(crate) fn debounce_ok(last_ms: Option<u64>, now_ms: u64) -> bool {
    last_ms.map(|t| now_ms.saturating_sub(t) >= TOGGLE_DEBOUNCE_MS).unwrap_or(true)
}

/// What B does with one mode command, given whether a Moonlight session is alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewerAction {
    Launch,
    Keep,
    NotStreaming,
    Quit,
    Nothing,
}

pub(crate) fn viewer_plan(cmd: Mode, launch_allowed: bool, moonlight_alive: bool) -> ViewerAction {
    match cmd {
        Mode::Moonlight if moonlight_alive => ViewerAction::Keep,
        Mode::Moonlight if launch_allowed => ViewerAction::Launch,
        Mode::Moonlight => ViewerAction::NotStreaming,
        Mode::Oti if moonlight_alive => ViewerAction::Quit,
        Mode::Oti => ViewerAction::Nothing,
    }
}

/// B accepts a command only if it is newer than the last one from the same
/// controller USB session. A new session restarts the sequence.
pub(crate) fn accept_generation(last: Option<(u64, u64)>, session: u64, generation: u64) -> bool {
    match last {
        Some((s, g)) if s == session => generation > g,
        _ => true,
    }
}

// ---- wire flags -------------------------------------------------------------

pub(crate) const MODE_FLAG_LAUNCH: u64 = 1;
pub(crate) const ACK_FLAG_OK: u64 = 1;
pub(crate) const ACK_FLAG_MOONLIGHT_RUNNING: u64 = 2;
pub(crate) const EVENT_MOONLIGHT_ENDED: u64 = 1;

// ---- Moonlight command line ----------------------------------------------------

/// Substitutes `{peer}` (A's computer name). Quotes are stripped from the name so
/// it cannot break out of the quoted argument in the template.
pub(crate) fn render_moonlight_command(template: &str, peer: &str) -> String {
    let peer: String = peer.chars().filter(|c| *c != '"' && !c.is_control()).collect();
    template.replace("{peer}", peer.trim())
}

/// Splits a Windows command line into (program, raw argument string) the way
/// CreateProcess resolves the program: a leading quoted token, otherwise the text
/// up to the first whitespace.
pub(crate) fn split_command_line(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    if cmd.is_empty() { return None; }
    let (program, rest) = if let Some(stripped) = cmd.strip_prefix('"') {
        let end = stripped.find('"')?;
        (&stripped[..end], &stripped[end + 1..])
    } else {
        match cmd.find(char::is_whitespace) {
            Some(i) => (&cmd[..i], &cmd[i..]),
            None => (cmd, ""),
        }
    };
    let program = program.trim();
    if program.is_empty() { return None; }
    Some((program.to_string(), rest.trim().to_string()))
}

/// Lower-cased file name of a program path ("C:\\X\\Moonlight.exe" -> "moonlight.exe").
pub(crate) fn exe_file_name(program: &str) -> String {
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program).trim();
    let mut name = name.to_ascii_lowercase();
    if !name.is_empty() && !name.ends_with(".exe") { name.push_str(".exe"); }
    name
}

// ---- VDD identification ------------------------------------------------------------

/// Device instance IDs contain only these characters; anything else (quotes,
/// spaces, shell metacharacters) is refused before it reaches pnputil.
pub(crate) fn valid_instance_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id.len() <= 200 && id.contains('\\')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '\\' | '_' | '&' | '.' | '-' | '{' | '}' | '#'))
}

/// A display adapter device path looks like `\\?\ROOT#DISPLAY#0001#{5b45201d-...}`;
/// the instance ID `ROOT\DISPLAY\0001` identifies the VDD adapter.
pub(crate) fn adapter_path_matches(adapter_path: &str, instance_id: &str) -> bool {
    let inst = instance_id.trim().replace('\\', "#").to_ascii_uppercase();
    if inst.is_empty() { return false; }
    let path = adapter_path.to_ascii_uppercase();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    path.strip_prefix(inst.as_str()).map(|rest| rest.is_empty() || rest.starts_with('#')).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_runs_full_switch_when_mode_differs() {
        assert_eq!(plan(Phase::Stable(Mode::Oti), Mode::Moonlight, false, true, true), Action::Enter);
        assert_eq!(plan(Phase::Stable(Mode::Moonlight), Mode::Oti, false, true, true), Action::Leave);
        // Aborted half-way into Moonlight because the user toggled back.
        assert_eq!(plan(Phase::Entering, Mode::Oti, false, true, true), Action::Leave);
        assert_eq!(plan(Phase::Entering, Mode::Moonlight, false, true, true), Action::Enter);
    }

    #[test]
    fn plan_stable_only_syncs_peer_and_never_relaunches() {
        assert_eq!(plan(Phase::Stable(Mode::Oti), Mode::Oti, false, true, true), Action::Idle);
        assert_eq!(plan(Phase::Stable(Mode::Oti), Mode::Oti, false, false, true), Action::AssertOti);
        assert_eq!(plan(Phase::Stable(Mode::Oti), Mode::Oti, false, false, false), Action::Idle);
        assert_eq!(plan(Phase::Stable(Mode::Moonlight), Mode::Moonlight, false, false, true), Action::VerifyMoonlight);
        assert_eq!(plan(Phase::Stable(Mode::Moonlight), Mode::Moonlight, false, true, true), Action::Idle);
    }

    #[test]
    fn plan_forced_button_repairs_even_when_stable() {
        assert_eq!(plan(Phase::Stable(Mode::Oti), Mode::Oti, true, true, true), Action::Leave);
        assert_eq!(plan(Phase::Stable(Mode::Moonlight), Mode::Moonlight, true, true, true), Action::Enter);
    }

    #[test]
    fn gate_is_open_only_in_stable_oti() {
        assert!(Phase::Stable(Mode::Oti).gate_open());
        assert!(!Phase::Stable(Mode::Moonlight).gate_open());
        assert!(!Phase::Entering.gate_open());
        assert!(!Phase::Leaving.gate_open());
    }

    #[test]
    fn rapid_toggles_converge_to_parity() {
        let mut desired = Mode::Oti;
        for _ in 0..3 { desired = toggled(desired); }
        assert_eq!(desired, Mode::Moonlight);
        desired = toggled(desired);
        assert_eq!(desired, Mode::Oti);
    }

    #[test]
    fn debounce_filters_bounce_only() {
        assert!(debounce_ok(None, 5));
        assert!(!debounce_ok(Some(1000), 1100));
        assert!(debounce_ok(Some(1000), 1000 + TOGGLE_DEBOUNCE_MS));
        assert!(debounce_ok(Some(u64::MAX), 0) == false);
    }

    #[test]
    fn viewer_plan_matrix() {
        assert_eq!(viewer_plan(Mode::Moonlight, true, false), ViewerAction::Launch);
        assert_eq!(viewer_plan(Mode::Moonlight, true, true), ViewerAction::Keep);
        assert_eq!(viewer_plan(Mode::Moonlight, false, true), ViewerAction::Keep);
        assert_eq!(viewer_plan(Mode::Moonlight, false, false), ViewerAction::NotStreaming);
        assert_eq!(viewer_plan(Mode::Oti, false, true), ViewerAction::Quit);
        assert_eq!(viewer_plan(Mode::Oti, true, false), ViewerAction::Nothing);
    }

    #[test]
    fn generations_are_monotonic_per_session() {
        assert!(accept_generation(None, 7, 1));
        assert!(accept_generation(Some((7, 3)), 7, 4));
        assert!(!accept_generation(Some((7, 3)), 7, 3));
        assert!(!accept_generation(Some((7, 3)), 7, 2));
        assert!(accept_generation(Some((7, 3)), 8, 1));
    }

    #[test]
    fn mode_wire_roundtrip() {
        for m in [Mode::Moonlight, Mode::Oti] { assert_eq!(Mode::from_wire(m.wire() as u64), Some(m)); }
        assert_eq!(Mode::from_wire(0), None);
        assert_eq!(Mode::from_wire(3), None);
    }

    #[test]
    fn command_line_split_and_render() {
        let t = r#""%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe" stream "{peer}" "Desktop""#;
        let cmd = render_moonlight_command(t, "DESK\"TOP-A\n");
        assert_eq!(cmd, r#""%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe" stream "DESKTOP-A" "Desktop""#);
        let (p, a) = split_command_line(&cmd).unwrap();
        assert_eq!(p, r"%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe");
        assert_eq!(a, r#"stream "DESKTOP-A" "Desktop""#);
        assert_eq!(split_command_line(r"C:\M\Moonlight.exe stream host Desktop").unwrap(),
                   (r"C:\M\Moonlight.exe".to_string(), "stream host Desktop".to_string()));
        assert_eq!(split_command_line("moonlight").unwrap(), ("moonlight".to_string(), String::new()));
        assert_eq!(split_command_line("   "), None);
        assert_eq!(split_command_line(r#""unterminated stream"#), None);
        assert_eq!(split_command_line(r#""" stream"#), None);
    }

    #[test]
    fn exe_names() {
        assert_eq!(exe_file_name(r"C:\Program Files\Moonlight Game Streaming\Moonlight.exe"), "moonlight.exe");
        assert_eq!(exe_file_name("moonlight"), "moonlight.exe");
        assert_eq!(exe_file_name("D:/x/MOONLIGHT.EXE"), "moonlight.exe");
    }

    #[test]
    fn instance_id_validation() {
        assert!(valid_instance_id(r"ROOT\DISPLAY\0001"));
        assert!(valid_instance_id(r"SWD\MTT\VDD&0001"));
        assert!(!valid_instance_id(""));
        assert!(!valid_instance_id("ROOTDISPLAY"));
        assert!(!valid_instance_id(r#"ROOT\DISPLAY\0001" /x"#));
        assert!(!valid_instance_id(r"ROOT\DISPLAY\0001 & calc"));
    }

    #[test]
    fn adapter_path_matching() {
        let p = r"\\?\ROOT#DISPLAY#0001#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}";
        assert!(adapter_path_matches(p, r"ROOT\DISPLAY\0001"));
        assert!(adapter_path_matches(&p.to_lowercase(), r"root\display\0001"));
        assert!(!adapter_path_matches(p, r"ROOT\DISPLAY\000"));
        assert!(!adapter_path_matches(p, r"ROOT\DISPLAY\0002"));
        assert!(!adapter_path_matches(r"\\?\PCI#VEN_10DE&DEV_2484#4&1#{5b45201d}", r"ROOT\DISPLAY\0001"));
        assert!(!adapter_path_matches(p, ""));
    }
}
