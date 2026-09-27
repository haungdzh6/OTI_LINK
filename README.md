# OTI-Link

OTI-Link is an open-source Windows application for high-speed PC-to-PC communication using OTi USB 3.x transfer cables.

It communicates directly with the cable through its WinUSB interface and provides a modern replacement for the original Smart Data Link software, with remote filesystem access, Explorer integration, clipboard synchronization, keyboard/mouse sharing, background tray operation, and automatic reconnect handling.

> This project is independently developed and is not affiliated with OTi, the cable manufacturer, or the original Smart Data Link software.

---

## Features

### High-Speed PC-to-PC File Transfer

OTI-Link uses the USB 3.x transfer cable directly through WinUSB.

The current implementation uses separate USB lanes for data transfer and control traffic, allowing high-throughput file access while keeping metadata, clipboard, and KVM traffic responsive.

---

### Remote Drives with WinFsp

The remote computer is exposed as a Windows filesystem using WinFsp.

Typical mounted structure:

```text
OTI-DESKTOP-XXXX (R:)
├── Desktop
├── Downloads
├── Documents
└── Drives
    ├── C
    ├── D
    └── ...
```

Supported operations include:

- File reading
- File creation
- File writing
- File overwrite
- File deletion
- Directory creation
- Directory deletion
- Rename and move
- File size changes
- Basic metadata updates
- Directory enumeration
- Writable remote drives
- Large sequential reads
- Read-ahead caching

Only local fixed drives are exported by default.

---

### Windows Explorer Copy / Paste

OTI-Link integrates with the Windows file clipboard.

You can copy files or folders on one PC:

```text
Right-click → Copy
```

Then switch to the other PC:

```text
Right-click → Paste
```

The file list is synchronized through OTI-Link, while the actual file data is transferred through the mounted remote filesystem.

Supported:

- Multiple files
- Multiple folders
- Recursive folder copy
- Files from exported fixed drives
- Native Explorer copy/paste workflow

The current implementation uses copy semantics and does not synchronize MOVE-only clipboard operations.

---

### Text Clipboard Synchronization

Text copied on one computer can automatically become available on the other computer.

This includes normal Windows clipboard text operations such as:

```text
Ctrl+C
Ctrl+V
```

Loop prevention is included to avoid repeatedly retransmitting clipboard content between both systems.

---

### Image Clipboard Synchronization

OTI-Link also supports image clipboard synchronization between the two computers.

Images copied on one side can be pasted on the other side using the normal Windows clipboard workflow.

---

### Keyboard and Mouse Sharing

OTI-Link includes keyboard and mouse sharing between the two computers.

The current KVM implementation includes:

- Keyboard forwarding
- Mouse forwarding
- Raw Input mouse handling
- Relative mouse movement
- Hotkey switching
- Remote input reset
- Mouse configuration synchronization

Default switching hotkey:

```text
Ctrl + Alt + F12
```

---

### Background Tray Application

OTI-Link runs as a Windows GUI subsystem application without a console window.

The tray menu provides:

- Open log
- Open log folder
- Exit OTI-Link

Double-clicking the tray icon opens the log.

Only one OTI-Link instance is allowed per Windows session through a global single-instance mutex.

---

### Reconnect Handling

OTI-Link does not immediately terminate the current session when the cable temporarily disappears.

Instead, it enters a waiting state:

```text
USB_WAIT_DEVICE
```

When Windows enumerates the cable again:

```text
USB_DEVICE_FOUND
USB_MI05_CLAIMED
```

OTI-Link automatically rebuilds the peer session and remounts the remote filesystem.

---

### Shutdown / Restart Coordination

OTI-Link includes an experimental peer shutdown coordination mechanism.

When Windows begins a shutdown or restart sequence, OTI-Link attempts to notify the peer before the local USB session disappears.

This allows both applications to release the active session earlier instead of waiting only for a transport timeout.

This mechanism improves session cleanup but does not guarantee recovery from hardware-level USB enumeration failures.

---

## Supported Hardware

The current implementation has been developed and tested with an OTi USB transfer cable using:

```text
VID: 0EA0
PID: 7301
```

Typical USB composite interfaces include:

```text
MI_00  RNDIS
MI_02  USB Mass Storage
MI_03  HID
MI_04  HID
MI_05  Oti U3 Transfer Cable / WinUSB
```

OTI-Link communicates primarily with:

```text
MI_05
```

Typical MI_05 endpoints:

```text
0x08  Bulk OUT
0x89  Bulk IN

0x0A  Bulk OUT
0x8B  Bulk IN
```

Current lane usage:

```text
Lane 0
OUT 0x08 → remote IN 0x89
Primary filesystem / bulk data traffic

Lane 1
OUT 0x0A → remote IN 0x8B
Control, metadata, clipboard, KVM, and session traffic
```

Other cable revisions may use different interfaces or endpoint layouts and may require code changes.

---

## Requirements

### Operating System

Windows 10 or Windows 11 is recommended.

---

### WinFsp

WinFsp is required for remote filesystem mounting.

Default installation path used during development:

```text
C:\Program Files (x86)\WinFsp\
```

Download WinFsp from its official project website.

---

### LLVM / libclang

Rust bindings used by the project require libclang.

Typical installation:

```text
C:\Program Files\LLVM\
```

Before building:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

---

### Rust

Install the current stable Rust toolchain using rustup.

Verify:

```powershell
rustc --version
cargo --version
```

---

## USB Driver

The OTi transfer interface should use the Microsoft WinUSB driver.

Expected device:

```text
Oti U3 Transfer Cable
```

Expected interface:

```text
USB\VID_0EA0&PID_7301&MI_05
```

You can inspect the device with:

```powershell
Get-PnpDevice -PresentOnly |
Where-Object {
    $_.InstanceId -like 'USB\VID_0EA0&PID_7301*'
} |
Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
```

The RNDIS interface is not required by OTI-Link.

---

## Building

Clone the repository:

```powershell
git clone https://github.com/YOUR_USERNAME/OTI-Link.git
cd OTI-Link
```

Set the LLVM path:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

Build the release version:

```powershell
cargo build --release --bin oti_link_v10
```

The executable will be created at:

```text
target\release\oti_link_v10.exe
```

---

## Build Dependencies

Example Cargo dependencies used by the project:

```toml
[dependencies]
nusb = "0.2.7"
crc32fast = "1"
ctrlc = "3"
arboard = "3.6.1"
filetime = "0.2"
notify = "8.2"
winfsp = { version = "0.13.1", features = ["windows-61"] }
winfsp-sys = "0.12.1"

[build-dependencies]
winfsp = "0.13.1"
```

The project intentionally does not use the WinFsp `system` feature.

---

## Running

Build the same version of OTI-Link for both computers.

Copy the same executable to both systems:

```text
oti_link_v10.exe
```

Make sure the original Smart Data Link application is closed before starting OTI-Link.

Run OTI-Link on both computers.

When the connection succeeds, the log should contain messages similar to:

```text
USB_DEVICE_FOUND
USB_MI05_CLAIMED interface=5
PEER_HELLO
PEER_READY
MOUNTED R:
```

The peer filesystem should then appear as a mounted drive in Windows Explorer.

---

## Logs

OTI-Link writes runtime diagnostic logs that can be opened directly from the tray menu.

Useful messages include:

```text
USB_WAIT_DEVICE
USB_DEVICE_FOUND
USB_MI05_CLAIMED
PEER_HELLO
PEER_READY
MOUNTED
SESSION_END
RECONNECT_BACKOFF_MS
```

Clipboard and KVM activity also have dedicated diagnostic messages.

---

## File Transfer Architecture

OTI-Link does not pre-copy remote files to temporary storage before Explorer paste operations.

Instead:

```text
PC A
Explorer Copy
    ↓
Clipboard file path synchronization
    ↓
PC B receives CF_HDROP
    ↓
Explorer Paste
    ↓
Windows reads the remote mounted path
    ↓
WinFsp
    ↓
OTI-Link
    ↓
USB bulk transfer
```

This keeps Explorer behavior close to normal Windows file copy operations.

---

## Protocol Overview

The current protocol uses separate control and data framing.

Control frame magic:

```text
OC10
```

Data frame magic:

```text
OD10
```

Examples of control message types include:

```text
HELLO
MANIFEST
READY
HEARTBEAT
CHANGE

STAT
LIST
READ
CREATE
WRITE
FLUSH
RENAME
DELETE
SET_SIZE
SET_BASIC

CLIP_TEXT
CLIP_IMAGE
CLIP_FILES

KVM_KEY
KVM_MOUSE
KVM_RESET
KVM_STATE

SESSION_ENDING
SESSION_END_ACK
```

Large file reads and writes use dedicated data frames with CRC32 validation.

---

## Performance

During development, the primary bulk lane has demonstrated transfer performance above 300 MiB/s under favorable conditions.

Observed throughput depends on:

- USB controller
- Cable revision
- Storage speed
- Filesystem workload
- File size
- Host CPU
- WinFsp overhead

These measurements should not be interpreted as a guaranteed hardware maximum.

---

## USB Reconnect Limitation

Some OTi transfer cable revisions can enter a hardware or firmware state where Windows reports:

```text
Unknown USB Device
Set Address Failed
```

or:

```text
USB\VID_0000&PID_0004
```

This can occur after one connected PC is rebooted while the bridge remains powered from the other PC.

Testing has shown that standard Windows recovery operations may not always restore the device, including:

```text
USB hub port cycle
USB root hub restart
PnP rescan
xHCI controller restart
```

A complete electrical power removal from the cable may be required for the affected hardware revision.

The original Smart Data Link software has also been observed to fail to detect the cable in this condition.

This appears to be a hardware/firmware limitation rather than an OTI-Link filesystem or protocol failure.

---

## Three-Connector Cable Notes

Some OTi cables contain three physical connectors, for example:

```text
Side A:
USB Type-A
USB Type-C

Side B:
USB Type-A
```

Testing indicates that the Type-A and Type-C connectors on the same side can both function as host connection options.

For normal use, use only one connector on that side:

```text
Type-C ↔ Type-A
```

or:

```text
Type-A ↔ Type-A
```

Do not assume that the Type-A and Type-C connectors on the same side are intended to be connected simultaneously.

---

## Recommended Usage

For the most reliable operation:

```text
1. Use only one connector on the Type-A / Type-C selectable side.
2. Connect directly to USB 3.x ports when possible.
3. Avoid unpowered USB hubs.
4. Close the original Smart Data Link software before starting OTI-Link.
5. Run the same OTI-Link version on both computers.
6. Allow Windows to finish USB enumeration before starting large transfers.
```

If a cable enters the `VID_0000&PID_0004` state, physically disconnecting all cable connectors long enough to fully remove power may be required before reconnecting.

---

## Tests

Run clipboard file tests:

```powershell
cargo test --release --bin oti_link_v10 clipboard_file_tests
```

Run directory enumeration tests:

```powershell
cargo test --release --bin oti_link_v10 directory_tests
```

---

## Project Goals

OTI-Link aims to provide:

- Direct access to OTi USB transfer hardware
- No dependency on the original proprietary application
- Native Windows Explorer integration
- High-throughput remote file access
- Transparent clipboard sharing
- Keyboard and mouse sharing
- Recoverable background operation
- Clear and inspectable protocol implementation
- Fully open-source development

---

## Project Status

OTI-Link is an experimental reverse-engineered project.

Core functionality is operational, including:

```text
USB communication
Remote filesystem
Writable files
Directory operations
Read caching
Explorer file clipboard
Text clipboard
Image clipboard
KVM
Tray mode
Reconnect waiting
Shutdown coordination
```

However, the project should still be considered development software.

Back up important data before testing writable remote filesystem functionality.

---

## Known Limitations

- Some OTi cable revisions may require full electrical power removal after a peer reboot.
- RNDIS functionality is not used.
- MOVE-only Explorer clipboard operations are not synchronized.
- Other OTi VID/PID revisions are not automatically supported.
- USB endpoint layouts may differ between hardware revisions.
- WinFsp is required.
- Windows is currently the primary supported operating system.
- Shutdown coordination cannot fix a bridge controller that is already electrically locked.
- Sudden power loss cannot be coordinated in software.

---

## Security Notes

OTI-Link gives one computer access to files exported by another computer.

Only run OTI-Link between computers you trust.

The current protocol is intended for a directly connected USB cable and should not be treated as an authenticated or encrypted network protocol.

Do not expose the protocol transport to untrusted systems.

---

## Disclaimer

This software is provided for research, interoperability, and personal development purposes.

Use it at your own risk.

The authors are not responsible for:

- Data loss
- File corruption
- Device malfunction
- USB controller instability
- Driver problems
- Hardware damage
- Compatibility issues

Always keep backups of important files before testing experimental filesystem software.

---

## License

Choose an open-source license before publishing the repository.

Common options include:

```text
MIT
Apache-2.0
GPL-3.0
```

For a permissive project, MIT or Apache-2.0 are common choices.

---

## Contributing

Contributions are welcome.

Useful contribution areas include:

- Support for additional OTi cable revisions
- USB protocol analysis
- Improved reconnect handling
- Performance optimization
- WinFsp filesystem improvements
- Clipboard interoperability
- KVM improvements
- Documentation
- Automated testing

When reporting hardware-related issues, please include:

```text
Windows version
Cable VID/PID
USB interface list
MI_05 endpoint layout
OTI-Link version
Relevant log output
```

---

## Acknowledgements

OTI-Link uses and builds upon open-source projects including:

- Rust
- nusb
- WinFsp
- arboard
- crc32fast

Thanks to the developers and maintainers of these projects.

---

## Name

**OTI-Link**

Open-source Windows software for OTi USB 3.x PC-to-PC transfer cables.
