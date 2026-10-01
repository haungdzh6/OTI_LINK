[**English**](README.md) | [简体中文](README-zh.md)

---

# OTI-Link

OTI-Link is an experimental open-source Windows project for high-speed PC-to-PC communication over OTi USB 3.x transfer cables. It provides remote file access, native Windows Explorer copy/paste, clipboard synchronization, keyboard/mouse sharing, cross-screen KVM, a virtual network adapter, WinNAT sharing, and Moonlight/VDD secondary-display workflow coordination.

> This project is independently developed and is not affiliated with OTi, the cable manufacturer, or the original Smart Data Link software.

## Current Version

- Current development line: **OTI-Link 10.0-FIX15**
- Current protocol version: **14**
- Both computers must run the same FIX15 build.
- The current branch is based on **FIX13 compilefix3 no-display**: OTI-Link's built-in `display.rs` / DXGI display-streaming implementation has been removed.
- The current secondary-display solution uses **Sunshine + Moonlight + Virtual Display Driver (VDD)** for video. OTI-Link coordinates USB transport, files, KVM, clipboard, networking, and mode switching.

---

## Features

### High-Speed USB Transport

OTI-Link communicates directly with the OTi USB 3.x transfer cable through WinUSB on interface MI_05.

Typical hardware:

```text
VID: 0EA0
PID: 7301
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
High-throughput data: file data and virtual-network traffic

Lane 1
OUT 0x0A → remote IN 0x8B
Low-latency control: session, RPC, clipboard, KVM,
network state, and FIX15 workflow messages
```

Since FIX13, Lane 0 frames include the current session and a header CRC, while Lane 1 filesystem RPC is also bound to the authenticated peer session. Unrecoverable Lane 0 read/write failures terminate the whole USB session and trigger reconnect, preventing a half-connected state where Lane 1 is alive but the data path is dead.

### Remote Filesystem / WinFsp

The peer computer is exposed as a Windows filesystem, for example:

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

- File reading, creation, writing, overwrite, and deletion
- Directory creation and deletion
- Rename and move
- File-size and basic metadata updates
- Directory enumeration
- Large sequential reads
- Adaptive read-ahead caching
- Fast failure of pending requests when the USB session is lost

Only local fixed drives are exported by default.

### Windows Explorer Copy / Paste

OTI-Link synchronizes the Windows file clipboard while actual file data is read through the WinFsp mount and transferred over Lane 0.

Typical flow:

```text
Explorer on PC A: Copy
        ↓
OTI-Link synchronizes the file list
        ↓
PC B receives CF_HDROP
        ↓
Explorer on PC B: Paste
        ↓
Windows reads the remote mounted path
        ↓
WinFsp → OTI-Link → USB Lane 0
```

Multiple files, multiple folders, and recursive directory copy are supported.

**The current implementation uses COPY semantics. The experimental cross-screen OLE drag-and-drop feature from FIX10 was removed in FIX11. Use normal copy/paste for cross-PC file transfer.**

### Clipboard Synchronization

Clipboard synchronization can be controlled independently for:

- Text
- Images
- Files

The clipboard path uses change detection, stabilization delay, and loop prevention to avoid ping-pong updates between both PCs.

In Moonlight display mode, all three clipboard paths are gated off by the workflow state machine. Returning to OTI mode restores each user's original clipboard settings.

### Keyboard / Mouse Sharing and Cross-Screen KVM

Supported capabilities include:

- Keyboard forwarding
- Raw Input mouse handling
- Relative mouse movement
- Mouse buttons and wheel
- KVM hotkey switching
- Bidirectional mouse cross-screen switching
- Direction, offset, and screen-layout configuration
- Session/disconnect input reset
- High-frequency mouse-motion coalescing to avoid flooding Lane 1

Default KVM hotkey:

```text
Ctrl + Alt + F12
```

Normal cross-screen switching preserves KeyDown/KeyUp routing. On disconnect or recovery, only input that was actually injected remotely is released.

> Windows `SendInput` is subject to UIPI and secure-desktop restrictions. A normally privileged OTI-Link process cannot reliably inject input into higher-integrity windows or the Ctrl+Alt+Del secure desktop.

### Virtual Network Adapter / Wintun

When enabled, OTI-Link creates a private virtual Ethernet-like link between the two computers.

Typical uses:

- `ping`
- SMB
- Remote Desktop
- LAN-style games
- iperf
- Other normal TCP/UDP applications

Setup:

1. Download Wintun.
2. Copy the amd64 `wintun.dll` next to `oti_link_v10.exe`.
3. Enable the virtual adapter in OTI-Link settings on both PCs.
4. UAC is used only by the elevated network helper. The main OTI-Link process should not normally run elevated, otherwise the WinFsp mount may appear only in the elevated session and not in normal Explorer.

Default network:

```text
OTI-Link
10.77.77.0/24
```

The two endpoint addresses are derived from each installation ID and remain stable as `.1` and `.2`.

Virtual-network traffic runs over Lane 0 and is interleaved with file traffic.

### WinNAT Internet Sharing

A typical configuration is:

```text
PC A (has Internet): Share this PC's network with the peer
PC B: Use peer for Internet access
```

The provider uses Windows WinNAT rather than ICS.

Since FIX13, configuration is transactional:

- The provider publishes `provider_ready` only after its local WinNAT setup succeeds.
- The client installs the OTI-Link default route only after receiving `provider_ready`.
- Every configuration has a generation number, so stale results cannot overwrite newer state.
- Provider/client failure paths remove intermediate NAT, forwarding, route, and DNS state.
- The target OTI-Link `/24` is checked for overlap with existing IPv4 addresses/routes before configuration.

Default DNS:

```text
223.5.5.5
119.29.29.29
```

It can be overridden with `net_share_dns=` in the settings file.

On some Windows client configurations, an existing `NetNat` created by Docker, Hyper-V, or another product may conflict with the OTI-Link NAT.

---

# FIX15: Moonlight Display Mode / OTI Mode

## Intended Setup

PC A:

- Main computer
- Runs Sunshine
- Has Virtual Display Driver (VDD) installed
- Runs OTI-Link with **"This PC is A"** enabled

PC B:

- Laptop / secondary-display machine
- Runs Moonlight
- Runs OTI-Link
- Does **not** enable "This PC is A"

FIX15 reduces the workflow to two mutually exclusive target states.

| Component | Moonlight Display Mode | OTI Mode |
|---|---|---|
| A: VDD device | Enabled | Kept enabled; no repeated PnP rebuild |
| A: Windows display topology | Physical display + VDD extended display | VDD display path disabled; normal physical displays remain |
| B: Moonlight | Streaming | Exited |
| A/B: OTI KVM + cross-screen | Disabled | Restored according to user settings |
| A/B: text/image/file clipboard | Disabled | Restored according to user settings |

## Hotkeys

Mode toggle:

```text
Ctrl + Alt + F11
```

Moonlight client shortcut for ending the current stream:

```text
Ctrl + Alt + Shift + Q
```

In normal operation, the user does not need to press the Moonlight shortcut manually on PC B. When PC A switches back to OTI mode, OTI-Link sends a Lane 1 workflow command to B, and B injects the shortcut locally.

## Starting VDD

The VDD instance ID verified on the current development machine is:

```text
ROOT\DISPLAY\0001
```

After Windows reboot, if the VDD device is disabled, OTI-Link's **Start Virtual Display** button requests UAC and performs the equivalent of:

```powershell
pnputil /enable-device "ROOT\DISPLAY\0001"
```

The instance ID is machine-specific and should be changed in settings when used on another PC.

The Start Virtual Display button only ensures that the VDD device is available. It does not by itself enter Moonlight display mode.

## Entering Moonlight Display Mode

PC A state flow:

```text
User selects Moonlight mode
        ↓
Immediately gate OTI KVM / cross-screen / clipboard
        ↓
Verify PC B is online
        ↓
Verify VDD is enabled
        ↓
If needed: UAC + pnputil
        ↓
SetDisplayConfig: enable extended display topology
        ↓
QueryDisplayConfig: verify VDD path is active
        ↓
Lane 1: tell B to start Moonlight
        ↓
Wait for matching-generation ACK
        ↓
Stable(Moonlight)
```

Default Moonlight command on PC B:

```text
"%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe" stream "{peer}" "Desktop"
```

`{peer}` is replaced with PC A's Windows computer name. If B cannot resolve that name, configure a fixed IP address or hostname instead.

Entering Moonlight mode requires PC B to be online. If any step fails, the state machine rolls back toward OTI mode so that Moonlight and OTI KVM are not left active at the same time.

## Returning to OTI Mode

```text
A tells B: enter OTI mode
        ↓
B brings the Moonlight window to the foreground
        ↓
B injects Ctrl+Alt+Shift+Q
        ↓
Wait for Moonlight to exit
        ↓
If a Moonlight process started by OTI is still alive after timeout,
terminate that owned process
        ↓
B sends ACK
        ↓
A disables the VDD display path
        ↓
Verify the virtual display is no longer part of the active desktop
        ↓
Restore OTI KVM / cross-screen / clipboard
        ↓
Stable(OTI)
```

FIX15 prefers disabling only the VDD display path rather than blindly calling `DisplaySwitch /internal`, so unrelated physical monitors are not unnecessarily disabled. A broader topology fallback may be used only when needed.

## Unexpected Moonlight Exit

PC B periodically checks the Moonlight process.

Moonlight may end because:

- The user exits it manually on B
- The network drops
- Sunshine ends the session
- Moonlight exits or crashes

B then performs:

```text
Restore local OTI gate
        ↓
CTRL_WORKFLOW_EVENT → A
        ↓
A desired = OTI
        ↓
A disables the VDD display path
        ↓
The mouse can no longer disappear into an invisible virtual display
        ↓
Restore OTI KVM / file / clipboard behavior
```

## FIX15 State-Machine Principles

- **Desired state + reconcile**: buttons and hotkeys change only `desired`; a worker reconciles reality toward that state.
- **Single writer**: A's worker owns A-side VDD/display changes; B's worker owns Moonlight process control.
- **Fail closed**: whenever the system is not stably in OTI mode, KVM/cross-screen/clipboard remain gated.
- **A is authoritative, B follows**: A can always return locally to OTI mode even if USB is disconnected; B is synchronized later.
- **Reconnect never auto-starts Moonlight**: reconnect performs validation only. A new Moonlight stream requires explicit user action.
- **Reality wins**: if A starts and discovers an active VDD display path, it initially treats that as a Moonlight-like state and validates B. If B is not streaming, A converges back to OTI.
- **Generation protects against stale messages**: late or duplicated ACKs cannot overwrite the current transition.
- **Rapid repeated hotkeys are safe**: transitions are debounced and each step checks the latest desired state.

## FIX15 Workflow Protocol

Current protocol version:

```text
PROTOCOL_VERSION = 14
```

Workflow messages:

| Kind | Direction | Meaning |
|---|---|---|
| `74 CTRL_WORKFLOW_MODE` | A → B | Request Moonlight or OTI mode |
| `75 CTRL_WORKFLOW_ACK` | B → A | Result and Moonlight-running state |
| `76 CTRL_WORKFLOW_EVENT` | B → A | Asynchronous event such as Moonlight ending |

Both PCs must run the same protocol version.

---

## Requirements

### Windows

Recommended:

```text
Windows 10 / Windows 11
```

### WinFsp

WinFsp is required for the remote filesystem.

Typical development installation path:

```text
C:\Program Files (x86)\WinFsp\
```

### Rust

Install stable Rust through rustup.

Verify:

```powershell
rustc --version
cargo --version
```

### LLVM / libclang

The Rust bindings used by the project require libclang.

Typical install path:

```text
C:\Program Files\LLVM\
```

PowerShell:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

CMD:

```cmd
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
```

### Wintun

Required only when using the OTI-Link virtual network adapter.

Place:

```text
wintun.dll
```

in the same directory as:

```text
oti_link_v10.exe
```

### Sunshine / Moonlight / VDD

Required only for FIX15 Moonlight display mode:

```text
PC A: Sunshine + Virtual Display Driver
PC B: Moonlight
```

Normal OTI mode does not require Moonlight or VDD.

---

## USB Driver

The OTi MI_05 transfer interface should use Microsoft's WinUSB driver.

Expected device/interface:

```text
Oti U3 Transfer Cable
USB\VID_0EA0&PID_7301&MI_05
```

Check with:

```powershell
Get-PnpDevice -PresentOnly |
Where-Object {
    $_.InstanceId -like 'USB\VID_0EA0&PID_7301*'
} |
Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
```

The RNDIS interface is not the main OTI-Link transport dependency.

---

## Building

Repository:

```text
https://github.com/haungdzh6/OTI_LINK.git
```

PowerShell:

```powershell
git clone https://github.com/haungdzh6/OTI_LINK.git
cd OTI_LINK

$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release --bin oti_link_v10
```

CMD:

```cmd
git clone https://github.com/haungdzh6/OTI_LINK.git
cd OTI_LINK

set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
cargo build --release --bin oti_link_v10
```

Output:

```text
target\release\oti_link_v10.exe
```

If CMD says:

```text
'cargo' is not recognized as an internal or external command
```

but `%USERPROFILE%\.cargo\bin\cargo.exe` exists, try:

```cmd
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
set "PATHEXT=.COM;.EXE;.BAT;.CMD"
cargo --version
```

Or bypass PATH entirely:

```cmd
"%USERPROFILE%\.cargo\bin\cargo.exe" build --release --bin oti_link_v10
```

---

## Running

Run the same build on both computers.

Before starting OTI-Link, close the original Smart Data Link software.

Launch:

```text
oti_link_v10.exe
```

A normal connection should produce log entries similar to:

```text
USB_DEVICE_FOUND
USB_MI05_CLAIMED interface=5
PEER_HELLO
PEER_READY
MOUNTED R:
```

The peer filesystem should then appear in Explorer.

---

## Recommended Setup Order

### Normal OTI Mode

Validate in this order:

1. USB connection
2. Remote filesystem mount
3. Small-file Explorer copy/paste
4. Text/image clipboard
5. KVM hotkey
6. Mouse cross-screen
7. Virtual network adapter, if needed
8. WinNAT sharing, if needed

### Moonlight Display Mode

Recommended preparation:

1. Install and configure Sunshine on PC A.
2. Install VDD on A and determine the VDD device instance ID.
3. Install Moonlight on B and manually verify that B can stream A's `Desktop`.
4. Run the same FIX15 build on A and B.
5. On A, enable **This PC is A**.
6. On B, configure the Moonlight launch command if necessary.
7. On A, use **Start Virtual Display** and verify that VDD can be enabled successfully.
8. Use `Ctrl+Alt+F11` or the settings UI to enter Moonlight display mode.
9. Use the same hotkey again to return to OTI mode.

---

## Virtual Network Verification

Inspect the adapter:

```powershell
Get-NetIPAddress -InterfaceAlias OTI-Link
Get-NetConnectionProfile -InterfaceAlias OTI-Link
```

Ping:

```powershell
ping 10.77.77.2
```

Inspect provider NAT:

```powershell
Get-NetNat
```

Inspect client default route:

```powershell
route print 0.0.0.0
```

iperf example:

```text
Peer:
iperf3 -s

Local:
iperf3 -c 10.77.77.2 -t 20 -P 4
```

---

## Logs

The tray menu can open the log file and log directory.

Common log keywords:

```text
USB_WAIT_DEVICE
USB_DEVICE_FOUND
USB_MI05_CLAIMED
PEER_HELLO
PEER_READY
SESSION_END
RECONNECT_BACKOFF_MS

MOUNTED
CLIP_TEXT_TX
CLIP_TEXT_RX
CLIP_IMAGE_TX
CLIP_IMAGE_RX

NET_STATUS
NET_HELPER_READY
NET_LINK_UP
NET_LINK_DOWN

KVM_READY
```

For FIX15 mode switching, also inspect workflow, VDD, Moonlight, display-topology, ACK, and transition-related log entries.

---

## Tests

File clipboard tests:

```powershell
cargo test --release --bin oti_link_v10 clipboard_file_tests
```

Directory enumeration tests:

```powershell
cargo test --release --bin oti_link_v10 directory_tests
```

---

## USB Reconnect Limitation

Some OTi cable revisions can enter a bad bridge/firmware state when one PC reboots while the other still powers the cable.

Symptoms may include:

```text
Unknown USB Device
Set Address Failed
```

or:

```text
USB\VID_0000&PID_0004
```

Software-only recovery attempts such as:

```text
USB hub port cycle
USB root hub restart
PnP rescan
xHCI controller restart
```

may not recover the device.

Some hardware revisions require physically disconnecting all cable interfaces so the bridge fully loses power, then reconnecting it.

This is a bridge/firmware-level failure. OTI-Link session reconnect cannot repair a USB device that no longer enumerates correctly.

---

## Three-Connector Cable Notes

Some OTi cables expose both Type-A and Type-C on one side.

Normally, use only one connector from that side:

```text
Type-C ↔ Type-A
```

or:

```text
Type-A ↔ Type-A
```

Do not assume the Type-A and Type-C connectors on the same side are intended to be connected to two hosts simultaneously.

---

## Known Limitations

- Windows is the primary supported platform.
- WinFsp is required for the remote filesystem.
- Wintun is required only for the virtual-network mode.
- Explorer MOVE-only clipboard operations are not synchronized as a cross-PC move; use copy/paste.
- The experimental FIX10 OLE cross-screen file drag feature was removed.
- Some OTi bridge failures require a full physical power cycle.
- Different OTi VID/PID or endpoint layouts may require source-code changes.
- Windows UIPI / secure desktop limits KVM input injection.
- WinNAT may conflict with an existing `NetNat`.
- Moonlight display mode depends on Sunshine, Moonlight, VDD, and Windows display configuration all working correctly.
- VDD instance IDs are machine-specific. `ROOT\DISPLAY\0001` is only the verified example for the current development PC.
- The current branch does not include OTI-Link's old DXGI display-streaming implementation; video is provided by Sunshine/Moonlight.

---

## Security Notes

OTI-Link allows one computer to access exported files on the other computer and can forward keyboard, mouse, clipboard, and virtual-network traffic.

Use it only between computers you trust.

The protocol is designed for a direct USB cable and should not be treated as an authenticated, encrypted protocol for untrusted networks.

Back up important data before testing writable remote-filesystem features.

---

## Version History

| Version | Main changes |
|---|---|
| FIX8 | Settings UI, configurable KVM hotkey, clipboard controls, clipboard contention fixes, KVM key-release fixes, disconnect handling, read-ahead/log improvements |
| FIX9 | Added Wintun virtual NIC and the private `10.77.77.0/24` OTI-Link network; network data moved over Lane 0 |
| FIX10 | Added bidirectional mouse cross-screen KVM and physical-display/layout mapping; experimental OLE cross-screen drag-and-drop was attempted |
| FIX11 | Removed the unsuccessful OLE drag-and-drop feature; added WinNAT Internet sharing; introduced the old OTI DXGI secondary-display implementation |
| FIX12 | Hardened display session/input/frame handling and thread lifecycle; fixed WinNAT configuration races |
| FIX13 | Added Lane0/Lane1 session isolation, Lane0 header CRC, USB transport-fatal reconnect, KVM mouse-flow control, and transactional WinNAT provider-ready/generation handling |
| FIX13 compilefix2 | Improved VDD/MTT1337 monitor enumeration, GDI→DXGI mapping, and diagnostics |
| FIX13 compilefix3 | Rust 2024 / Win32 FFI `unsafe` and ABI cleanup without functional changes |
| FIX13 no-display | Removed `display.rs` and OTI's built-in display streaming while keeping files, clipboard, KVM, cross-screen, virtual NIC, and WinNAT |
| FIX14 | Added Moonlight + VDD workflow control on top of the no-display branch |
| **FIX15** | Reworked workflow into a convergent, re-entrant, self-healing state machine; manages VDD display path, Moonlight lifecycle, reconnect behavior, and OTI gates; **current version, protocol 14** |

Historical references to OTI-Link's built-in secondary-display streaming describe older development stages only. The current FIX15 no-display line does not use that implementation.

---

## Project Status

OTI-Link remains experimental and reverse-engineering-oriented software.

Current focus areas include:

```text
USB communication
WinFsp remote filesystem
Explorer file copy/paste
Text/image/file clipboard
KVM
Mouse cross-screen
Virtual networking
WinNAT Internet sharing
Moonlight/VDD display workflow
USB automatic reconnect
```

Keep backups when testing with important data.

---

## License

Before publishing the repository as a formal open-source project, choose and add an explicit license such as MIT, Apache-2.0, or GPL-3.0.

This README does not automatically assign a license.

---

## Acknowledgements

OTI-Link uses or integrates with open-source projects/components including:

- Rust
- nusb
- WinFsp
- Wintun
- arboard
- crc32fast
- Sunshine
- Moonlight
- Virtual Display Driver

Thanks to the developers and maintainers of these projects.
