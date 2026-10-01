# OTI-Link

OTI-Link 是一个面向 Windows 的开源实验项目，用于通过 OTi USB 3.x PC-to-PC 传输线在两台电脑之间提供高速文件访问、资源管理器复制/粘贴、剪贴板同步、键鼠共享、跨屏 KVM、虚拟网卡以及 Moonlight/VDD 副屏模式联动。

> 本项目为独立开发项目，与 OTi、线缆制造商或原 Smart Data Link 软件无隶属关系。

## 当前版本

- 当前开发线：**OTI-Link 10.0-FIX15**
- 当前协议版本：**14**
- FIX15 两端必须使用相同版本。
- 当前分支基于 **FIX13 compilefix3 no-display**：OTI-Link 内置的 `display.rs` / DXGI 副屏传输已经移除。
- 当前“副屏”方案由 **Sunshine + Moonlight + Virtual Display Driver (VDD)** 负责画面；OTI-Link 负责 USB 通道、文件、KVM、剪贴板、网络和两种工作模式的协调。

---

## 主要功能

### 高速 USB 数据通道

OTI-Link 直接通过 WinUSB 使用 OTi USB 3.x 传输线的 MI_05 接口。

典型设备：

```text
VID: 0EA0
PID: 7301
```

典型 MI_05 endpoint：

```text
0x08  Bulk OUT
0x89  Bulk IN

0x0A  Bulk OUT
0x8B  Bulk IN
```

当前通道分工：

```text
Lane 0
OUT 0x08 → remote IN 0x89
高吞吐数据：文件数据、虚拟网卡数据

Lane 1
OUT 0x0A → remote IN 0x8B
低延迟控制：会话、RPC、剪贴板、KVM、网络状态、FIX15 工作流
```

FIX13 起，Lane0 数据帧带 session 和 header CRC；Lane1 文件 RPC 也绑定当前已认证 session。Lane0 出现不可恢复的读写错误时会结束整个 USB session 并重连，避免出现“Lane1 还在线但数据通道已经死掉”的半连接状态。

### 远程文件系统 / WinFsp

对端电脑会被挂载为 Windows 文件系统，例如：

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

支持：

- 文件读取、创建、写入、覆盖、删除
- 文件夹创建、删除
- 重命名 / 移动
- 修改文件大小和基础元数据
- 目录枚举
- 大文件顺序读取
- 自适应预读缓存
- 断线后挂起请求快速失败并进入重连

默认只导出本机固定磁盘。

### Windows 资源管理器复制 / 粘贴

OTI-Link 同步 Windows 文件剪贴板，实际文件内容通过 WinFsp 挂载盘和 Lane0 读取。

典型流程：

```text
A 电脑 Explorer：复制文件
        ↓
OTI-Link 同步文件列表
        ↓
B 电脑收到 CF_HDROP
        ↓
B 电脑 Explorer：粘贴
        ↓
Windows 读取远程挂载路径
        ↓
WinFsp → OTI-Link → USB Lane0
```

支持多文件、多目录和递归目录复制。

**当前使用 COPY 语义。跨屏 OLE 文件拖放在 FIX11 已回滚，不是当前功能；跨机文件请使用复制/粘贴。**

### 剪贴板同步

可分别控制：

- 文本
- 图片
- 文件

剪贴板使用变化检测、稳定等待和回环抑制，避免两端互相覆盖。Moonlight 副屏模式中，这三类同步会被 workflow gate 暂停；回到 OTI 模式后按用户原设置恢复。

### KVM / 鼠标跨屏

支持：

- 键盘转发
- Raw Input 鼠标
- 相对鼠标移动
- 鼠标按键和滚轮
- KVM 快捷键切换
- 双向鼠标跨屏
- 跨屏方向、错位量和屏幕布局
- session/断线输入复位
- 高频鼠标移动合并，避免 Lane1 被 500/1000 Hz 鼠标事件塞满

默认 KVM 快捷键：

```text
Ctrl + Alt + F12
```

正常跨屏切换保持 KeyDown/KeyUp 路由一致；断线或故障恢复时只释放由远端注入的输入。

> Windows `SendInput` 受 UIPI / 安全桌面限制，普通权限 OTI-Link 不能保证向更高完整性级别窗口或 Ctrl+Alt+Del 安全桌面注入输入。

### 虚拟网卡 / Wintun

启用后，两台电脑之间建立专用虚拟网卡，可用于：

- `ping`
- SMB
- 远程桌面
- 游戏局域网
- iperf
- 其它普通 TCP/UDP 程序

准备：

1. 下载 Wintun。
2. 把 `wintun.dll`（amd64）放到 `oti_link_v10.exe` 同目录。
3. 两台电脑在 OTI-Link 设置中启用虚拟网卡。
4. UAC 只用于提权的网络 helper；主 OTI-Link 不应整体以管理员身份运行，否则普通资源管理器可能看不到 WinFsp 挂载盘。

默认网络：

```text
OTI-Link
10.77.77.0/24
```

两端地址根据安装 ID 稳定分配为 `.1` / `.2`。

网络包走 Lane0，并与文件传输穿插发送。

### WinNAT 网络共享

可配置：

```text
A（有互联网）：把本机网络共享给对端
B：通过对端上网
```

Provider 使用 Windows WinNAT，不使用 ICS。

FIX13 起使用事务式配置：

- provider 配置成功后才发布 `provider_ready`
- client 收到 `provider_ready` 后才安装 OTI-Link 默认路由
- 每次配置带 generation，旧配置结果不会覆盖新配置
- provider/client 失败都会清理 NAT、forwarding、默认路由、DNS 等中间状态
- 启用前检查 OTI-Link `/24` 是否与本机已有 IPv4 地址/路由冲突

默认 DNS：

```text
223.5.5.5
119.29.29.29
```

可在配置中修改 `net_share_dns=`。

Windows 客户端环境中如果已经存在其它 `NetNat`（例如部分 Docker / Hyper-V 配置），可能与 OTI-Link NAT 冲突。

---

# FIX15：Moonlight 副屏模式 / OTI 模式

## 使用场景

A 电脑：

- 主电脑
- 运行 Sunshine
- 安装 Virtual Display Driver (VDD)
- 运行 OTI-Link，并勾选“本机是 A 电脑”

B 电脑：

- 笔记本 / 副屏
- 安装 Moonlight
- 运行 OTI-Link
- 不勾选“本机是 A 电脑”

FIX15 把副屏使用方式收敛成两个互斥目标状态。

| 项目 | Moonlight 副屏模式 | OTI 模式 |
|---|---|---|
| A：VDD 设备 | 已启用 | 保持启用，不反复 PnP 重建 |
| A：Windows 显示拓扑 | 显示器 1 + VDD 扩展屏 | 停用 VDD 显示路径，仅保留正常物理显示 |
| B：Moonlight | 串流运行 | 已退出 |
| A/B：OTI KVM + 跨屏 | 关闭 | 按用户设置恢复 |
| A/B：文本/图片/文件剪贴板 | 关闭 | 按用户设置恢复 |

## 快捷键

模式切换：

```text
Ctrl + Alt + F11
```

Moonlight 客户端退出当前串流：

```text
Ctrl + Alt + Shift + Q
```

正常使用时不需要用户在 B 上手动按退出快捷键；A 端切到 OTI 模式后，OTI-Link 会通过 Lane1 通知 B，由 B 端注入该快捷键。

## VDD 启动

你的机器当前已验证 VDD 实例 ID：

```text
ROOT\DISPLAY\0001
```

Windows 重启后如果 VDD 处于 Disabled，OTI-Link 的“启动虚拟显示器”按钮会申请 UAC，并执行等价操作：

```powershell
pnputil /enable-device "ROOT\DISPLAY\0001"
```

该 Instance ID 是机器相关配置，换电脑后应在设置中修改。

“启动虚拟显示器”按钮只负责确保 VDD 设备启动，不代表立即进入 Moonlight 副屏模式。

## 进入 Moonlight 副屏模式

A 端状态机：

```text
用户选择 Moonlight
        ↓
立即关闭 OTI KVM / 跨屏 / 剪贴板 gate
        ↓
确认 B 在线
        ↓
确认 VDD 已启用
        ↓
必要时 UAC + pnputil
        ↓
SetDisplayConfig：启用扩展显示
        ↓
QueryDisplayConfig：确认 VDD 路径已经活动
        ↓
Lane1 通知 B 启动 Moonlight
        ↓
等待匹配 generation 的 ACK
        ↓
Stable(Moonlight)
```

B 端默认 Moonlight 命令：

```text
"%ProgramFiles%\Moonlight Game Streaming\Moonlight.exe" stream "{peer}" "Desktop"
```

`{peer}` 会替换为 A 的 Windows 计算机名。若 B 无法解析该名称，可以在设置里改成固定 IP / 主机名。

进入 Moonlight 模式必须有 B 在线。任何步骤失败都会回滚到 OTI 模式，避免出现 Moonlight 与 OTI KVM 同时活动。

## 返回 OTI 模式

```text
A 通知 B：进入 OTI
        ↓
B 将 Moonlight 窗口置前
        ↓
B 注入 Ctrl+Alt+Shift+Q
        ↓
等待 Moonlight 退出
        ↓
若 OTI 自己启动的 Moonlight 5 秒仍未退出，则结束该进程
        ↓
B ACK
        ↓
A 停用 VDD 的显示路径
        ↓
校验虚拟屏不再参与桌面
        ↓
恢复 OTI KVM / 跨屏 / 剪贴板
        ↓
Stable(OTI)
```

FIX15 优先精确停用 VDD 显示路径，而不是无条件使用 `DisplaySwitch /internal`，以免误伤其它真实物理显示器；必要时才退回系统拓扑切换。

## Moonlight 意外退出

B 每约 300 ms 检查 Moonlight 进程。

以下情况导致 Moonlight 结束：

- 用户在 B 手动退出
- 网络断开
- Sunshine 结束串流
- Moonlight 自身退出

B 会：

```text
恢复本机 OTI gate
        ↓
CTRL_WORKFLOW_EVENT → A
        ↓
A desired = OTI
        ↓
A 自动停用 VDD 显示路径
        ↓
鼠标不再进入不可见虚拟屏
        ↓
恢复 OTI KVM / 文件与剪贴板
```

## FIX15 状态机原则

- **期望状态 + reconcile**：按钮和快捷键只修改 `desired`，工作线程负责把真实系统收敛到目标状态。
- **单写者**：A 的 worker 负责 A 的 VDD/显示拓扑；B 的 worker 负责 Moonlight。
- **fail-closed**：只要不处于稳定 OTI 模式，KVM / 跨屏 / 剪贴板默认关闭。
- **A 主、B 跟随**：USB 断开时 A 仍可本地切回 OTI；重连后再同步 B。
- **重连不会自动启动 Moonlight**：重连时只校验；只有用户操作才能启动新的 Moonlight 串流。
- **以现实状态为准**：A 启动时若发现 VDD 显示路径仍活动，会先按 Moonlight 状态处理，再与 B 校验；若 B 不在串流则自动回 OTI。
- **generation 防旧消息**：迟到/重复 ACK 不会覆盖当前切换。
- **快速连按**：快捷键有去抖，进行中的切换在步骤边界检查最新 `desired`。

## FIX15 工作流协议

当前协议版本：

```text
PROTOCOL_VERSION = 14
```

新增/使用：

| kind | 方向 | 含义 |
|---|---|---|
| `74 CTRL_WORKFLOW_MODE` | A → B | 请求 Moonlight / OTI 模式 |
| `75 CTRL_WORKFLOW_ACK` | B → A | 模式处理结果 / Moonlight 是否运行 |
| `76 CTRL_WORKFLOW_EVENT` | B → A | Moonlight 已结束等异步事件 |

两台电脑必须同时升级到当前协议版本。

---

## 安装要求

### Windows

建议：

```text
Windows 10 / Windows 11
```

### WinFsp

远程文件系统需要 WinFsp。

开发时默认路径：

```text
C:\Program Files (x86)\WinFsp\
```

### Rust

安装 stable Rust / rustup。

验证：

```powershell
rustc --version
cargo --version
```

### LLVM / libclang

Rust 绑定构建需要 libclang。

常见安装目录：

```text
C:\Program Files\LLVM\
```

PowerShell：

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

CMD：

```cmd
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
```

### Wintun

只有使用 OTI-Link 虚拟网卡时需要。

把：

```text
wintun.dll
```

放到：

```text
oti_link_v10.exe
```

同一目录。

### Sunshine / Moonlight / VDD

只有使用 FIX15 Moonlight 副屏模式时需要：

```text
A：Sunshine + Virtual Display Driver
B：Moonlight
```

OTI 模式本身不依赖 Moonlight/VDD。

---

## USB 驱动

OTi 的 MI_05 传输接口应使用 Microsoft WinUSB。

预期：

```text
Oti U3 Transfer Cable
USB\VID_0EA0&PID_7301&MI_05
```

检查：

```powershell
Get-PnpDevice -PresentOnly |
Where-Object {
    $_.InstanceId -like 'USB\VID_0EA0&PID_7301*'
} |
Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
```

RNDIS 不是 OTI-Link 主要传输依赖。

---

## 编译

仓库：

```text
https://github.com/haungdzh6/OTI_LINK.git
```

PowerShell：

```powershell
git clone https://github.com/haungdzh6/OTI_LINK.git
cd OTI_LINK

$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
cargo build --release --bin oti_link_v10
```

CMD：

```cmd
git clone https://github.com/haungdzh6/OTI_LINK.git
cd OTI_LINK

set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
cargo build --release --bin oti_link_v10
```

输出：

```text
target\release\oti_link_v10.exe
```

如果 CMD 提示：

```text
'cargo' 不是内部或外部命令
```

但 `%USERPROFILE%\.cargo\bin\cargo.exe` 存在，可先：

```cmd
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
set "PATHEXT=.COM;.EXE;.BAT;.CMD"
cargo --version
```

也可直接：

```cmd
"%USERPROFILE%\.cargo\bin\cargo.exe" build --release --bin oti_link_v10
```

---

## 运行

两台电脑必须运行相同版本。

启动前关闭原 Smart Data Link 软件，然后两端运行：

```text
oti_link_v10.exe
```

正常连接日志大致包含：

```text
USB_DEVICE_FOUND
USB_MI05_CLAIMED interface=5
PEER_HELLO
PEER_READY
MOUNTED R:
```

随后资源管理器中应出现远程挂载盘。

---

## 配置建议

### 普通 OTI 模式

建议先验证：

1. USB 连接
2. 远程盘挂载
3. 小文件复制/粘贴
4. 文本/图片剪贴板
5. KVM 快捷键
6. 鼠标跨屏
7. 虚拟网卡（如需要）
8. WinNAT（如需要）

### Moonlight 副屏模式

建议按顺序准备：

1. A 安装并配置 Sunshine。
2. A 安装 VDD，并确认 VDD 设备实例 ID。
3. B 安装 Moonlight，并先手工验证能连接 A 的 `Desktop`。
4. A/B 同时运行 FIX15。
5. A 勾选“本机是 A 电脑”。
6. B 配好 Moonlight 启动命令。
7. A 点“启动虚拟显示器”，确认 VDD 可正常启用。
8. A 使用 `Ctrl+Alt+F11` 或设置按钮进入 Moonlight 副屏模式。
9. 再次使用快捷键返回 OTI 模式。

---

## 虚拟网卡验证

查看接口：

```powershell
Get-NetIPAddress -InterfaceAlias OTI-Link
Get-NetConnectionProfile -InterfaceAlias OTI-Link
```

Ping：

```powershell
ping 10.77.77.2
```

WinNAT provider：

```powershell
Get-NetNat
```

客户端默认路由：

```powershell
route print 0.0.0.0
```

iperf 示例：

```text
对端：
iperf3 -s

本机：
iperf3 -c 10.77.77.2 -t 20 -P 4
```

---

## 日志

托盘菜单可打开日志和日志目录。

常见关键词：

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

FIX15 模式切换时还应关注 workflow / VDD / Moonlight 相关状态日志和错误提示。

---

## 测试

文件剪贴板测试：

```powershell
cargo test --release --bin oti_link_v10 clipboard_file_tests
```

目录枚举测试：

```powershell
cargo test --release --bin oti_link_v10 directory_tests
```

---

## USB 重连限制

部分 OTi 线缆在一端重启、另一端仍给桥芯片供电时，可能进入：

```text
Unknown USB Device
Set Address Failed
```

或：

```text
USB\VID_0000&PID_0004
```

测试中以下软件操作不一定能恢复：

```text
USB hub port cycle
USB root hub restart
PnP rescan
xHCI controller restart
```

某些硬件版本需要把线缆所有接口都物理断开，完全掉电后再连接。

这属于桥芯片/固件层状态，OTI-Link 的会话重连无法修复已经无法正常枚举的 USB 设备。

---

## 三头线缆注意事项

部分 OTi 线缆一侧同时有 Type-A 和 Type-C。

正常使用时在该侧只接其中一个：

```text
Type-C ↔ Type-A
```

或：

```text
Type-A ↔ Type-A
```

不要默认认为同一侧 Type-A 和 Type-C 应同时接入两台主机。

---

## 已知限制

- 当前主要支持 Windows。
- WinFsp 为远程文件系统必需组件。
- Wintun 只在虚拟网卡模式需要。
- Explorer MOVE-only 文件剪贴板操作不作为跨机移动同步；使用复制/粘贴。
- FIX10 的 OLE 跨屏文件拖放已经回滚，不是当前功能。
- 某些 OTi 桥芯片锁死后需要完全断电恢复。
- 不同 OTi VID/PID 或 endpoint 布局可能需要改代码。
- Windows UIPI / 安全桌面限制 KVM 输入注入。
- WinNAT 可能与机器上已有的 NetNat 配置冲突。
- Moonlight 副屏模式依赖 Sunshine、Moonlight、VDD 和 Windows 显示配置均正常。
- VDD Instance ID 是机器相关值，默认示例 `ROOT\DISPLAY\0001` 不保证适用于其它电脑。
- 当前分支不包含 OTI-Link 自有 DXGI 副屏传输；画面由 Moonlight/Sunshine 负责。

---

## 安全说明

OTI-Link 会让一台电脑访问另一台电脑导出的文件，并可转发键鼠、剪贴板和虚拟网络数据。

只应在你信任的两台电脑之间使用。

当前协议面向直连 USB 线缆，不应视为面向不可信网络的加密认证协议。

测试可写远程文件系统前，请备份重要数据。

---

## 版本历史

| 版本 | 主要变化 |
|---|---|
| FIX8 | 新设置窗口；KVM 快捷键可配置；文本/图片/文件剪贴板开关；修复剪贴板争用、KVM 键释放、断线文件请求；优化预读和日志 |
| FIX9 | 新增 Wintun 虚拟网卡；OTI-Link 专用 `10.77.77.0/24` 网络；网络数据走 Lane0 |
| FIX10 | 新增双向鼠标跨屏 KVM、显示器布局/物理尺寸映射；曾尝试跨屏 OLE 文件拖放 |
| FIX11 | 回滚未成功的 OLE 文件拖放；加入 WinNAT 网络共享；曾加入 OTI 自带 DXGI 副屏扩展 |
| FIX12 | 加固副屏 session/input/frame 完整性和线程生命周期；修复 WinNAT 配置竞态 |
| FIX13 | Lane0/Lane1 session 隔离；Lane0 header CRC；USB fatal 重连；KVM 鼠标流控；WinNAT `provider_ready`/generation 事务；协议升级 |
| FIX13 compilefix2 | 修正 VDD/MTT1337 显示器枚举、GDI→DXGI 匹配和诊断 |
| FIX13 compilefix3 | Rust 2024 / Win32 FFI `unsafe` 与 ABI 清理，不改变功能 |
| FIX13 no-display | 移除 `display.rs` 和 OTI 自带副屏传输；保留文件、剪贴板、KVM、跨屏、虚拟网卡和 WinNAT |
| FIX14 | 在 no-display 基线上加入 Moonlight + VDD 工作模式联动和 Lane1 workflow 控制 |
| **FIX15** | 重写 workflow 为可收敛、可重入、可自愈状态机；自动处理 VDD 显示路径、Moonlight 生命周期、USB 重连和 OTI gate；**当前版本，协议 14** |

历史版本中的“OTI 自带副屏扩展”仅代表当时开发阶段，当前 FIX15 no-display 路线不再使用该实现。

---

## 项目状态

OTI-Link 仍属于实验性、逆向工程性质的软件。

当前核心方向包括：

```text
USB 通信
WinFsp 远程文件系统
资源管理器文件复制/粘贴
文本/图片/文件剪贴板
KVM
鼠标跨屏
虚拟网卡
WinNAT 网络共享
Moonlight/VDD 副屏工作流
USB 自动重连
```

在重要数据环境中测试前，请保留备份。

---

## License

仓库正式公开前请明确选择并加入开源许可证，例如 MIT、Apache-2.0 或 GPL-3.0。当前文档不替项目自动指定许可证。

---

## Acknowledgements

OTI-Link 使用或集成的开源项目/组件包括：

- Rust
- nusb
- WinFsp
- Wintun
- arboard
- crc32fast
- Sunshine
- Moonlight
- Virtual Display Driver

感谢这些项目的开发者和维护者。
