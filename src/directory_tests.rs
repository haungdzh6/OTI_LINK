use super::*;

fn init_runtime() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let logger = Logger::new("directory_tests").unwrap();
        let _runtime = init_winfsp_runtime(&logger).unwrap();
    });
}

fn entry(name: impl Into<String>) -> RemoteEntry {
    RemoteEntry { name: name.into(), meta: synthetic_meta() }
}

#[test]
fn custom_tray_icon_has_transparency_and_distinct_link_colors() {
    let pixels = tray_icon_pixels();
    assert_eq!(pixels.len(), (TRAY_ICON_SIZE * TRAY_ICON_SIZE) as usize);
    assert_eq!(pixels[0], 0);
    assert!(pixels.contains(&0xFF20BFD0));
    assert!(pixels.contains(&0xFFF5A94F));
    assert!(pixels.contains(&0xFFB9F36A));
    unsafe {
        let icon = create_tray_icon().expect("custom tray icon");
        assert!(!icon.is_null());
        assert_ne!(DestroyIcon(icon), 0);
    }
}

#[test]
fn kvm_mouse_settings_wire_payload_round_trips() {
    let settings = MouseSettings { threshold1: 6, threshold2: 10, acceleration: 1, speed: 12 };
    assert_eq!(MouseSettings::decode(&settings.encode()).unwrap(), settings);
}

#[test]
fn reads_current_windows_mouse_system_settings() {
    let settings = read_mouse_settings().expect("Windows mouse settings");
    assert!(settings.threshold1 >= 0);
    assert!(settings.threshold2 >= 0);
    assert!((0..=2).contains(&settings.acceleration));
    assert!((1..=20).contains(&settings.speed));
}

#[test]
fn kvm_mouse_settings_reject_malformed_or_out_of_range_payloads() {
    assert!(MouseSettings::decode(&[0; 15]).is_err());
    let invalid_speed = MouseSettings { threshold1: 6, threshold2: 10, acceleration: 1, speed: 21 };
    assert!(MouseSettings::decode(&invalid_speed.encode()).is_err());
    let invalid_acceleration = MouseSettings { threshold1: 6, threshold2: 10, acceleration: 3, speed: 12 };
    assert!(MouseSettings::decode(&invalid_acceleration.encode()).is_err());
}

// Decode the public WinFsp wire layout independently of the page writer.
fn decode_page(buffer: &[u8]) -> (Vec<String>, bool) {
    let name_offset = std::mem::offset_of!(winfsp_sys::FSP_FSCTL_DIR_INFO, FileNameBuf);
    let mut names = Vec::new();
    let mut pos = 0;
    while pos < buffer.len() {
        let size = u16::from_le_bytes(buffer[pos..pos + 2].try_into().unwrap()) as usize;
        if size == 0 {
            assert_eq!(pos + 2, buffer.len());
            return (names, true);
        }
        assert!(size >= name_offset && pos + size <= buffer.len());
        let wide: Vec<u16> = buffer[pos + name_offset..pos + size]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        names.push(String::from_utf16(&wide).unwrap());
        pos += (size + 7) & !7;
    }
    (names, false)
}

#[test]
fn directory_names_have_no_terminator_and_allow_255_utf16_units() {
    init_runtime();
    let entries = vec![entry("OTI_V9_TEST_中文_😀"), entry("x".repeat(255))];
    let mut buffer = [0xA5; 4096];
    let count = write_directory_page(&entries, None, &mut buffer).unwrap();
    let (names, eof) = decode_page(&buffer[..count as usize]);
    assert_eq!(names, entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>());
    assert!(eof);
}

#[test]
fn partial_page_must_not_signal_end_of_directory() {
    init_runtime();
    let entries = vec![entry("."), entry(".."), entry("OTI_V9_TEST")];
    // Dot records occupy 112 bytes each, leaving room for an EOF record
    // but not for the next filename. This reproduces the false EOF.
    let mut buffer = [0xA5; 240];
    let count = write_directory_page(&entries, None, &mut buffer).unwrap();
    let (names, eof) = decode_page(&buffer[..count as usize]);
    assert_eq!(names, [".", ".."]);
    assert!(!eof, "the target is still on the next page");
}

#[test]
fn paginated_listing_reaches_target_without_missing_or_duplicate_entries() {
    init_runtime();
    let mut entries = vec![entry("."), entry("..")];
    entries.extend((0..300).map(|i| entry(format!("entry_{i:04}"))));
    entries.push(entry("OTI_V9_TEST_中文_😀"));
    for capacity in [240, 512, 4096] {
        let mut buffer = vec![0xA5; capacity];
        let mut names = Vec::new();
        let mut marker: Option<String> = None;
        let mut ended = false;
        for _ in 0..=entries.len() {
            let count = write_directory_page(&entries, marker.as_deref(), &mut buffer).unwrap();
            let (page, eof) = decode_page(&buffer[..count as usize]);
            if let Some(last) = page.last() {
                marker = Some(last.clone());
            }
            assert!(eof || !page.is_empty(), "enumeration must make progress");
            names.extend(page);
            if eof { ended = true; break; }
        }
        assert!(ended, "enumeration must terminate");
        assert_eq!(names, entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>());
    }
}

#[test]
fn full_last_page_returns_eof_on_next_call() {
    init_runtime();
    let entries = vec![entry(".")];
    let mut buffer = [0xA5; 112];
    let count = write_directory_page(&entries, None, &mut buffer).unwrap();
    assert_eq!(decode_page(&buffer[..count as usize]), (vec![".".into()], false));
    let count = write_directory_page(&entries, Some("."), &mut buffer).unwrap();
    assert_eq!(decode_page(&buffer[..count as usize]), (vec![], true));
}

#[test]
fn empty_listing_returns_eof() {
    init_runtime();
    let mut buffer = [0xA5; 16];
    let count = write_directory_page(&[], None, &mut buffer).unwrap();
    assert_eq!(decode_page(&buffer[..count as usize]), (vec![], true));
}

// A read-only, in-memory mount exercises Windows' exact-name filtering and
// PowerShell's pre-delete link check without touching the remote machine.
struct DirectoryFixture(Vec<RemoteEntry>);

impl DirectoryFixture {
    fn metadata(&self, path: &str) -> winfsp::Result<RemoteMeta> {
        if path == "\\" || self.0.iter().any(|e| eq_ci(path, &format!("\\{}", e.name))) {
            Ok(synthetic_meta())
        } else {
            Err(nt_error(STATUS_OBJECT_NAME_NOT_FOUND))
        }
    }
}

impl FileSystemContext for DirectoryFixture {
    type FileContext = String;

    fn get_security_by_name(&self, name: &U16CStr, _sd: Option<&mut [c_void]>,
        _resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>) -> winfsp::Result<FileSecurity> {
        let meta = self.metadata(&u16c_to_string(name))?;
        Ok(FileSecurity { reparse: false, sz_security_descriptor: 0, attributes: meta.attributes })
    }

    fn open(&self, name: &U16CStr, _opts: u32, _access: FILE_ACCESS_RIGHTS,
        info: &mut OpenFileInfo) -> winfsp::Result<String> {
        let path = u16c_to_string(name);
        *info.as_mut() = self.metadata(&path)?.to_file_info();
        Ok(path)
    }

    fn close(&self, _context: String) {}

    fn get_file_info(&self, context: &String, info: &mut FileInfo) -> winfsp::Result<()> {
        *info = self.metadata(context)?.to_file_info();
        Ok(())
    }

    fn read_directory(&self, context: &String, _pattern: Option<&U16CStr>,
        marker: DirMarker<'_>, buffer: &mut [u8]) -> winfsp::Result<u32> {
        let marker = marker.inner_as_cstr().map(u16c_to_string);
        let entries = if context == "\\" { self.0.as_slice() } else { &[] };
        write_directory_page(entries, marker.as_deref(), buffer)
    }
}

#[test]
#[ignore = "requires installed WinFsp driver and an unused drive letter"]
fn windows_exact_lookup_and_powershell_link_check() {
    init_runtime();
    let mut entries: Vec<_> = (0..300).map(|i| entry(format!("entry_{i:04}"))).collect();
    entries.push(entry("OTI_V9_TEST_TARGET"));
    let mut params = VolumeParams::new();
    params.filesystem_name("OTITEST").read_only_volume(true)
        .case_sensitive_search(false).case_preserved_names(true).unicode_on_disk(true)
        .persistent_acls(false).file_info_timeout(0).dir_info_timeout(0);
    let mut host = FileSystemHost::<DirectoryFixture, FineGuard>::new(params, DirectoryFixture(entries)).unwrap();
    let drive = choose_drive().unwrap();
    host.mount(&drive).unwrap();
    host.start_with_threads(2).unwrap();
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference = 'Stop'
$root = $env:OTI_DIRECTORY_TEST_DRIVE + '\'
$all = @(Get-ChildItem -LiteralPath $root -Force)
if ($all.Count -ne 301) { throw "Listing incomplete: $($all.Count)" }
$type = [psobject].Assembly.GetType('Microsoft.PowerShell.Commands.InternalSymbolicLinkLinkCodeMethods')
$method = $type.GetMethod('IsReparsePointLikeSymlink', [Reflection.BindingFlags]'Static,Public,NonPublic')
foreach ($name in @('entry_0000', 'OTI_V9_TEST_TARGET')) {
    $match = @(Get-ChildItem -LiteralPath $root -Filter $name)
    if ($match.Count -ne 1) { throw "Exact lookup failed: $name" }
    $directory = [IO.DirectoryInfo]::new($root + $name)
    if ($method.Invoke($null, @($directory))) { throw "Unexpected symlink: $name" }
}
if (@(Get-ChildItem -LiteralPath $root -Filter 'missing').Count -ne 0) { throw 'Missing entry returned' }
Write-Output '[OK] 301 entries, exact-name lookup, PowerShell pre-delete link check'
"#])
        .env("OTI_DIRECTORY_TEST_DRIVE", &drive)
        .output().unwrap();
    host.stop();
    host.unmount();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
