use super::*;

fn events(mouse: &RawMouse, delta: (i32, i32)) -> Vec<Vec<u8>> {
    let mut out = vec![];
    mouse_events(mouse, delta, |p| out.push(p));
    out
}

fn native(p: &[u8]) -> MOUSEINPUT {
    let input = decode_mouse_input(p).unwrap();
    assert_eq!(input.r#type, INPUT_MOUSE);
    unsafe { input.u.mi }
}

#[test]
fn repeated_relative_moves_survive_a_stationary_local_cursor() {
    let mut motion = MouseMotion::default();
    let mouse = RawMouse { x: 7, y: -3, ..Default::default() };
    for _ in 0..100 {
        let delta = motion.delta(1, &mouse, 0, 0);
        let packet = events(&mouse, delta);
        assert_eq!(packet.len(), 1);
        // Wire layout remains compatible with the existing peer receiver.
        assert_eq!(packet[0], vec![0, 2, 0, 0, 7, 0, 0, 0, 253, 255, 255, 255, 0, 0, 0, 0]);
        let mi = native(&packet[0]);
        assert_eq!((mi.dx, mi.dy, mi.dwFlags, mi.dwExtraInfo), (7, -3, MOUSEEVENTF_MOVE, KVM_TAG));
    }
}

#[test]
fn stationary_mouse_still_sends_all_five_buttons() {
    for (bits, flag, data) in [
        (0x001, MOUSEEVENTF_LEFTDOWN, 0), (0x002, MOUSEEVENTF_LEFTUP, 0),
        (0x004, MOUSEEVENTF_RIGHTDOWN, 0), (0x008, MOUSEEVENTF_RIGHTUP, 0),
        (0x010, MOUSEEVENTF_MIDDLEDOWN, 0), (0x020, MOUSEEVENTF_MIDDLEUP, 0),
        (0x040, MOUSEEVENTF_XDOWN, 1), (0x080, MOUSEEVENTF_XUP, 1),
        (0x100, MOUSEEVENTF_XDOWN, 2), (0x200, MOUSEEVENTF_XUP, 2),
    ] {
        let packets = events(&RawMouse { buttons: bits, ..Default::default() }, (0, 0));
        assert_eq!(packets.len(), 1);
        let mi = native(&packets[0]);
        assert_eq!((mi.dx, mi.dy, mi.dwFlags, mi.mouseData), (0, 0, flag, data));
    }
}

#[test]
fn both_wheels_preserve_signed_and_high_resolution_deltas() {
    for (bit, flag) in [(0x400, MOUSEEVENTF_WHEEL), (0x800, MOUSEEVENTF_HWHEEL)] {
        for delta in [-240i16, -120, -1, 1, 120, 240] {
            let mouse = RawMouse { buttons: bit | ((delta as u16 as u32) << 16), ..Default::default() };
            let packets = events(&mouse, (0, 0));
            assert_eq!(packets.len(), 1);
            let mi = native(&packets[0]);
            assert_eq!((mi.dwFlags, mi.mouseData as i32), (flag, delta as i32));
        }
    }
}

#[test]
fn combined_packet_moves_before_click_and_scroll() {
    let mouse = RawMouse { buttons: 0x401 | (120 << 16), ..Default::default() };
    let packets = events(&mouse, (-20, 9));
    assert_eq!(packets.len(), 3);
    let flags: Vec<_> = packets.iter().map(|p| native(p).dwFlags).collect();
    assert_eq!(flags, [MOUSEEVENTF_MOVE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_WHEEL]);
    assert!(events(&RawMouse::default(), (0, 0)).is_empty());
}

#[test]
fn absolute_devices_have_independent_baselines_and_reset_on_topology_changes() {
    let mut motion = MouseMotion::default();
    let mut mouse = RawMouse { flags: MOUSE_MOVE_ABSOLUTE, x: 0, y: 65535, ..Default::default() };
    assert_eq!(motion.delta(1, &mouse, 1920, 1080), (0, 0));
    mouse.x = 65535; mouse.y = 0;
    assert_eq!(motion.delta(2, &mouse, 1920, 1080), (0, 0));
    assert_eq!(motion.delta(1, &mouse, 1920, 1080), (1919, -1079));
    assert_eq!(motion.delta(1, &mouse, 3840, 1080), (0, 0));
    mouse.flags |= MOUSE_VIRTUAL_DESKTOP;
    assert_eq!(motion.delta(1, &mouse, 3840, 1080), (0, 0));
    mouse.x = 0; mouse.y = 65535;
    assert_eq!(motion.delta(1, &mouse, 3840, 1080), (-3839, 1079));
}

#[test]
fn target_and_session_changes_clear_absolute_baselines() {
    let mut state = CaptureState::default();
    let mouse = RawMouse { flags: MOUSE_MOVE_ABSOLUTE, ..Default::default() };
    for target in [Some((1, true)), Some((1, false)), Some((2, true)), None] {
        state.motion.delta(1, &mouse, 1920, 1080);
        state.set_target(target);
        assert!(state.motion.absolute.is_empty());
    }
}

#[test]
fn mouse_protocol_rejects_truncation_and_unknown_messages() {
    assert!(matches!(decode_mouse_input(&[0; 15]), Err(87)));
    assert!(matches!(decode_mouse_input(&[0; 16]), Err(87)));
}

#[test]
fn windows_mouse_ffi_layout_matches_sdk() {
    assert_eq!(std::mem::size_of::<RawMouse>(), 24);
    assert_eq!(std::mem::offset_of!(RawMouse, buttons), 4);
    assert_eq!(std::mem::offset_of!(RawMouse, x), 12);
    assert_eq!(std::mem::offset_of!(RawMouse, extra), 20);
    let x64 = std::mem::size_of::<usize>() == 8;
    assert_eq!(std::mem::size_of::<RawInputHeader>(), if x64 { 24 } else { 16 });
    assert_eq!(std::mem::size_of::<RawMouseInput>(), if x64 { 48 } else { 40 });
    assert_eq!(std::mem::size_of::<INPUT>(), if x64 { 40 } else { 28 });
}

#[test]
fn native_raw_input_window_registers_and_cleans_up() {
    // No hooks, injected clicks or USB connection: verifies real Win32 setup.
    unsafe {
        let hwnd = create_capture_window().expect("Raw Input registration");
        destroy_capture_window(hwnd);
        let hwnd = create_capture_window().expect("Raw Input re-registration");
        destroy_capture_window(hwnd);
    }
}

#[test]
fn capture_routes_only_connected_remote_input_and_never_echoes_our_tag() {
    let (tx, rx) = mpsc::channel();
    let (status_tx, _) = mpsc::channel();
    let remote = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(true));
    *hook_slot().lock().unwrap() = Some(HookState {
        session: 42, tx, remote: remote.clone(), connected: connected.clone(), status_tx,
        logger: Logger::new("kvm_mouse_tests").unwrap(),
    });
    let mut input: RawMouseInput = unsafe { std::mem::zeroed() };
    input.mouse.x = -9; input.mouse.y = 4; input.mouse.buttons = 1;
    capture_mouse(&input);
    assert!(rx.try_recv().is_err());
    remote.store(true, Ordering::Release);
    capture_mouse(&input);
    let movement = rx.try_recv().unwrap();
    assert_eq!(movement.session, 42);
    assert_eq!((native(&movement.payload).dx, native(&movement.payload).dy), (-9, 4));
    assert_eq!(native(&rx.try_recv().unwrap().payload).dwFlags, MOUSEEVENTF_LEFTDOWN);
    assert!(rx.try_recv().is_err());
    input.mouse.extra = KVM_TAG as u32;
    capture_mouse(&input);
    assert!(rx.try_recv().is_err());
    input.mouse.extra = 0;
    connected.store(false, Ordering::Release);
    capture_mouse(&input);
    assert!(rx.try_recv().is_err());
    *hook_slot().lock().unwrap() = None;
    reset_capture();
}
