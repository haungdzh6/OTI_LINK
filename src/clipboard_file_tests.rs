use super::*;

#[test]
fn clipboard_file_payload_roundtrip() {
    let input = vec![
        r"\Desktop\alpha.txt".to_string(),
        r"\Drives\C\Users\Test\中文 😀\beta.bin".to_string(),
        r"\Downloads\folder".to_string(),
    ];
    let encoded = encode_clip_files(&input).unwrap();
    let decoded = decode_clip_files(&encoded).unwrap();
    assert_eq!(decoded, input);
}

#[test]
fn clipboard_virtual_paths_map_to_current_peer_drive() {
    assert_eq!(
        virtual_to_mount_path("R:", r"\Desktop\a.txt").unwrap(),
        PathBuf::from(r"R:\Desktop\a.txt")
    );
    assert_eq!(
        virtual_to_mount_path("S:", r"\Drives\D\x\y.bin").unwrap(),
        PathBuf::from(r"S:\Drives\D\x\y.bin")
    );
}

#[test]
fn clipboard_virtual_paths_reject_escape_components() {
    assert!(virtual_to_mount_path("R:", r"\Desktop\..\secret.txt").is_err());
    assert!(decode_clip_files(&{
        let mut b=Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes());
        enc_string(&mut b,r"\Desktop\..\bad").unwrap();
        b
    }).is_err());
}

#[test]
fn peer_mount_paths_are_detected_for_loop_suppression() {
    assert!(path_on_mount(Path::new(r"R:\Desktop\a.txt"),"R:"));
    assert!(path_on_mount(Path::new(r"r:\Drives\C\a.txt"),"R:"));
    assert!(!path_on_mount(Path::new(r"C:\a.txt"),"R:"));
}
