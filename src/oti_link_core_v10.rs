#![cfg(windows)]
#![allow(dead_code)]
#![allow(non_snake_case)]

use arboard::{Clipboard, ImageData};
use crc32fast::hash as crc32;
use notify::{recommended_watcher, RecursiveMode, Watcher};
use nusb::{io::{EndpointRead, EndpointWrite}, transfer::{Bulk, In, Out}, MaybeFuture};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    env,
    ffi::{c_void, OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{ffi::{OsStrExt, OsStringExt}, fs::MetadataExt, io::AsRawHandle},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{atomic::{AtomicBool, AtomicU64, Ordering}, mpsc, Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use winfsp::{
    filesystem::{DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo, WideNameInfo},
    host::{FileSystemHost, FineGuard, VolumeParams},
    FspError, U16CStr,
};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

pub type AppResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Debug)]
pub struct AppConfig {
    pub version: &'static str,
    pub writable: bool,
    pub cache: bool,
    pub reconnect: bool,
    pub change_sync: bool,
    pub kvm: bool,
    pub clipboard: bool,
    pub tray: bool,
    pub write_through: bool,
    pub read_ahead: usize,
}
impl AppConfig {
    pub const fn v8_1() -> Self { Self { version:"8.1", writable:false, cache:false, reconnect:true, change_sync:true, kvm:false, clipboard:false, tray:false, write_through:true, read_ahead:16*1024*1024 } }
    pub const fn v9() -> Self { Self { version:"9.0", writable:true, cache:false, reconnect:true, change_sync:true, kvm:false, clipboard:false, tray:false, write_through:true, read_ahead:16*1024*1024 } }
    pub const fn v9_1() -> Self { Self { version:"9.1", writable:true, cache:true, reconnect:true, change_sync:true, kvm:false, clipboard:false, tray:false, write_through:false, read_ahead:64*1024*1024 } }
    pub const fn v10() -> Self { Self { version:"10.0-FIX6", writable:true, cache:true, reconnect:true, change_sync:true, kvm:true, clipboard:true, tray:true, write_through:false, read_ahead:64*1024*1024 } }
}

// USB 0EA0:7301 MI_05, measured topology.
const VID:u16=0x0EA0; const PID:u16=0x7301; const INTERFACE:u8=5;
const DATA_OUT:u8=0x08; const DATA_IN:u8=0x89; const CTRL_OUT:u8=0x0A; const CTRL_IN:u8=0x8B;
const DATA_BLOCK:usize=1024*1024; const DATA_BUFFER:usize=1024*1024; const DATA_TRANSFERS:usize=16;
const CTRL_BUFFER:usize=256*1024; const CTRL_TRANSFERS:usize=4;
const RPC_TIMEOUT:Duration=Duration::from_secs(30); const IO_TIMEOUT:Duration=Duration::from_secs(120); const WRITE_TIMEOUT:Duration=Duration::from_secs(120);
const HEARTBEAT_INTERVAL:Duration=Duration::from_secs(1); const PEER_STALE:Duration=Duration::from_secs(12); const HANDSHAKE_REOPEN:Duration=Duration::from_secs(12);
const CACHE_TTL:Duration=Duration::from_secs(1); const CLIPBOARD_POLL:Duration=Duration::from_millis(150);
const MAX_CTRL_PAYLOAD:usize=32*1024*1024;
const CTRL_MAGIC:[u8;4]=*b"OC10"; const DATA_MAGIC:[u8;4]=*b"OD10"; const PROTOCOL_VERSION:u8=10;
const CTRL_HEADER_SIZE:usize=48; const DATA_HEADER_SIZE:usize=40;

const CTRL_HELLO:u8=1; const CTRL_MANIFEST:u8=2; const CTRL_READY:u8=3; const CTRL_HEARTBEAT:u8=4; const CTRL_CHANGE:u8=5;
const CTRL_STAT_REQ:u8=16; const CTRL_STAT_RESP:u8=17; const CTRL_LIST_REQ:u8=18; const CTRL_LIST_RESP:u8=19; const CTRL_READ_REQ:u8=20;
const CTRL_CREATE_REQ:u8=21; const CTRL_CREATE_RESP:u8=22; const CTRL_WRITE_REQ:u8=23; const CTRL_WRITE_READY:u8=24; const CTRL_WRITE_RESP:u8=25;
const CTRL_FLUSH_REQ:u8=26; const CTRL_MUTATE_RESP:u8=27; const CTRL_RENAME_REQ:u8=28; const CTRL_DELETE_CHECK_REQ:u8=29; const CTRL_DELETE_COMMIT_REQ:u8=30;
const CTRL_SET_SIZE_REQ:u8=31; const CTRL_OVERWRITE_REQ:u8=32; const CTRL_SET_BASIC_REQ:u8=33; const CTRL_FS_ERROR:u8=39;
const CTRL_CLIP_TEXT:u8=48; const CTRL_CLIP_IMAGE:u8=49; const CTRL_KVM_KEY:u8=50; const CTRL_KVM_MOUSE:u8=51; const CTRL_KVM_RESET:u8=52; const CTRL_KVM_STATE:u8=53; const CTRL_KVM_MOUSE_SETTINGS:u8=54; const CTRL_CLIP_FILES:u8=55; const CTRL_SESSION_ENDING:u8=56; const CTRL_SESSION_END_ACK:u8=57;
const DATA_READ_CHUNK:u8=1; const DATA_READ_END:u8=2; const DATA_WRITE_CHUNK:u8=3; const DATA_WRITE_END:u8=4;

const STATUS_OBJECT_NAME_NOT_FOUND:i32=0xC0000034u32 as i32; const STATUS_NOT_A_DIRECTORY:i32=0xC0000103u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY:i32=0xC00000BAu32 as i32; const STATUS_ACCESS_DENIED:i32=0xC0000022u32 as i32;
const STATUS_DEVICE_NOT_CONNECTED:i32=0xC000009Du32 as i32; const STATUS_IO_TIMEOUT:i32=0xC00000B5u32 as i32;
const STATUS_INVALID_PARAMETER:i32=0xC000000Du32 as i32; const STATUS_IO_DEVICE_ERROR:i32=0xC0000185u32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY:i32=0xC0000101u32 as i32; const STATUS_OBJECT_NAME_COLLISION:i32=0xC0000035u32 as i32;
const FILE_ATTRIBUTE_READONLY:u32=1; const FILE_ATTRIBUTE_DIRECTORY:u32=0x10; const FILE_ATTRIBUTE_NORMAL:u32=0x80; const FILE_ATTRIBUTE_REPARSE_POINT:u32=0x400;
const FILE_READ_ATTRIBUTES:u32=0x0080; const FILE_WRITE_ATTRIBUTES:u32=0x0100;
const FILE_SHARE_READ:u32=0x00000001; const FILE_SHARE_WRITE:u32=0x00000002; const FILE_SHARE_DELETE:u32=0x00000004;
const OPEN_EXISTING:u32=3; const FILE_FLAG_BACKUP_SEMANTICS:u32=0x02000000;
const FILE_DIRECTORY_FILE:u32=1; const FSP_CLEANUP_DELETE:u32=1; const DRIVE_FIXED:u32=3;

// Minimal Win32 surface used by lifecycle, known folders, attributes, KVM and tray.
type BOOL=i32; type HANDLE=*mut c_void; type HWND=*mut c_void; type HHOOK=*mut c_void; type HICON=*mut c_void; type HINSTANCE=*mut c_void; type HMENU=*mut c_void; type WPARAM=usize; type LPARAM=isize; type LRESULT=isize;
#[repr(C)] #[derive(Clone,Copy)] struct GUID{data1:u32,data2:u16,data3:u16,data4:[u8;8]}
const FOLDERID_DESKTOP:GUID=GUID{data1:0xB4BFCC3A,data2:0xDB2C,data3:0x424C,data4:[0xB0,0x29,0x7F,0xE9,0x9A,0x87,0xC6,0x41]};
const FOLDERID_DOWNLOADS:GUID=GUID{data1:0x374DE290,data2:0x123F,data3:0x4565,data4:[0x91,0x64,0x39,0xC4,0x92,0x5E,0x46,0x7B]};
const FOLDERID_DOCUMENTS:GUID=GUID{data1:0xFDD39AD0,data2:0x238F,data3:0x46AF,data4:[0xAD,0xB4,0x6C,0x85,0x48,0x03,0x69,0xC7]};
#[repr(C)] #[derive(Clone,Copy,Default)] struct POINT{x:i32,y:i32}
#[repr(C)] struct MSG{hwnd:HWND,message:u32,wParam:WPARAM,lParam:LPARAM,time:u32,pt:POINT,lPrivate:u32}
#[repr(C)] struct KBDLLHOOKSTRUCT{vkCode:u32,scanCode:u32,flags:u32,time:u32,dwExtraInfo:usize}
#[repr(C)] struct MSLLHOOKSTRUCT{pt:POINT,mouseData:u32,flags:u32,time:u32,dwExtraInfo:usize}
#[repr(C)] #[derive(Clone,Copy)] struct MOUSEINPUT{dx:i32,dy:i32,mouseData:u32,dwFlags:u32,time:u32,dwExtraInfo:usize}
#[repr(C)] #[derive(Clone,Copy)] struct KEYBDINPUT{wVk:u16,wScan:u16,dwFlags:u32,time:u32,dwExtraInfo:usize}
#[repr(C)] union INPUTUNION{mi:MOUSEINPUT,ki:KEYBDINPUT}
#[repr(C)] struct INPUT{r#type:u32,u:INPUTUNION}
#[repr(C)] struct DROPFILES{p_files:u32,pt:POINT,f_nc:BOOL,f_wide:BOOL}
#[repr(C)] struct NOTIFYICONDATAW{cbSize:u32,hWnd:HWND,uID:u32,uFlags:u32,uCallbackMessage:u32,hIcon:HICON,szTip:[u16;128],dwState:u32,dwStateMask:u32,szInfo:[u16;256],uTimeoutOrVersion:u32,szInfoTitle:[u16;64],dwInfoFlags:u32,guidItem:GUID,hBalloonIcon:HICON}
#[repr(C)] struct WNDCLASSW{style:u32,lpfnWndProc:Option<unsafe extern "system" fn(HWND,u32,WPARAM,LPARAM)->LRESULT>,cbClsExtra:i32,cbWndExtra:i32,hInstance:HINSTANCE,hIcon:HICON,hCursor:HANDLE,hbrBackground:HANDLE,lpszMenuName:*const u16,lpszClassName:*const u16}
#[repr(C)] struct BITMAPINFOHEADER{size:u32,width:i32,height:i32,planes:u16,bit_count:u16,compression:u32,size_image:u32,x_pels_per_meter:i32,y_pels_per_meter:i32,colors_used:u32,colors_important:u32}
#[repr(C)] struct RGBQUAD{blue:u8,green:u8,red:u8,reserved:u8}
#[repr(C)] struct BITMAPINFO{header:BITMAPINFOHEADER,colors:[RGBQUAD;1]}
#[repr(C)] struct ICONINFO{is_icon:BOOL,x_hotspot:u32,y_hotspot:u32,mask:HANDLE,color:HANDLE}
#[repr(C)] struct FILE_ALLOCATION_INFO_RAW{allocation_size:i64}
#[repr(C)] struct FILE_END_OF_FILE_INFO_RAW{end_of_file:i64}
const FILE_ALLOCATION_INFO_CLASS:i32=5;
const FILE_END_OF_FILE_INFO_CLASS:i32=6;
#[repr(C)] struct FILE_BASIC_INFO_RAW{creation_time:i64,last_access_time:i64,last_write_time:i64,change_time:i64,file_attributes:u32}
const FILE_BASIC_INFO_CLASS:i32=0;
const INVALID_FILE_ATTRIBUTES:u32=0xFFFF_FFFF;
const INPUT_MOUSE:u32=0; const INPUT_KEYBOARD:u32=1; const WH_KEYBOARD_LL:i32=13; const WH_MOUSE_LL:i32=14;
const WM_KEYDOWN:u32=0x100; const WM_KEYUP:u32=0x101; const WM_SYSKEYDOWN:u32=0x104; const WM_SYSKEYUP:u32=0x105; const WM_MOUSEMOVE:u32=0x200;
const WM_LBUTTONDOWN:u32=0x201; const WM_LBUTTONUP:u32=0x202; const WM_RBUTTONDOWN:u32=0x204; const WM_RBUTTONUP:u32=0x205; const WM_MBUTTONDOWN:u32=0x207; const WM_MBUTTONUP:u32=0x208; const WM_MOUSEWHEEL:u32=0x20A; const WM_XBUTTONDOWN:u32=0x20B; const WM_XBUTTONUP:u32=0x20C; const WM_MOUSEHWHEEL:u32=0x20E;
const VK_F12:i32=0x7B; const VK_CONTROL:i32=0x11; const VK_MENU:i32=0x12; const VK_LCONTROL:i32=0xA2; const VK_RCONTROL:i32=0xA3; const VK_LMENU:i32=0xA4; const VK_RMENU:i32=0xA5; const HC_ACTION:i32=0; const PM_REMOVE:u32=1;
const LLKHF_EXTENDED:u32=0x01; const KEYEVENTF_EXTENDEDKEY:u32=0x0001; const KEYEVENTF_KEYUP:u32=0x0002; const KEYEVENTF_SCANCODE:u32=0x0008;
const MOUSEEVENTF_MOVE:u32=0x0001; const MOUSEEVENTF_LEFTDOWN:u32=0x0002; const MOUSEEVENTF_LEFTUP:u32=0x0004; const MOUSEEVENTF_RIGHTDOWN:u32=0x0008; const MOUSEEVENTF_RIGHTUP:u32=0x0010; const MOUSEEVENTF_MIDDLEDOWN:u32=0x0020; const MOUSEEVENTF_MIDDLEUP:u32=0x0040; const MOUSEEVENTF_XDOWN:u32=0x0080; const MOUSEEVENTF_XUP:u32=0x0100; const MOUSEEVENTF_WHEEL:u32=0x0800; const MOUSEEVENTF_HWHEEL:u32=0x1000; const MOUSEEVENTF_VIRTUALDESK:u32=0x4000; const MOUSEEVENTF_ABSOLUTE:u32=0x8000;
const SPI_GETMOUSE:u32=0x0003; const SPI_SETMOUSE:u32=0x0004; const SPI_GETMOUSESPEED:u32=0x0070; const SPI_SETMOUSESPEED:u32=0x0071; const SPIF_SENDCHANGE:u32=0x0002;
const ES_SYSTEM_REQUIRED:u32=0x00000001; const ES_DISPLAY_REQUIRED:u32=0x00000002; const ES_CONTINUOUS:u32=0x80000000;
const SM_XVIRTUALSCREEN:i32=76; const SM_YVIRTUALSCREEN:i32=77; const SM_CXVIRTUALSCREEN:i32=78; const SM_CYVIRTUALSCREEN:i32=79;
const KVM_TAG:usize=0x4F54494B564D3130usize; const NIM_ADD:u32=0; const NIM_MODIFY:u32=1; const NIM_DELETE:u32=2; const NIF_MESSAGE:u32=1; const NIF_ICON:u32=2; const NIF_TIP:u32=4; const IDI_APPLICATION:usize=32512; const TRAY_ICON_SIZE:i32=32;
const WM_DESTROY:u32=0x0002; const WM_QUERYENDSESSION:u32=0x0011; const WM_ENDSESSION:u32=0x0016; const WM_QUIT:u32=0x0012; const WM_CONTEXTMENU:u32=0x007B; const WM_LBUTTONDBLCLK:u32=0x0203; const WM_APP:u32=0x8000;
const WM_TRAY_CALLBACK:u32=WM_APP+10; const WM_TRAY_UPDATE:u32=WM_APP+11; const WM_TRAY_SHUTDOWN:u32=WM_APP+12;
const MF_STRING:u32=0x0000; const MF_SEPARATOR:u32=0x0800; const TPM_RIGHTBUTTON:u32=0x0002; const TPM_NONOTIFY:u32=0x0080; const TPM_RETURNCMD:u32=0x0100;
const SW_SHOWNORMAL:i32=1; const MB_OK:u32=0; const MB_ICONERROR:u32=0x10;
const TRAY_CMD_LOG:usize=1001; const TRAY_CMD_LOG_DIR:usize=1002; const TRAY_CMD_EXIT:usize=1003;
const CF_HDROP:u32=15; const GMEM_MOVEABLE:u32=0x0002; const GMEM_ZEROINIT:u32=0x0040;
const DROPEFFECT_COPY:u32=0x00000001; const DROPEFFECT_MOVE:u32=0x00000002;
const CLIP_FILES_MAX_ITEMS:usize=4096;

unsafe extern "system"{
 fn GetLogicalDrives()->u32; fn GetDriveTypeW(root:*const u16)->u32; fn GetDiskFreeSpaceExW(p:*const u16,a:*mut u64,t:*mut u64,f:*mut u64)->BOOL; fn GetVolumeInformationW(root:*const u16,label:*mut u16,label_n:u32,serial:*mut u32,max_component:*mut u32,flags:*mut u32,fsname:*mut u16,fs_n:u32)->BOOL; fn DefineDosDeviceW(flags:u32,device:*const u16,target:*const u16)->BOOL;
 fn SHGetKnownFolderPath(id:*const GUID,flags:u32,token:HANDLE,path:*mut *mut u16)->i32; fn CoTaskMemFree(p:*const c_void);
 fn CreateMutexW(a:*const c_void,owner:BOOL,name:*const u16)->HANDLE; fn GetLastError()->u32; fn CloseHandle(h:HANDLE)->BOOL;
 fn CreateFileW(name:*const u16,access:u32,share:u32,sa:*const c_void,creation:u32,flags:u32,template:HANDLE)->HANDLE;
 fn LoadLibraryW(path:*const u16)->HINSTANCE; fn SetDllDirectoryW(path:*const u16)->BOOL;
 fn SetFileAttributesW(path:*const u16,attrs:u32)->BOOL;
 fn SetFileInformationByHandle(h:HANDLE,class:i32,info:*const c_void,size:u32)->BOOL;
 fn SetWindowsHookExW(id:i32,proc:unsafe extern "system" fn(i32,WPARAM,LPARAM)->LRESULT,inst:HINSTANCE,tid:u32)->HHOOK; fn UnhookWindowsHookEx(h:HHOOK)->BOOL; fn CallNextHookEx(h:HHOOK,n:i32,w:WPARAM,l:LPARAM)->LRESULT;
 fn GetMessageW(msg:*mut MSG,hwnd:HWND,min:u32,max:u32)->i32; fn TranslateMessage(msg:*const MSG)->BOOL; fn DispatchMessageW(msg:*const MSG)->LRESULT; fn PostThreadMessageW(id:u32,msg:u32,w:WPARAM,l:LPARAM)->BOOL; fn PostQuitMessage(code:i32); fn GetCurrentThreadId()->u32; fn GetAsyncKeyState(vk:i32)->i16; fn GetSystemMetrics(index:i32)->i32; fn GetCursorPos(point:*mut POINT)->BOOL;
 fn SendInput(count:u32,inputs:*const INPUT,size:i32)->u32; fn SystemParametersInfoW(action:u32,param:u32,data:*mut c_void,win_ini:u32)->BOOL; fn SetThreadExecutionState(flags:u32)->u32; fn GetConsoleWindow()->HWND; fn LoadIconW(inst:HINSTANCE,name:*const u16)->HICON; fn Shell_NotifyIconW(msg:u32,data:*mut NOTIFYICONDATAW)->BOOL;
 fn CreateDIBSection(dc:HANDLE,info:*const BITMAPINFO,usage:u32,bits:*mut *mut c_void,section:HANDLE,offset:u32)->HANDLE; fn CreateBitmap(width:i32,height:i32,planes:u32,bits_per_pixel:u32,bits:*const c_void)->HANDLE; fn CreateIconIndirect(info:*const ICONINFO)->HICON; fn DeleteObject(object:HANDLE)->BOOL; fn DestroyIcon(icon:HICON)->BOOL;
 fn GetModuleHandleW(name:*const u16)->HINSTANCE; fn RegisterClassW(wc:*const WNDCLASSW)->u16; fn CreateWindowExW(ex_style:u32,class_name:*const u16,window_name:*const u16,style:u32,x:i32,y:i32,w:i32,h:i32,parent:HWND,menu:HMENU,instance:HINSTANCE,param:*mut c_void)->HWND; fn DestroyWindow(hwnd:HWND)->BOOL; fn DefWindowProcW(hwnd:HWND,msg:u32,w:WPARAM,l:LPARAM)->LRESULT;
 fn CreatePopupMenu()->HMENU; fn AppendMenuW(menu:HMENU,flags:u32,id:usize,text:*const u16)->BOOL; fn TrackPopupMenu(menu:HMENU,flags:u32,x:i32,y:i32,reserved:i32,hwnd:HWND,rect:*const c_void)->BOOL; fn DestroyMenu(menu:HMENU)->BOOL; fn SetForegroundWindow(hwnd:HWND)->BOOL;
 fn ShellExecuteW(hwnd:HWND,op:*const u16,file:*const u16,params:*const u16,dir:*const u16,show:i32)->HINSTANCE; fn MessageBoxW(hwnd:HWND,text:*const u16,caption:*const u16,flags:u32)->i32;
 fn OpenClipboard(hwnd:HWND)->BOOL; fn CloseClipboard()->BOOL; fn EmptyClipboard()->BOOL; fn IsClipboardFormatAvailable(format:u32)->BOOL; fn GetClipboardData(format:u32)->HANDLE; fn SetClipboardData(format:u32,mem:HANDLE)->HANDLE; fn GetClipboardSequenceNumber()->u32; fn RegisterClipboardFormatW(name:*const u16)->u32;
 fn GlobalAlloc(flags:u32,bytes:usize)->HANDLE; fn GlobalLock(mem:HANDLE)->*mut c_void; fn GlobalUnlock(mem:HANDLE)->BOOL; fn GlobalFree(mem:HANDLE)->HANDLE;
 fn DragQueryFileW(drop:HANDLE,index:u32,file:*mut u16,cch:u32)->u32;
}
const ERROR_ALREADY_EXISTS:u32=183; const DDD_REMOVE_DEFINITION:u32=2; const DDD_EXACT_MATCH_ON_REMOVE:u32=4; const DDD_NO_BROADCAST_SYSTEM:u32=8;

fn wide_null(s:impl AsRef<OsStr>)->Vec<u16>{s.as_ref().encode_wide().chain(std::iter::once(0)).collect()}
fn known_folder(id:&GUID)->io::Result<PathBuf>{unsafe{let mut p:*mut u16=null_mut();let hr=SHGetKnownFolderPath(id,0,null_mut(),&mut p);if hr<0||p.is_null(){return Err(io::Error::new(io::ErrorKind::NotFound,"known folder unavailable"));}let mut n=0;while *p.add(n)!=0{n+=1;}let out=PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p,n)));CoTaskMemFree(p.cast());Ok(out)}}
fn unix_ms()->u128{SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()}
fn new_id()->u64{(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64)^((std::process::id() as u64)<<32)}
fn reconnect_backoff()->Duration{Duration::from_millis(700+(new_id()%900))}
fn nt_error(s:i32)->FspError{FspError::NTSTATUS(s)}
fn normalize_virtual_path(p:&str)->String{let mut s=p.replace('/',"\\");if s.is_empty(){return "\\".into();}if !s.starts_with('\\'){s.insert(0,'\\');}while s.len()>1&&s.ends_with('\\'){s.pop();}s}
fn eq_ci(a:&str,b:&str)->bool{a.eq_ignore_ascii_case(b)}
fn u16c_to_string(p:&U16CStr)->String{p.to_string_lossy()}
fn round_alloc(n:u64)->u64{if n==0{0}else{((n+4095)/4096)*4096}}
fn rate(bytes:u64,d:Duration)->(f64,f64){let s=d.as_secs_f64();if s<=0.0{(0.0,0.0)}else{(bytes as f64/1048576.0/s,bytes as f64*8.0/s/1e9)}}
fn winfsp_dll_name()->&'static str{
    if cfg!(target_arch="x86_64"){"winfsp-x64.dll"}
    else if cfg!(target_arch="x86"){"winfsp-x86.dll"}
    else if cfg!(target_arch="aarch64"){"winfsp-a64.dll"}
    else{"winfsp-x64.dll"}
}

fn init_winfsp_runtime(logger:&Logger)->AppResult<winfsp::FspInit>{
    let dll=winfsp_dll_name();
    let mut candidates=Vec::<PathBuf>::new();

    if let Ok(exe)=env::current_exe(){
        if let Some(dir)=exe.parent(){
            candidates.push(dir.join(dll));
        }
    }
    if let Ok(p)=env::var("ProgramFiles(x86)"){
        candidates.push(PathBuf::from(p).join("WinFsp").join("bin").join(dll));
    }
    if let Ok(p)=env::var("ProgramFiles"){
        candidates.push(PathBuf::from(p).join("WinFsp").join("bin").join(dll));
    }

    // De-duplicate paths while preserving order.
    let mut seen=HashSet::<PathBuf>::new();
    candidates.retain(|p|seen.insert(p.clone()));

    for path in &candidates{
        logger.line(format!("WINFSP_DLL_CANDIDATE={} exists={}",path.display(),path.exists()));
        if !path.exists(){continue}

        let dir=path.parent().map(Path::to_path_buf);
        if let Some(ref dir)=dir{
            let wdir=wide_null(dir.as_os_str());
            let ok=unsafe{SetDllDirectoryW(wdir.as_ptr())};
            logger.line(format!("WINFSP_SET_DLL_DIRECTORY path={} result={}",dir.display(),ok));
        }

        let w=wide_null(path.as_os_str());
        let h=unsafe{LoadLibraryW(w.as_ptr())};

        if h.is_null(){
            let code=unsafe{GetLastError()};
            logger.line(format!("WINFSP_LOAD_FAILED path={} win32={}",path.display(),code));
            unsafe{let _=SetDllDirectoryW(null());}
            continue
        }

        logger.line(format!("WINFSP_DLL_LOADED={}",path.display()));

        let init=winfsp::winfsp_init();
        unsafe{let _=SetDllDirectoryW(null());}

        match init{
            Ok(token)=>{
                logger.line("WINFSP_INIT=OK");
                return Ok(token)
            }
            Err(e)=>{
                logger.line(format!("WINFSP_INIT_FAILED debug={:?} display={}",e,e));
                return Err(format!("WinFsp initialization failed after loading {}: {:?}",path.display(),e).into())
            }
        }
    }

    // Last attempt in case the user has already placed the DLL somewhere in
    // the normal Windows DLL search path.
    match winfsp::winfsp_init(){
        Ok(token)=>{
            logger.line("WINFSP_INIT=OK(search-path)");
            Ok(token)
        }
        Err(e)=>{
            logger.line(format!("WINFSP_INIT_FAILED debug={:?} display={}",e,e));
            Err(format!(
                "WinFsp runtime DLL '{}' could not be loaded. Checked executable directory and installed WinFsp bin folders. Error: {:?}",
                dll,e
            ).into())
        }
    }
}
fn put_u32(b:&mut[u8],o:usize,v:u32){b[o..o+4].copy_from_slice(&v.to_le_bytes())} fn put_u64(b:&mut[u8],o:usize,v:u64){b[o..o+8].copy_from_slice(&v.to_le_bytes())}
fn get_u32(b:&[u8],o:usize)->u32{u32::from_le_bytes(b[o..o+4].try_into().unwrap())} fn get_u64(b:&[u8],o:usize)->u64{u64::from_le_bytes(b[o..o+8].try_into().unwrap())}
fn enc_string(out:&mut Vec<u8>,s:&str)->AppResult<()>{let b=s.as_bytes();if b.len()>u32::MAX as usize{return Err("string too long".into())}out.extend_from_slice(&(b.len() as u32).to_le_bytes());out.extend_from_slice(b);Ok(())}
fn take<'a>(d:&mut &'a[u8],n:usize)->AppResult<&'a[u8]>{if d.len()<n{return Err("truncated payload".into())}let(a,b)=d.split_at(n);*d=b;Ok(a)}
fn dec_u32(d:&mut &[u8])->AppResult<u32>{Ok(u32::from_le_bytes(take(d,4)?.try_into().unwrap()))} fn dec_u64(d:&mut &[u8])->AppResult<u64>{Ok(u64::from_le_bytes(take(d,8)?.try_into().unwrap()))}
fn dec_string(d:&mut &[u8])->AppResult<String>{let n=dec_u32(d)? as usize;Ok(String::from_utf8(take(d,n)?.to_vec())?)}

#[derive(Clone)] struct Logger{inner:Arc<Mutex<(File,PathBuf)>>}
impl Logger{fn new(v:&str)->io::Result<Self>{fs::create_dir_all("logs")?;let host=env::var("COMPUTERNAME").unwrap_or_else(|_|"UNKNOWN".into());let p=PathBuf::from(format!("logs\\oti_link_{}_{}_{}.log",v.replace('.',"_"),host,unix_ms()));Ok(Self{inner:Arc::new(Mutex::new((File::create(&p)?,p)))})}fn line(&self,s:impl AsRef<str>){let s=s.as_ref();if unsafe{!GetConsoleWindow().is_null()}{println!("{s}");}if let Ok(mut g)=self.inner.lock(){let _=writeln!(g.0,"{s}");let _=g.0.flush();}}fn path(&self)->PathBuf{self.inner.lock().map(|g|g.1.clone()).unwrap_or_default()}}
macro_rules! logln{($l:expr,$($a:tt)*)=>{{$l.line(format!($($a)*));}}}

struct KeepAwake{logger:Logger}
impl KeepAwake{
 fn new(logger:Logger)->AppResult<Self>{
  let result=unsafe{SetThreadExecutionState(ES_CONTINUOUS|ES_SYSTEM_REQUIRED|ES_DISPLAY_REQUIRED)};
  if result==0{return Err(format!("SetThreadExecutionState(keep awake) failed win32={}",unsafe{GetLastError()}).into())}
  logger.line("POWER_KEEP_AWAKE=system+display");
  Ok(Self{logger})
 }
}
impl Drop for KeepAwake{
 fn drop(&mut self){
  if unsafe{SetThreadExecutionState(ES_CONTINUOUS)}==0{
   logln!(self.logger,"POWER_KEEP_AWAKE_CLEAR_FAILED win32={}",unsafe{GetLastError()});
  }else{self.logger.line("POWER_KEEP_AWAKE=CLEARED");}
 }
}

struct SingleInstance(HANDLE); unsafe impl Send for SingleInstance{} impl Drop for SingleInstance{fn drop(&mut self){unsafe{if !self.0.is_null(){let _=CloseHandle(self.0);}}}}
fn acquire_single_instance()->AppResult<SingleInstance>{let w=wide_null("Global\\OTI-Link-SingleInstance");let h=unsafe{CreateMutexW(null(),0,w.as_ptr())};if h.is_null(){return Err("CreateMutexW failed".into())}if unsafe{GetLastError()}==ERROR_ALREADY_EXISTS{unsafe{CloseHandle(h)};return Err("OTI-Link is already running".into())}Ok(SingleInstance(h))}

#[derive(Clone,Debug)] struct DriveInfo{letter:char,total:u64,free:u64}
#[derive(Clone,Debug)] struct Manifest{hostname:String,drives:Vec<DriveInfo>}
#[derive(Clone)] struct LocalExports{desktop:PathBuf,downloads:PathBuf,documents:PathBuf,drives:HashMap<char,PathBuf>}
fn disk_space(root:&str)->(u64,u64){let w=wide_null(root);let(mut a,mut t,mut f)=(0,0,0);let ok=unsafe{GetDiskFreeSpaceExW(w.as_ptr(),&mut a,&mut t,&mut f)};if ok==0{(0,0)}else{(t,f)}}
fn build_manifest()->AppResult<(Manifest,LocalExports)>{let hostname=env::var("COMPUTERNAME").unwrap_or_else(|_|"UNKNOWN-PC".into());let desktop=known_folder(&FOLDERID_DESKTOP)?;let downloads=known_folder(&FOLDERID_DOWNLOADS)?;let documents=known_folder(&FOLDERID_DOCUMENTS)?;let mask=unsafe{GetLogicalDrives()};let mut drives=vec![];let mut roots=HashMap::new();for i in 0..26u8{if mask&(1<<i)==0{continue}let letter=(b'A'+i) as char;let root=format!("{letter}:\\");if unsafe{GetDriveTypeW(wide_null(&root).as_ptr())}!=DRIVE_FIXED{continue}let(total,free)=disk_space(&root);drives.push(DriveInfo{letter,total,free});roots.insert(letter,PathBuf::from(root));}drives.sort_by_key(|d|d.letter);Ok((Manifest{hostname,drives},LocalExports{desktop,downloads,documents,drives:roots}))}
fn encode_manifest(m:&Manifest)->AppResult<Vec<u8>>{let mut o=vec![];enc_string(&mut o,&m.hostname)?;o.extend_from_slice(&(m.drives.len() as u16).to_le_bytes());for d in &m.drives{o.extend_from_slice(&(d.letter as u16).to_le_bytes());o.extend_from_slice(&d.total.to_le_bytes());o.extend_from_slice(&d.free.to_le_bytes());}Ok(o)}
fn decode_manifest(mut d:&[u8])->AppResult<Manifest>{let hostname=dec_string(&mut d)?;let n=u16::from_le_bytes(take(&mut d,2)?.try_into().unwrap()) as usize;let mut drives=vec![];for _ in 0..n{let l=u16::from_le_bytes(take(&mut d,2)?.try_into().unwrap());drives.push(DriveInfo{letter:char::from_u32(l as u32).ok_or("bad drive")?.to_ascii_uppercase(),total:dec_u64(&mut d)?,free:dec_u64(&mut d)?});}Ok(Manifest{hostname,drives})}

#[derive(Clone,Debug)] struct RemoteMeta{attributes:u32,size:u64,allocation_size:u64,creation_time:u64,last_access_time:u64,last_write_time:u64,change_time:u64}
impl RemoteMeta{fn is_dir(&self)->bool{self.attributes&FILE_ATTRIBUTE_DIRECTORY!=0}fn synthetic_dir()->Self{Self{attributes:FILE_ATTRIBUTE_DIRECTORY|FILE_ATTRIBUTE_READONLY,size:0,allocation_size:0,creation_time:0,last_access_time:0,last_write_time:0,change_time:0}}fn to_file_info(&self)->FileInfo{FileInfo{file_attributes:self.attributes,reparse_tag:0,allocation_size:self.allocation_size,file_size:self.size,creation_time:self.creation_time,last_access_time:self.last_access_time,last_write_time:self.last_write_time,change_time:self.change_time,index_number:0,hard_links:0,ea_size:0}}}
#[derive(Clone,Debug)] struct RemoteEntry{name:String,meta:RemoteMeta}
fn encode_meta(o:&mut Vec<u8>,m:&RemoteMeta){o.extend_from_slice(&m.attributes.to_le_bytes());for x in [m.size,m.allocation_size,m.creation_time,m.last_access_time,m.last_write_time,m.change_time]{o.extend_from_slice(&x.to_le_bytes());}}
fn decode_meta(d:&mut &[u8])->AppResult<RemoteMeta>{Ok(RemoteMeta{attributes:dec_u32(d)?,size:dec_u64(d)?,allocation_size:dec_u64(d)?,creation_time:dec_u64(d)?,last_access_time:dec_u64(d)?,last_write_time:dec_u64(d)?,change_time:dec_u64(d)?})}
fn encode_entries(es:&[RemoteEntry])->AppResult<Vec<u8>>{let mut o=vec![];o.extend_from_slice(&(es.len() as u32).to_le_bytes());for e in es{enc_string(&mut o,&e.name)?;encode_meta(&mut o,&e.meta);}if o.len()>MAX_CTRL_PAYLOAD{return Err("directory response too large".into())}Ok(o)}
fn decode_entries(mut d:&[u8])->AppResult<Vec<RemoteEntry>>{let n=dec_u32(&mut d)? as usize;let mut v=Vec::with_capacity(n);for _ in 0..n{v.push(RemoteEntry{name:dec_string(&mut d)?,meta:decode_meta(&mut d)?});}Ok(v)}

#[derive(Clone,Debug)] struct CtrlFrame{kind:u8,session:u64,request:u64,arg0:u64,arg1:u64,payload:Vec<u8>}
impl CtrlFrame{fn new(kind:u8,session:u64,request:u64,arg0:u64,arg1:u64,payload:Vec<u8>)->Self{Self{kind,session,request,arg0,arg1,payload}}}
fn write_ctrl(tx:&mut EndpointWrite<Bulk>,f:&CtrlFrame)->AppResult<()>{if f.payload.len()>MAX_CTRL_PAYLOAD{return Err("control payload too large".into())}let mut h=[0u8;CTRL_HEADER_SIZE];h[..4].copy_from_slice(&CTRL_MAGIC);h[4]=PROTOCOL_VERSION;h[5]=f.kind;put_u64(&mut h,8,f.session);put_u64(&mut h,16,f.request);put_u64(&mut h,24,f.arg0);put_u64(&mut h,32,f.arg1);put_u32(&mut h,40,f.payload.len() as u32);put_u32(&mut h,44,crc32(&f.payload));tx.write_all(&h)?;if !f.payload.is_empty(){tx.write_all(&f.payload)?;}tx.flush()?;Ok(())}
fn read_ctrl(rx:&mut EndpointRead<Bulk>)->AppResult<(CtrlFrame,u64)>{
 let mut m=[0u8;4];rx.read_exact(&mut m)?;let mut skipped=0u64;
 while m!=CTRL_MAGIC{
  m[0]=m[1];m[1]=m[2];m[2]=m[3];rx.read_exact(&mut m[3..4])?;skipped+=1;
  if skipped>16*1024*1024{return Err("control resync limit exceeded".into())}
 }
 let mut h=[0u8;CTRL_HEADER_SIZE];h[..4].copy_from_slice(&m);rx.read_exact(&mut h[4..])?;
 if h[4]!=PROTOCOL_VERSION{return Err(format!("protocol mismatch remote={} local={}",h[4],PROTOCOL_VERSION).into())}
 let n=get_u32(&h,40) as usize;if n>MAX_CTRL_PAYLOAD{return Err("control payload too large".into())}
 let mut p=vec![0;n];if n>0{rx.read_exact(&mut p)?;}
 if crc32(&p)!=get_u32(&h,44){return Err("control CRC mismatch".into())}
 Ok((CtrlFrame{kind:h[5],session:get_u64(&h,8),request:get_u64(&h,16),arg0:get_u64(&h,24),arg1:get_u64(&h,32),payload:p},skipped))
}
#[derive(Clone,Copy,Debug)] struct DataHeader{kind:u8,request:u64,offset:u64,seq:u64,len:u32,crc:u32}
impl DataHeader{fn bytes(self)->[u8;DATA_HEADER_SIZE]{let mut h=[0u8;DATA_HEADER_SIZE];h[..4].copy_from_slice(&DATA_MAGIC);h[4]=PROTOCOL_VERSION;h[5]=self.kind;put_u64(&mut h,8,self.request);put_u64(&mut h,16,self.offset);put_u64(&mut h,24,self.seq);put_u32(&mut h,32,self.len);put_u32(&mut h,36,self.crc);h}fn parse(h:&[u8;DATA_HEADER_SIZE])->AppResult<Self>{if h[..4]!=DATA_MAGIC||h[4]!=PROTOCOL_VERSION{return Err("data protocol mismatch".into())}Ok(Self{kind:h[5],request:get_u64(h,8),offset:get_u64(h,16),seq:get_u64(h,24),len:get_u32(h,32),crc:get_u32(h,36)})}}
fn read_data_header(rx:&mut EndpointRead<Bulk>)->AppResult<(DataHeader,u64)>{
 let mut m=[0u8;4];rx.read_exact(&mut m)?;let mut skipped=0u64;
 while m!=DATA_MAGIC{
  m[0]=m[1];m[1]=m[2];m[2]=m[3];rx.read_exact(&mut m[3..4])?;skipped+=1;
  if skipped>64*1024*1024{return Err("data resync limit exceeded".into())}
 }
 let mut h=[0u8;DATA_HEADER_SIZE];h[..4].copy_from_slice(&m);rx.read_exact(&mut h[4..])?;
 Ok((DataHeader::parse(&h)?,skipped))
}

fn open_interface(logger:&Logger,stop:&AtomicBool)->AppResult<nusb::Interface>{
 let mut deadline=Instant::now()+Duration::from_secs(15);
 let mut attempt=0u32;
 let mut absent_logged=false;
 loop{
  if stop.load(Ordering::Acquire){return Err("shutdown requested".into())}
  attempt+=1;
  let mut saw_device=false;
  let mut selected=None;
  for d in nusb::list_devices().wait()?{
   if d.vendor_id()!=VID||d.product_id()!=PID{continue}
   saw_device=true;
   let ready=d.interfaces().any(|i|i.interface_number()==INTERFACE);
   if ready{selected=Some(d);break}
  }

  let info=match selected{
   Some(x)=>{
    if absent_logged{logln!(logger,"USB_DEVICE_FOUND vid={VID:04X} pid={PID:04X} attempt={attempt}");}
    absent_logged=false;
    x
   },
   None=>{
    if !saw_device{
     // Cable absence/re-enumeration is a normal background state, not a
     // session failure. Keep this session alive and give a newly appearing
     // device a fresh 15-second MI_05/open/claim window.
     deadline=Instant::now()+Duration::from_secs(15);
     if !absent_logged||attempt%20==0{
      logln!(logger,"USB_WAIT_DEVICE vid={VID:04X} pid={PID:04X} attempt={attempt}");
      absent_logged=true;
     }
     for _ in 0..5{if stop.load(Ordering::Acquire){return Err("shutdown requested".into())}thread::sleep(Duration::from_millis(100));}
     continue
    }
    if Instant::now()>=deadline{return Err("OTI device present but MI_05 did not become ready".into())}
    logln!(logger,"USB_WAIT_MI05 attempt={attempt}");
    for _ in 0..5{if stop.load(Ordering::Acquire){return Err("shutdown requested".into())}thread::sleep(Duration::from_millis(100));}
    continue
   }
  };

  logln!(logger,"USB={} speed={:?} MI_05=ready attempt={attempt}",info.product_string().unwrap_or("SmartKMLink"),info.speed());

  let dev=match info.open().wait(){
   Ok(x)=>x,
   Err(e)=>{
    if Instant::now()>=deadline{return Err(format!("USB open failed after retries: {e}").into())}
    logln!(logger,"USB_OPEN_RETRY attempt={attempt} error={e}");
    for _ in 0..8{if stop.load(Ordering::Acquire){return Err("shutdown requested".into())}thread::sleep(Duration::from_millis(100));}
    continue
   }
  };

  match dev.claim_interface(INTERFACE).wait(){
   Ok(i)=>{
    logln!(logger,"USB_MI05_CLAIMED interface={INTERFACE} attempt={attempt}");
    return Ok(i)
   }
   Err(e)=>{
    drop(dev);
    if Instant::now()>=deadline{return Err(format!("MI_05 claim failed after retries: {e}").into())}
    logln!(logger,"USB_CLAIM_RETRY interface={INTERFACE} attempt={attempt} error={e}");
    for _ in 0..8{if stop.load(Ordering::Acquire){return Err("shutdown requested".into())}thread::sleep(Duration::from_millis(100));}
   }
  }
 }
}
fn data_writer(i:&nusb::Interface)->AppResult<EndpointWrite<Bulk>>{Ok(i.endpoint::<Bulk,Out>(DATA_OUT)?.writer(DATA_BUFFER).with_num_transfers(DATA_TRANSFERS).with_write_timeout(Duration::from_secs(8)))}
fn data_reader(i:&nusb::Interface)->AppResult<EndpointRead<Bulk>>{Ok(i.endpoint::<Bulk,In>(DATA_IN)?.reader(DATA_BUFFER).with_num_transfers(DATA_TRANSFERS).with_read_timeout(Duration::from_secs(2)))}
fn ctrl_writer(i:&nusb::Interface)->AppResult<EndpointWrite<Bulk>>{Ok(i.endpoint::<Bulk,Out>(CTRL_OUT)?.writer(CTRL_BUFFER).with_num_transfers(CTRL_TRANSFERS).with_write_timeout(Duration::from_secs(8)))}
fn ctrl_reader(i:&nusb::Interface)->AppResult<EndpointRead<Bulk>>{Ok(i.endpoint::<Bulk,In>(CTRL_IN)?.reader(CTRL_BUFFER).with_num_transfers(CTRL_TRANSFERS).with_read_timeout(Duration::from_secs(2)))}

// ============================================================
// Exported namespace and safe path resolution
// ============================================================

fn components(p:&str)->AppResult<Vec<String>>{let p=normalize_virtual_path(p);let mut out=vec![];for c in p.trim_start_matches('\\').split('\\'){if c.is_empty()||c=="."{continue}if c==".."||c.contains(':'){return Err("invalid path component".into())}out.push(c.to_string());}Ok(out)}
fn export_base<'a>(e:&'a LocalExports,parts:&[String])->AppResult<(&'a PathBuf,usize)>{if parts.is_empty(){return Err("synthetic root".into())}if eq_ci(&parts[0],"Desktop"){return Ok((&e.desktop,1))}if eq_ci(&parts[0],"Downloads"){return Ok((&e.downloads,1))}if eq_ci(&parts[0],"Documents"){return Ok((&e.documents,1))}if eq_ci(&parts[0],"Drives"){if parts.len()<2{return Err("synthetic drives root".into())}let l=parts[1].chars().next().ok_or("bad drive")?.to_ascii_uppercase();return e.drives.get(&l).map(|p|(p,2)).ok_or_else(||"drive not exported".into())}Err("unknown exported root".into())}
fn lexical_local(e:&LocalExports,v:&str)->AppResult<(PathBuf,PathBuf)>{let parts=components(v)?;let(base,skip)=export_base(e,&parts)?;let mut p=base.clone();for c in parts.iter().skip(skip){p.push(c)}Ok((base.clone(),p))}
fn safe_existing(e:&LocalExports,v:&str)->AppResult<PathBuf>{let(base,p)=lexical_local(e,v)?;let cb=fs::canonicalize(base)?;let cp=fs::canonicalize(&p)?;if !cp.starts_with(&cb){return Err("path escapes export root".into())}Ok(cp)}
fn safe_existing_io(e:&LocalExports,v:&str)->io::Result<PathBuf>{
 let(base,p)=lexical_local(e,v).map_err(|x|io::Error::new(io::ErrorKind::InvalidInput,x.to_string()))?;
 let cb=fs::canonicalize(&base)?;
 match fs::canonicalize(&p){
  Ok(cp)=>{
   if !cp.starts_with(&cb){return Err(io::Error::new(io::ErrorKind::PermissionDenied,"path escapes export root"))}
   Ok(cp)
  }
  Err(err)=>{
   // Preserve NotFound/PermissionDenied/etc. exactly. For a missing leaf,
   // also verify that the existing parent is still inside the export root.
   if err.kind()==io::ErrorKind::NotFound{
    if let Some(parent)=p.parent(){
     let cp=fs::canonicalize(parent)?;
     if !cp.starts_with(&cb){return Err(io::Error::new(io::ErrorKind::PermissionDenied,"path escapes export root"))}
    }
   }
   Err(err)
  }
 }
}
fn safe_create_target(e:&LocalExports,v:&str)->AppResult<PathBuf>{let(base,p)=lexical_local(e,v)?;let cb=fs::canonicalize(base)?;let parent=p.parent().ok_or("target has no parent")?;let cp=fs::canonicalize(parent)?;if !cp.starts_with(&cb){return Err("path escapes export root".into())}Ok(p)}
fn local_to_virtual(e:&LocalExports,p:&Path)->Option<String>{let cp=fs::canonicalize(p).ok().unwrap_or_else(||p.to_path_buf());for(name,base) in [("Desktop",&e.desktop),("Downloads",&e.downloads),("Documents",&e.documents)]{let cb=fs::canonicalize(base).ok().unwrap_or_else(||base.clone());if let Ok(tail)=cp.strip_prefix(&cb){let mut s=format!("\\{name}");for c in tail.components(){s.push('\\');s.push_str(&c.as_os_str().to_string_lossy());}return Some(s)}}for(l,base) in &e.drives{let cb=fs::canonicalize(base).ok().unwrap_or_else(||base.clone());if let Ok(tail)=cp.strip_prefix(&cb){let mut s=format!("\\Drives\\{l}");for c in tail.components(){s.push('\\');s.push_str(&c.as_os_str().to_string_lossy());}return Some(s)}}None}

fn meta_from_fs(m:&fs::Metadata)->RemoteMeta{RemoteMeta{attributes:if m.file_attributes()==0{if m.is_dir(){FILE_ATTRIBUTE_DIRECTORY}else{FILE_ATTRIBUTE_NORMAL}}else{m.file_attributes()},size:m.file_size(),allocation_size:round_alloc(m.file_size()),creation_time:m.creation_time(),last_access_time:m.last_access_time(),last_write_time:m.last_write_time(),change_time:m.last_write_time()}}
fn local_stat(e:&LocalExports,p:&str)->io::Result<RemoteMeta>{let p=safe_existing_io(e,p)?;Ok(meta_from_fs(&fs::metadata(p)?))}
fn local_list(e:&LocalExports,p:&str)->io::Result<Vec<RemoteEntry>>{let p=safe_existing_io(e,p)?;if !p.is_dir(){return Err(io::Error::new(io::ErrorKind::NotADirectory,"not a directory"))}let mut v=vec![];for ent in fs::read_dir(p)?{let ent=match ent{Ok(x)=>x,Err(_)=>continue};let m=match ent.metadata(){Ok(x)=>x,Err(_)=>continue};v.push(RemoteEntry{name:ent.file_name().to_string_lossy().to_string(),meta:meta_from_fs(&m)});}v.sort_by(|a,b|a.name.to_lowercase().cmp(&b.name.to_lowercase()));Ok(v)}
#[cfg(windows)] fn read_at(f:&File,o:u64,b:&mut[u8])->io::Result<usize>{use std::os::windows::fs::FileExt;f.seek_read(b,o)}
#[cfg(windows)] fn write_at(f:&File,o:u64,b:&[u8])->io::Result<usize>{use std::os::windows::fs::FileExt;f.seek_write(b,o)}
fn apply_attributes(path:&Path,attrs:u32)->io::Result<()>{if attrs==0{return Ok(())}let w=wide_null(path.as_os_str());if unsafe{SetFileAttributesW(w.as_ptr(),attrs)}==0{Err(io::Error::last_os_error())}else{Ok(())}}
fn set_file_allocation(f:&File,n:u64)->io::Result<()>{
 if n>i64::MAX as u64{return Err(io::Error::new(io::ErrorKind::InvalidInput,"allocation too large"))}
 let x=FILE_ALLOCATION_INFO_RAW{allocation_size:n as i64};
 let ok=unsafe{SetFileInformationByHandle(f.as_raw_handle() as HANDLE,FILE_ALLOCATION_INFO_CLASS,(&x as *const FILE_ALLOCATION_INFO_RAW).cast(),std::mem::size_of::<FILE_ALLOCATION_INFO_RAW>() as u32)};
 if ok==0{Err(io::Error::last_os_error())}else{Ok(())}
}
fn set_file_eof(f:&File,n:u64)->io::Result<()>{
 if n>i64::MAX as u64{return Err(io::Error::new(io::ErrorKind::InvalidInput,"file size too large"))}
 let x=FILE_END_OF_FILE_INFO_RAW{end_of_file:n as i64};
 let ok=unsafe{SetFileInformationByHandle(f.as_raw_handle() as HANDLE,FILE_END_OF_FILE_INFO_CLASS,(&x as *const FILE_END_OF_FILE_INFO_RAW).cast(),std::mem::size_of::<FILE_END_OF_FILE_INFO_RAW>() as u32)};
 if ok==0{Err(io::Error::last_os_error())}else{Ok(())}
}
fn apply_basic_info(path:&Path,attrs:u32,creation:u64,atime:u64,mtime:u64,change:u64)->io::Result<()>{
 fn as_i64(v:u64,name:&str)->io::Result<i64>{if v>i64::MAX as u64{Err(io::Error::new(io::ErrorKind::InvalidInput,format!("{name} out of range")))}else{Ok(v as i64)}}
 let effective_attrs=if attrs==INVALID_FILE_ATTRIBUTES{0}else if attrs==0{FILE_ATTRIBUTE_NORMAL}else{attrs};
 let info=FILE_BASIC_INFO_RAW{
  creation_time:as_i64(creation,"creation time")?,
  last_access_time:as_i64(atime,"last access time")?,
  last_write_time:as_i64(mtime,"last write time")?,
  change_time:as_i64(change,"change time")?,
  file_attributes:effective_attrs,
 };
 let w=wide_null(path.as_os_str());
 let h=unsafe{CreateFileW(
  w.as_ptr(),
  FILE_READ_ATTRIBUTES|FILE_WRITE_ATTRIBUTES,
  FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,
  null(),
  OPEN_EXISTING,
  FILE_FLAG_BACKUP_SEMANTICS,
  null_mut()
 )};
 if h as isize == -1{return Err(io::Error::last_os_error())}
 let ok=unsafe{SetFileInformationByHandle(
  h,
  FILE_BASIC_INFO_CLASS,
  (&info as *const FILE_BASIC_INFO_RAW).cast(),
  std::mem::size_of::<FILE_BASIC_INFO_RAW>() as u32
 )};
 let result=if ok==0{Err(io::Error::last_os_error())}else{Ok(())};
 unsafe{let _=CloseHandle(h);}
 result
}
fn io_status(e:&io::Error)->i32{match e.kind(){io::ErrorKind::NotFound=>STATUS_OBJECT_NAME_NOT_FOUND,io::ErrorKind::PermissionDenied=>STATUS_ACCESS_DENIED,io::ErrorKind::AlreadyExists=>STATUS_OBJECT_NAME_COLLISION,io::ErrorKind::InvalidInput=>STATUS_INVALID_PARAMETER,_=>STATUS_IO_DEVICE_ERROR}}

fn synthetic_meta()->RemoteMeta{RemoteMeta::synthetic_dir()}
fn synthetic_stat(m:&Manifest,p:&str)->Option<RemoteMeta>{let p=normalize_virtual_path(p);if p=="\\"||eq_ci(&p,"\\Desktop")||eq_ci(&p,"\\Downloads")||eq_ci(&p,"\\Documents")||eq_ci(&p,"\\Drives"){return Some(synthetic_meta())}let ps:Vec<_>=p.trim_start_matches('\\').split('\\').filter(|x|!x.is_empty()).collect();if ps.len()==2&&eq_ci(ps[0],"Drives"){let l=ps[1].chars().next()?.to_ascii_uppercase();if m.drives.iter().any(|d|d.letter==l){return Some(synthetic_meta())}}None}
fn synthetic_list(m:&Manifest,p:&str)->Option<Vec<RemoteEntry>>{let p=normalize_virtual_path(p);if p=="\\"{return Some(["Desktop","Downloads","Documents","Drives"].into_iter().map(|x|RemoteEntry{name:x.into(),meta:synthetic_meta()}).collect())}if eq_ci(&p,"\\Drives"){return Some(m.drives.iter().map(|d|RemoteEntry{name:d.letter.to_string(),meta:synthetic_meta()}).collect())}None}
fn writable_path(p:&str)->bool{let p=normalize_virtual_path(p);synthetic_stat(&Manifest{hostname:String::new(),drives:vec![]},&p).is_none()&&!eq_ci(&p,"\\Drives")&&!eq_ci(&p,"\\Desktop")&&!eq_ci(&p,"\\Downloads")&&!eq_ci(&p,"\\Documents")}

// ============================================================
// Stale mount state
// ============================================================

#[derive(Clone)] struct MountState{drive:String,label:String,peer:String,session:u64,pid:u32}
fn state_dir()->PathBuf{env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|_|PathBuf::from(".")).join("OTI-Link")}
fn state_file(v:&str)->PathBuf{state_dir().join(format!("{}.mount",v.replace('.',"_")))}
fn save_state(v:&str,s:&MountState)->io::Result<()>{fs::create_dir_all(state_dir())?;fs::write(state_file(v),format!("drive={}\nlabel={}\npeer={}\nsession={:016X}\npid={}\n",s.drive,s.label,s.peer,s.session,s.pid))}
fn remove_state(v:&str){let _=fs::remove_file(state_file(v));}
fn logical_drive_exists(d:&str)->bool{let b=d.as_bytes();if b.is_empty(){return false}let l=b[0].to_ascii_uppercase();if !(b'A'..=b'Z').contains(&l){return false}(unsafe{GetLogicalDrives()} & (1u32<<(l-b'A'))) != 0}
fn volume_signature(d:&str)->Option<(String,String)>{let root=wide_null(format!("{}\\",d.trim_end_matches('\\')));let mut label=vec![0u16;256];let mut fsn=vec![0u16;256];let ok=unsafe{GetVolumeInformationW(root.as_ptr(),label.as_mut_ptr(),label.len() as u32,null_mut(),null_mut(),null_mut(),fsn.as_mut_ptr(),fsn.len() as u32)};if ok==0{return None}fn z(v:&[u16])->String{let n=v.iter().position(|&x|x==0).unwrap_or(v.len());OsString::from_wide(&v[..n]).to_string_lossy().to_string()}Some((z(&label),z(&fsn)))}
fn cleanup_stale(v:&str,l:&Logger){let body=match fs::read_to_string(state_file(v)){Ok(x)=>x,Err(_)=>return};let mut map=HashMap::<String,String>::new();for line in body.lines(){if let Some((a,b))=line.split_once('='){map.insert(a.to_string(),b.to_string());}}let drive=map.get("drive").cloned().unwrap_or_default();let label=map.get("label").cloned().unwrap_or_default();logln!(l,"STALE_STATE drive={} label={}",drive,label);if !drive.is_empty()&&logical_drive_exists(&drive){match volume_signature(&drive){Some((cur,fsn)) if cur==label&&fsn.eq_ignore_ascii_case("OTILINK")=>{let w=wide_null(&drive);let mut ok=unsafe{DefineDosDeviceW(DDD_REMOVE_DEFINITION|DDD_EXACT_MATCH_ON_REMOVE|DDD_NO_BROADCAST_SYSTEM,w.as_ptr(),null())};if ok==0{ok=unsafe{DefineDosDeviceW(DDD_REMOVE_DEFINITION|DDD_NO_BROADCAST_SYSTEM,w.as_ptr(),null())};}logln!(l,"STALE_CLEANUP verified OTI mount remove_result={}",ok);},Some((cur,fsn))=>logln!(l,"STALE_CLEANUP skip: drive now belongs to label='{}' fs='{}'",cur,fsn),None=>l.line("STALE_CLEANUP skip: occupied drive could not be verified")}}remove_state(v);}

// ============================================================
// Control/data brokers
// ============================================================

type CtrlReply=Result<CtrlFrame,String>; type CtrlPending=Arc<Mutex<HashMap<u64,mpsc::Sender<CtrlReply>>>>;
enum DataPacket{Data{offset:u64,bytes:Vec<u8>},End{offset:u64},Error(String)}
type DataPending=Arc<Mutex<HashMap<u64,mpsc::Sender<DataPacket>>>>;
struct ActiveWrite{file:File,path:PathBuf,start:u64,expected:u64,received:u64,write_through:bool}
type ActiveWrites=Arc<Mutex<HashMap<u64,ActiveWrite>>>;

enum DataTxJob{ReadResponse{request:u64,path:PathBuf,offset:u64,len:u64},Upload{request:u64,offset:u64,data:Vec<u8>}}

#[derive(Clone)] struct RpcClient{session:u64,ctrl_tx:mpsc::Sender<CtrlFrame>,ctrl_pending:CtrlPending,data_pending:DataPending,data_tx:mpsc::Sender<DataTxJob>,next:Arc<AtomicU64>,connected:Arc<AtomicBool>}
impl RpcClient{
 fn id(&self)->u64{self.next.fetch_add(1,Ordering::Relaxed).max(1)}
 fn check(&self)->winfsp::Result<()>{if self.connected.load(Ordering::Acquire){Ok(())}else{Err(nt_error(STATUS_DEVICE_NOT_CONNECTED))}}
 fn request_with_id(&self,req:u64,kind:u8,arg0:u64,arg1:u64,payload:Vec<u8>,timeout:Duration)->AppResult<CtrlFrame>{if !self.connected.load(Ordering::Acquire){return Err("peer disconnected".into())}let(tx,rx)=mpsc::channel();self.ctrl_pending.lock().map_err(|_|"pending poisoned")?.insert(req,tx);if self.ctrl_tx.send(CtrlFrame::new(kind,self.session,req,arg0,arg1,payload)).is_err(){let _=self.ctrl_pending.lock().map(|mut m|m.remove(&req));return Err("Lane1 stopped".into())}let r=rx.recv_timeout(timeout);let _=self.ctrl_pending.lock().map(|mut m|m.remove(&req));match r{Ok(Ok(f))=>Ok(f),Ok(Err(e))=>Err(e.into()),Err(mpsc::RecvTimeoutError::Timeout)=>Err("RPC timeout".into()),Err(_)=>Err("RPC disconnected".into())}}
 fn request(&self,kind:u8,arg0:u64,arg1:u64,payload:Vec<u8>)->AppResult<CtrlFrame>{self.request_with_id(self.id(),kind,arg0,arg1,payload,RPC_TIMEOUT)}
 fn stat(&self,p:&str)->winfsp::Result<RemoteMeta>{self.check()?;let r=self.request(CTRL_STAT_REQ,0,0,p.as_bytes().to_vec()).map_err(app_fsp)?;if r.kind!=CTRL_STAT_RESP{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}decode_meta(&mut r.payload.as_slice()).map_err(app_fsp)}
 fn list(&self,p:&str)->winfsp::Result<Vec<RemoteEntry>>{self.check()?;let r=self.request(CTRL_LIST_REQ,0,0,p.as_bytes().to_vec()).map_err(app_fsp)?;if r.kind!=CTRL_LIST_RESP{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}decode_entries(&r.payload).map_err(app_fsp)}
 fn read_range(&self,p:&str,offset:u64,len:usize)->winfsp::Result<Vec<u8>>{self.check()?;let req=self.id();let(tx,rx)=mpsc::channel();self.data_pending.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.insert(req,tx);if self.ctrl_tx.send(CtrlFrame::new(CTRL_READ_REQ,self.session,req,offset,len as u64,p.as_bytes().to_vec())).is_err(){return Err(nt_error(STATUS_DEVICE_NOT_CONNECTED))}let mut out=Vec::with_capacity(len);let mut expected=offset;loop{match rx.recv_timeout(IO_TIMEOUT){Ok(DataPacket::Data{offset,bytes})=>{if offset!=expected{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}expected+=bytes.len() as u64;out.extend_from_slice(&bytes)},Ok(DataPacket::End{offset})=>{if offset!=expected{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}break},Ok(DataPacket::Error(e))=>{let _=self.data_pending.lock().map(|mut m|m.remove(&req));return Err(parse_remote_error(&e))},Err(mpsc::RecvTimeoutError::Timeout)=>return Err(nt_error(STATUS_IO_TIMEOUT)),Err(_)=>return Err(nt_error(STATUS_DEVICE_NOT_CONNECTED))}}let _=self.data_pending.lock().map(|mut m|m.remove(&req));Ok(out)}
 fn create(&self,p:&str,is_dir:bool,attrs:u32,allocation:u64)->winfsp::Result<RemoteMeta>{let mut q=vec![];enc_string(&mut q,p).map_err(app_fsp)?;q.extend_from_slice(&allocation.to_le_bytes());let r=self.request(CTRL_CREATE_REQ,is_dir as u64,attrs as u64,q).map_err(app_fsp)?;decode_meta(&mut r.payload.as_slice()).map_err(app_fsp)}
 fn overwrite(&self,p:&str,attrs:u32,replace_attrs:bool,allocation:u64)->winfsp::Result<RemoteMeta>{let mut q=vec![];enc_string(&mut q,p).map_err(app_fsp)?;q.extend_from_slice(&allocation.to_le_bytes());let r=self.request(CTRL_OVERWRITE_REQ,attrs as u64,replace_attrs as u64,q).map_err(app_fsp)?;decode_meta(&mut r.payload.as_slice()).map_err(app_fsp)}
 fn write_range(&self,p:&str,offset:u64,data:&[u8],to_eof:bool,write_through:bool)->winfsp::Result<(u32,RemoteMeta)>{self.check()?;let req=self.id();let(tx,rx)=mpsc::channel();self.ctrl_pending.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.insert(req,tx);let mut q=vec![];enc_string(&mut q,p).map_err(app_fsp)?;q.push(to_eof as u8);q.push(write_through as u8);self.ctrl_tx.send(CtrlFrame::new(CTRL_WRITE_REQ,self.session,req,offset,data.len() as u64,q)).map_err(|_|nt_error(STATUS_DEVICE_NOT_CONNECTED))?;let ready=rx.recv_timeout(RPC_TIMEOUT).map_err(|_|nt_error(STATUS_IO_TIMEOUT))?.map_err(|e|parse_remote_error(&e))?;if ready.kind!=CTRL_WRITE_READY{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}let actual=ready.arg0;self.data_tx.send(DataTxJob::Upload{request:req,offset:actual,data:data.to_vec()}).map_err(|_|nt_error(STATUS_DEVICE_NOT_CONNECTED))?;let done=rx.recv_timeout(WRITE_TIMEOUT).map_err(|_|nt_error(STATUS_IO_TIMEOUT))?.map_err(|e|parse_remote_error(&e))?;let _=self.ctrl_pending.lock().map(|mut m|m.remove(&req));if done.kind!=CTRL_WRITE_RESP{return Err(nt_error(STATUS_IO_DEVICE_ERROR))}let m=decode_meta(&mut done.payload.as_slice()).map_err(app_fsp)?;Ok((done.arg0 as u32,m))}
 fn mutate_path(&self,kind:u8,p:&str,arg0:u64,arg1:u64,extra:Option<&str>)->winfsp::Result<RemoteMeta>{let mut q=vec![];enc_string(&mut q,p).map_err(app_fsp)?;if let Some(x)=extra{enc_string(&mut q,x).map_err(app_fsp)?;}let r=self.request(kind,arg0,arg1,q).map_err(app_fsp)?;if r.payload.is_empty(){return Ok(RemoteMeta::synthetic_dir())}decode_meta(&mut r.payload.as_slice()).map_err(app_fsp)}
 fn flush(&self,p:&str)->winfsp::Result<RemoteMeta>{self.mutate_path(CTRL_FLUSH_REQ,p,0,0,None)}
 fn rename(&self,old:&str,new:&str,replace:bool)->winfsp::Result<RemoteMeta>{self.mutate_path(CTRL_RENAME_REQ,old,replace as u64,0,Some(new))}
 fn delete_check(&self,p:&str)->winfsp::Result<()> {self.mutate_path(CTRL_DELETE_CHECK_REQ,p,0,0,None).map(|_|())}
 fn delete_commit(&self,p:&str)->winfsp::Result<()> {self.mutate_path(CTRL_DELETE_COMMIT_REQ,p,0,0,None).map(|_|())}
 fn set_size(&self,p:&str,n:u64,set_allocation:bool)->winfsp::Result<RemoteMeta>{self.mutate_path(CTRL_SET_SIZE_REQ,p,n,set_allocation as u64,None)}
 fn set_basic(&self,p:&str,attrs:u32,creation:u64,at:u64,mt:u64,change:u64)->winfsp::Result<RemoteMeta>{let mut q=vec![];enc_string(&mut q,p).map_err(app_fsp)?;q.extend_from_slice(&creation.to_le_bytes());q.extend_from_slice(&at.to_le_bytes());q.extend_from_slice(&mt.to_le_bytes());q.extend_from_slice(&change.to_le_bytes());let r=self.request(CTRL_SET_BASIC_REQ,attrs as u64,0,q).map_err(app_fsp)?;decode_meta(&mut r.payload.as_slice()).map_err(app_fsp)}
}
fn parse_remote_error(s:&str)->FspError{if let Some(r)=s.strip_prefix("NTSTATUS:"){if let Some((h,_))=r.split_once(':'){if let Ok(v)=u32::from_str_radix(h,16){return nt_error(v as i32)}}}nt_error(STATUS_IO_DEVICE_ERROR)}
fn app_fsp(e:Box<dyn std::error::Error+Send+Sync>)->FspError{let s=e.to_string();if s.contains("timeout"){nt_error(STATUS_IO_TIMEOUT)}else if s.contains("disconnect"){nt_error(STATUS_DEVICE_NOT_CONNECTED)}else{parse_remote_error(&s)}}

// ============================================================
// Session events and server requests
// ============================================================

enum MainEvent{Activity{session:u64},Hello{session:u64,host:String},Manifest{session:u64,m:Manifest},Ready{session:u64,accepted:u64},Heartbeat{session:u64},Change{session:u64,path:String},ClipText{session:u64,text:String},ClipImage{session:u64,w:usize,h:usize,rgba:Vec<u8>},ClipFiles{session:u64,paths:Vec<String>},KvmKey{session:u64,payload:Vec<u8>},KvmMouse{session:u64,payload:Vec<u8>},KvmReset{session:u64},KvmState{session:u64,remote:bool},KvmMouseSettings{session:u64,payload:Vec<u8>},PeerSessionEnding{session:u64,reason:u64},PeerSessionEndAck{session:u64,accepted:u64},Fatal(String)}
enum ServerRequest{Stat(CtrlFrame),List(CtrlFrame),Read(CtrlFrame),Create(CtrlFrame),WriteBegin(CtrlFrame),Flush(CtrlFrame),Rename(CtrlFrame),DeleteCheck(CtrlFrame),DeleteCommit(CtrlFrame),SetSize(CtrlFrame),Overwrite(CtrlFrame),SetBasic(CtrlFrame)}

fn send_fs_error(tx:&mpsc::Sender<CtrlFrame>,session:u64,req:u64,status:i32,msg:impl AsRef<str>){let _=tx.send(CtrlFrame::new(CTRL_FS_ERROR,session,req,status as u32 as u64,0,msg.as_ref().as_bytes().to_vec()));}
fn ctrl_error_string(f:&CtrlFrame)->String{format!("NTSTATUS:{:08X}:{}",f.arg0 as u32,String::from_utf8_lossy(&f.payload))}

fn spawn_ctrl_writer(mut w:EndpointWrite<Bulk>,rx:mpsc::Receiver<CtrlFrame>,ev:mpsc::Sender<MainEvent>,alive:Arc<AtomicBool>)->thread::JoinHandle<()>{
 thread::spawn(move||loop{
  if !alive.load(Ordering::Acquire){break}
  let f=match rx.recv_timeout(Duration::from_millis(100)){
   Ok(x)=>x,
   Err(mpsc::RecvTimeoutError::Timeout)=>continue,
   Err(mpsc::RecvTimeoutError::Disconnected)=>break,
  };
  if let Err(e)=write_ctrl(&mut w,&f){
   if alive.load(Ordering::Acquire){let _=ev.send(MainEvent::Fatal(format!("Lane1 write: {e}")));}
   break
  }
 })
}
fn spawn_ctrl_reader(mut r:EndpointRead<Bulk>,pending:CtrlPending,data:DataPending,server:mpsc::Sender<ServerRequest>,ev:mpsc::Sender<MainEvent>,alive:Arc<AtomicBool>,logger:Logger)->thread::JoinHandle<()>{thread::spawn(move||loop{if !alive.load(Ordering::Acquire){break}let(f,skipped)=match read_ctrl(&mut r){Ok(x)=>x,Err(e)=>{let text=e.to_string().to_lowercase();if text.contains("timed out")||text.contains("timeout"){continue}if text.contains("control crc mismatch"){logln!(logger,"CTRL_CRC_DROP={e}");continue}if alive.load(Ordering::Acquire){let _=ev.send(MainEvent::Fatal(format!("Lane1 read: {e}")));}break}};if skipped>0{logln!(logger,"CTRL_RESYNC skipped_bytes={skipped}");}let _=ev.send(MainEvent::Activity{session:f.session});match f.kind{
 CTRL_STAT_RESP|CTRL_LIST_RESP|CTRL_CREATE_RESP|CTRL_WRITE_READY|CTRL_WRITE_RESP|CTRL_MUTATE_RESP=>{if let Some(tx)=pending.lock().ok().and_then(|m|m.get(&f.request).cloned()){let _=tx.send(Ok(f));}}
 CTRL_FS_ERROR=>{let s=ctrl_error_string(&f);if let Some(tx)=pending.lock().ok().and_then(|m|m.get(&f.request).cloned()){let _=tx.send(Err(s.clone()));}if let Some(tx)=data.lock().ok().and_then(|m|m.get(&f.request).cloned()){let _=tx.send(DataPacket::Error(s));}}
 CTRL_STAT_REQ=>{let _=server.send(ServerRequest::Stat(f));} CTRL_LIST_REQ=>{let _=server.send(ServerRequest::List(f));} CTRL_READ_REQ=>{let _=server.send(ServerRequest::Read(f));}
 CTRL_CREATE_REQ=>{let _=server.send(ServerRequest::Create(f));} CTRL_WRITE_REQ=>{let _=server.send(ServerRequest::WriteBegin(f));} CTRL_FLUSH_REQ=>{let _=server.send(ServerRequest::Flush(f));}
 CTRL_RENAME_REQ=>{let _=server.send(ServerRequest::Rename(f));} CTRL_DELETE_CHECK_REQ=>{let _=server.send(ServerRequest::DeleteCheck(f));} CTRL_DELETE_COMMIT_REQ=>{let _=server.send(ServerRequest::DeleteCommit(f));}
 CTRL_SET_SIZE_REQ=>{let _=server.send(ServerRequest::SetSize(f));} CTRL_OVERWRITE_REQ=>{let _=server.send(ServerRequest::Overwrite(f));} CTRL_SET_BASIC_REQ=>{let _=server.send(ServerRequest::SetBasic(f));}
 CTRL_HELLO=>{let _=ev.send(MainEvent::Hello{session:f.session,host:String::from_utf8_lossy(&f.payload).to_string()});}
 CTRL_MANIFEST=>match decode_manifest(&f.payload){Ok(m)=>{let _=ev.send(MainEvent::Manifest{session:f.session,m});},Err(e)=>{let _=ev.send(MainEvent::Fatal(format!("manifest: {e}")));}}
 CTRL_READY=>{let _=ev.send(MainEvent::Ready{session:f.session,accepted:f.arg0});} CTRL_HEARTBEAT=>{let _=ev.send(MainEvent::Heartbeat{session:f.session});}
 CTRL_CHANGE=>{let _=ev.send(MainEvent::Change{session:f.session,path:String::from_utf8_lossy(&f.payload).to_string()});}
 CTRL_CLIP_TEXT=>{let _=ev.send(MainEvent::ClipText{session:f.session,text:String::from_utf8_lossy(&f.payload).to_string()});}
 CTRL_CLIP_IMAGE=>{if f.payload.len()>=8{let w=u32::from_le_bytes(f.payload[0..4].try_into().unwrap()) as usize;let h=u32::from_le_bytes(f.payload[4..8].try_into().unwrap()) as usize;if let Some(expect)=w.checked_mul(h).and_then(|x|x.checked_mul(4)){if expect==f.payload.len()-8{let _=ev.send(MainEvent::ClipImage{session:f.session,w,h,rgba:f.payload[8..].to_vec()});}else{logln!(logger,"CLIP_IMAGE_DROP bad_length={} expected={}",f.payload.len()-8,expect);}}}}
 CTRL_CLIP_FILES=>{match decode_clip_files(&f.payload){Ok(paths)=>{let _=ev.send(MainEvent::ClipFiles{session:f.session,paths});},Err(e)=>logln!(logger,"CLIP_FILES_DROP={e}")}}
 CTRL_KVM_KEY=>{let _=ev.send(MainEvent::KvmKey{session:f.session,payload:f.payload});}
 CTRL_KVM_MOUSE=>{let _=ev.send(MainEvent::KvmMouse{session:f.session,payload:f.payload});}
 CTRL_KVM_RESET=>{let _=ev.send(MainEvent::KvmReset{session:f.session});}
 CTRL_KVM_STATE=>{let _=ev.send(MainEvent::KvmState{session:f.session,remote:f.arg0!=0});}
 CTRL_KVM_MOUSE_SETTINGS=>{let _=ev.send(MainEvent::KvmMouseSettings{session:f.session,payload:f.payload});}
 CTRL_SESSION_ENDING=>{let _=ev.send(MainEvent::PeerSessionEnding{session:f.session,reason:f.arg0});}
 CTRL_SESSION_END_ACK=>{let _=ev.send(MainEvent::PeerSessionEndAck{session:f.session,accepted:f.arg0});}
 _=>{}
}})}

// ============================================================
// Change watcher: only directories actually browsed are watched, plus known folders.
// ============================================================

fn spawn_change_watcher(exports:LocalExports,session:u64,ctrl:mpsc::Sender<CtrlFrame>,reg_rx:mpsc::Receiver<PathBuf>,logger:Logger,alive:Arc<AtomicBool>)->thread::JoinHandle<()>{thread::spawn(move||{
 let(tx,rx)=mpsc::channel();let mut watcher=match recommended_watcher(tx){Ok(w)=>w,Err(e)=>{logln!(logger,"WATCHER_INIT_ERROR={e}");return}};let mut watched=HashSet::<PathBuf>::new();
 for p in [&exports.desktop,&exports.downloads,&exports.documents]{if watcher.watch(p,RecursiveMode::Recursive).is_ok(){watched.insert(p.clone());}}
 loop{
  if !alive.load(Ordering::Acquire){break}
  while let Ok(p)=reg_rx.try_recv(){if watched.insert(p.clone()){let _=watcher.watch(&p,RecursiveMode::NonRecursive);}}
  match rx.recv_timeout(Duration::from_millis(100)){
   Ok(Ok(event))=>for p in event.paths{if let Some(v)=local_to_virtual(&exports,&p){let _=ctrl.send(CtrlFrame::new(CTRL_CHANGE,session,0,0,0,v.into_bytes()));}},
   Ok(Err(e))=>logln!(logger,"WATCH_ERROR={e}"),Err(mpsc::RecvTimeoutError::Timeout)=>{},Err(_)=>break
  }
 }
})}

// ============================================================
// Local server and Lane0 data workers
// ============================================================

fn mutate_reply(tx:&mpsc::Sender<CtrlFrame>,session:u64,req:u64,m:&RemoteMeta){let mut p=vec![];encode_meta(&mut p,m);let _=tx.send(CtrlFrame::new(CTRL_MUTATE_RESP,session,req,0,0,p));}
fn create_reply(tx:&mpsc::Sender<CtrlFrame>,session:u64,req:u64,m:&RemoteMeta){let mut p=vec![];encode_meta(&mut p,m);let _=tx.send(CtrlFrame::new(CTRL_CREATE_RESP,session,req,0,0,p));}

fn spawn_server(exports:LocalExports,config:AppConfig,session:u64,rx:mpsc::Receiver<ServerRequest>,ctrl:mpsc::Sender<CtrlFrame>,data_tx:mpsc::Sender<DataTxJob>,writes:ActiveWrites,watch_reg:mpsc::Sender<PathBuf>,logger:Logger)->thread::JoinHandle<()>{thread::spawn(move||while let Ok(req)=rx.recv(){match req{
 ServerRequest::Stat(f)=>{let p=String::from_utf8_lossy(&f.payload).to_string();match local_stat(&exports,&p){Ok(m)=>{let mut q=vec![];encode_meta(&mut q,&m);let _=ctrl.send(CtrlFrame::new(CTRL_STAT_RESP,session,f.request,0,0,q));},Err(e)=>{let st=io_status(&e);if config.writable{logln!(logger,"V9_STAT_ERR req={:016X} path='{}' kind={:?} status=0x{:08X} msg={}",f.request,p,e.kind(),st as u32,e);}send_fs_error(&ctrl,session,f.request,st,e.to_string())}}}
 ServerRequest::List(f)=>{let p=String::from_utf8_lossy(&f.payload).to_string();match safe_existing_io(&exports,&p){Ok(lp)=>{let _=watch_reg.send(lp);match local_list(&exports,&p).and_then(|x|encode_entries(&x).map_err(|e|io::Error::new(io::ErrorKind::Other,e.to_string()))){Ok(q)=>{let _=ctrl.send(CtrlFrame::new(CTRL_LIST_RESP,session,f.request,0,0,q));},Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}},Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}}
 ServerRequest::Read(f)=>{let p=String::from_utf8_lossy(&f.payload).to_string();match safe_existing(&exports,&p){Ok(lp)=>{if data_tx.send(DataTxJob::ReadResponse{request:f.request,path:lp,offset:f.arg0,len:f.arg1}).is_err(){send_fs_error(&ctrl,session,f.request,STATUS_DEVICE_NOT_CONNECTED,"data worker stopped")}},Err(e)=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())}}
 ServerRequest::Create(f)=>{
  if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only version");continue}
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  let allocation=if q.len()>=8{dec_u64(&mut q).unwrap_or(0)}else{0};
  let is_dir=f.arg0!=0;
  logln!(logger,"V9_CREATE req={:016X} path='{}' dir={} attrs=0x{:08X} allocation={}",f.request,p,is_dir,f.arg1 as u32,allocation);
  match safe_create_target(&exports,&p){
   Ok(lp)=>{
    let result=(||->io::Result<RemoteMeta>{
     if is_dir{
      fs::create_dir(&lp)?;
      let _=apply_attributes(&lp,f.arg1 as u32);
      return Ok(meta_from_fs(&fs::metadata(&lp)?))
     }
     let file=OpenOptions::new().read(true).write(true).create_new(true).open(&lp)?;
     if allocation>0{set_file_allocation(&file,allocation)?;}
     let _=apply_attributes(&lp,f.arg1 as u32);
     let mut meta=meta_from_fs(&file.metadata()?);
     if allocation>meta.allocation_size{meta.allocation_size=allocation;}
     Ok(meta)
    })();
    match result{
     Ok(m)=>{logln!(logger,"V9_CREATE_OK req={:016X} size={} allocation={}",f.request,m.size,m.allocation_size);create_reply(&ctrl,session,f.request,&m)}
     Err(e)=>{logln!(logger,"V9_CREATE_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}
    }
   }
   Err(e)=>{logln!(logger,"V9_CREATE_PATH_ERR req={:016X} msg={}",f.request,e);send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())}
  }
 }
 ServerRequest::Overwrite(f)=>{
  if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  let allocation=if q.len()>=8{dec_u64(&mut q).unwrap_or(0)}else{0};
  let replace_attrs=f.arg1!=0;
  logln!(logger,"V9_OVERWRITE req={:016X} path='{}' attrs=0x{:08X} replace_attrs={} allocation={}",f.request,p,f.arg0 as u32,replace_attrs,allocation);
  match safe_existing(&exports,&p){
   Ok(lp)=>{
    let result=(||->io::Result<RemoteMeta>{
     let file=OpenOptions::new().read(true).write(true).truncate(true).open(&lp)?;
     if allocation>0{set_file_allocation(&file,allocation)?;}
     let mut attrs=f.arg0 as u32;
     if !replace_attrs{attrs|=file.metadata()?.file_attributes();}
     let _=apply_attributes(&lp,attrs);
     let mut meta=meta_from_fs(&file.metadata()?);
     if allocation>meta.allocation_size{meta.allocation_size=allocation;}
     Ok(meta)
    })();
    match result{
     Ok(m)=>mutate_reply(&ctrl,session,f.request,&m),
     Err(e)=>{logln!(logger,"V9_OVERWRITE_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}
    }
   }
   Err(e)=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())
  }
 }
 ServerRequest::WriteBegin(f)=>{if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}let mut q=f.payload.as_slice();let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};let to_eof=q.first().copied().unwrap_or(0)!=0;let wt=q.get(1).copied().unwrap_or(0)!=0;match safe_existing(&exports,&p){Ok(lp)=>match OpenOptions::new().read(true).write(true).open(&lp){Ok(file)=>{let actual=if to_eof{file.metadata().map(|m|m.len()).unwrap_or(f.arg0)}else{f.arg0};writes.lock().unwrap().insert(f.request,ActiveWrite{file,path:lp,start:actual,expected:f.arg1,received:0,write_through:wt});let _=ctrl.send(CtrlFrame::new(CTRL_WRITE_READY,session,f.request,actual,0,vec![]));},Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())},Err(e)=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())}}
 ServerRequest::Flush(f)=>{let mut q=f.payload.as_slice();let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};match safe_existing(&exports,&p){Ok(lp)=>match OpenOptions::new().read(true).write(config.writable).open(&lp).and_then(|x|{x.sync_all()?;x.metadata()}){Ok(m)=>mutate_reply(&ctrl,session,f.request,&meta_from_fs(&m)),Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())},Err(e)=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())}}
 ServerRequest::Rename(f)=>{if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}let mut q=f.payload.as_slice();let old=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};let new=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};match (safe_existing(&exports,&old),safe_create_target(&exports,&new)){(Ok(a),Ok(b))=>{if b.exists()&&f.arg0!=0{let _=if b.is_dir(){fs::remove_dir_all(&b)}else{fs::remove_file(&b)};}match fs::rename(&a,&b).and_then(|_|fs::metadata(&b)){Ok(m)=>mutate_reply(&ctrl,session,f.request,&meta_from_fs(&m)),Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}},_=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"unsafe rename")}}
 ServerRequest::DeleteCheck(f)=>{
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  logln!(logger,"V9_DELETE_CHECK req={:016X} path='{}'",f.request,p);
  match safe_existing_io(&exports,&p){
   Ok(lp)=>{
    let m=match fs::metadata(&lp){Ok(m)=>m,Err(e)=>{send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string());continue}};
    if m.is_dir(){
     match fs::read_dir(&lp){
      Ok(mut it)=>{
       if it.next().is_some(){
        logln!(logger,"V9_DELETE_CHECK_NOT_EMPTY req={:016X}",f.request);
        send_fs_error(&ctrl,session,f.request,STATUS_DIRECTORY_NOT_EMPTY,"directory not empty")
       }else{
        logln!(logger,"V9_DELETE_CHECK_OK req={:016X} dir=true",f.request);
        mutate_reply(&ctrl,session,f.request,&meta_from_fs(&m))
       }
      },
      Err(e)=>send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())
     }
    }else{
     logln!(logger,"V9_DELETE_CHECK_OK req={:016X} dir=false",f.request);
     mutate_reply(&ctrl,session,f.request,&meta_from_fs(&m))
    }
   }
   Err(e)=>{
    logln!(logger,"V9_DELETE_CHECK_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);
    send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())
   }
  }
 }
 ServerRequest::DeleteCommit(f)=>{
  if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  logln!(logger,"V9_DELETE_COMMIT req={:016X} path='{}'",f.request,p);
  match safe_existing_io(&exports,&p){
   Ok(lp)=>{
    let r=if lp.is_dir(){fs::remove_dir(&lp)}else{fs::remove_file(&lp)};
    match r{
     Ok(_)=>{
      logln!(logger,"V9_DELETE_COMMIT_OK req={:016X}",f.request);
      let _=ctrl.send(CtrlFrame::new(CTRL_MUTATE_RESP,session,f.request,0,0,vec![]));
     },
     Err(e) if e.kind()==io::ErrorKind::NotFound=>{
      // Cleanup is non-reporting and can be observed more than once through
      // independent file objects. Treat an already-removed namespace entry as
      // successful completion of the requested delete.
      logln!(logger,"V9_DELETE_COMMIT_ALREADY_GONE req={:016X}",f.request);
      let _=ctrl.send(CtrlFrame::new(CTRL_MUTATE_RESP,session,f.request,0,0,vec![]));
     },
     Err(e)=>{
      logln!(logger,"V9_DELETE_COMMIT_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);
      send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())
     }
    }
   }
   Err(e) if e.kind()==io::ErrorKind::NotFound=>{
    logln!(logger,"V9_DELETE_COMMIT_ALREADY_GONE req={:016X}",f.request);
    let _=ctrl.send(CtrlFrame::new(CTRL_MUTATE_RESP,session,f.request,0,0,vec![]));
   }
   Err(e)=>{
    logln!(logger,"V9_DELETE_COMMIT_PATH_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);
    send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())
   }
  }
 }
 ServerRequest::SetSize(f)=>{
  if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  let set_allocation=f.arg1!=0;
  logln!(logger,"V9_SET_SIZE req={:016X} path='{}' size={} allocation_mode={}",f.request,p,f.arg0,set_allocation);
  match safe_existing(&exports,&p){
   Ok(lp)=>{
    let result=(||->io::Result<RemoteMeta>{
     let file=OpenOptions::new().read(true).write(true).open(&lp)?;
     if set_allocation{set_file_allocation(&file,f.arg0)?;}else{set_file_eof(&file,f.arg0)?;}
     let mut meta=meta_from_fs(&file.metadata()?);
     if set_allocation&&f.arg0>meta.allocation_size{meta.allocation_size=f.arg0;}
     Ok(meta)
    })();
    match result{
     Ok(m)=>mutate_reply(&ctrl,session,f.request,&m),
     Err(e)=>{logln!(logger,"V9_SET_SIZE_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}
    }
   }
   Err(e)=>send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,e.to_string())
  }
 }
 ServerRequest::SetBasic(f)=>{
  if !config.writable{send_fs_error(&ctrl,session,f.request,STATUS_ACCESS_DENIED,"read-only");continue}
  let mut q=f.payload.as_slice();
  let p=match dec_string(&mut q){Ok(x)=>x,Err(e)=>{send_fs_error(&ctrl,session,f.request,STATUS_INVALID_PARAMETER,e.to_string());continue}};
  let creation=dec_u64(&mut q).unwrap_or(0);
  let at=dec_u64(&mut q).unwrap_or(0);
  let mt=dec_u64(&mut q).unwrap_or(0);
  let change=dec_u64(&mut q).unwrap_or(0);
  logln!(logger,"V9_SET_BASIC req={:016X} path='{}' attrs=0x{:08X} ct={} at={} wt={} cht={}",f.request,p,f.arg0 as u32,creation,at,mt,change);
  match safe_existing_io(&exports,&p){
   Ok(lp)=>{
    match apply_basic_info(&lp,f.arg0 as u32,creation,at,mt,change).and_then(|_|fs::metadata(&lp).map(|m|meta_from_fs(&m))){
     Ok(m)=>{logln!(logger,"V9_SET_BASIC_OK req={:016X}",f.request);mutate_reply(&ctrl,session,f.request,&m)}
     Err(e)=>{logln!(logger,"V9_SET_BASIC_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}
    }
   }
   Err(e)=>{logln!(logger,"V9_SET_BASIC_PATH_ERR req={:016X} kind={:?} os={:?} msg={}",f.request,e.kind(),e.raw_os_error(),e);send_fs_error(&ctrl,session,f.request,io_status(&e),e.to_string())}
  }
 }
}})}

fn spawn_data_writer(mut tx:EndpointWrite<Bulk>,rx:mpsc::Receiver<DataTxJob>,ctrl:mpsc::Sender<CtrlFrame>,session:u64,logger:Logger,alive:Arc<AtomicBool>)->thread::JoinHandle<()>{thread::spawn(move||loop{
 if !alive.load(Ordering::Acquire){break}
 let job=match rx.recv_timeout(Duration::from_millis(100)){
  Ok(x)=>x,
  Err(mpsc::RecvTimeoutError::Timeout)=>continue,
  Err(mpsc::RecvTimeoutError::Disconnected)=>break,
 };
 match job{
 DataTxJob::ReadResponse{request,path,offset,len}=>{let st=Instant::now();let r=(||->AppResult<u64>{let f=File::open(&path)?;let size=f.metadata()?.len();let total=size.saturating_sub(offset).min(len);let mut sent=0;let mut seq=0;let mut b=vec![0u8;DATA_BLOCK];while sent<total{let want=(total-sent).min(DATA_BLOCK as u64) as usize;let n=read_at(&f,offset+sent,&mut b[..want])?;if n==0{break}let h=DataHeader{kind:DATA_READ_CHUNK,request,offset:offset+sent,seq,len:n as u32,crc:crc32(&b[..n])};tx.write_all(&h.bytes())?;tx.write_all(&b[..n])?;sent+=n as u64;seq+=1;}tx.write_all(&DataHeader{kind:DATA_READ_END,request,offset:offset+sent,seq,len:0,crc:0}.bytes())?;tx.flush()?;Ok(sent)})();match r{Ok(n)=>{let(a,b)=rate(n,st.elapsed());logln!(logger,"READ_TX req={request:016X} bytes={n} {a:.1} MiB/s {b:.3} Gbit/s")},Err(e)=>send_fs_error(&ctrl,session,request,STATUS_IO_DEVICE_ERROR,e.to_string())}}
 DataTxJob::Upload{request,offset,data}=>{let r=(||->AppResult<()>{let mut pos=0usize;let mut seq=0u64;while pos<data.len(){let n=(data.len()-pos).min(DATA_BLOCK);let sl=&data[pos..pos+n];tx.write_all(&DataHeader{kind:DATA_WRITE_CHUNK,request,offset:offset+pos as u64,seq,len:n as u32,crc:crc32(sl)}.bytes())?;tx.write_all(sl)?;pos+=n;seq+=1;}tx.write_all(&DataHeader{kind:DATA_WRITE_END,request,offset:offset+data.len() as u64,seq,len:0,crc:0}.bytes())?;tx.flush()?;Ok(())})();if let Err(e)=r{send_fs_error(&ctrl,session,request,STATUS_IO_DEVICE_ERROR,e.to_string())}}
}})}

fn spawn_data_reader(mut rx:EndpointRead<Bulk>,pending:DataPending,writes:ActiveWrites,ctrl:mpsc::Sender<CtrlFrame>,session:u64,logger:Logger,alive:Arc<AtomicBool>)->thread::JoinHandle<()>{thread::spawn(move||loop{if !alive.load(Ordering::Acquire){break}let(h,skipped)=match read_data_header(&mut rx){Ok(x)=>x,Err(e)=>{let text=e.to_string().to_lowercase();if text.contains("timed out")||text.contains("timeout"){continue}if alive.load(Ordering::Acquire){logln!(logger,"DATA_RX_FATAL={e}");}break}};if skipped>0{logln!(logger,"DATA_RESYNC skipped_bytes={skipped}");}match h.kind{
 DATA_READ_CHUNK=>{if h.len as usize>DATA_BLOCK{break}let mut b=vec![0u8;h.len as usize];if rx.read_exact(&mut b).is_err(){break}if let Some(tx)=pending.lock().ok().and_then(|m|m.get(&h.request).cloned()){if crc32(&b)==h.crc{let _=tx.send(DataPacket::Data{offset:h.offset,bytes:b});}else{let _=tx.send(DataPacket::Error("CRC mismatch".into()));}}}
 DATA_READ_END=>{if let Some(tx)=pending.lock().ok().and_then(|mut m|m.remove(&h.request)){let _=tx.send(DataPacket::End{offset:h.offset});}}
 DATA_WRITE_CHUNK=>{if h.len as usize>DATA_BLOCK{break}let mut b=vec![0u8;h.len as usize];if rx.read_exact(&mut b).is_err(){break}if crc32(&b)!=h.crc{send_fs_error(&ctrl,session,h.request,STATUS_IO_DEVICE_ERROR,"write CRC mismatch");continue}let mut map=writes.lock().unwrap();if let Some(w)=map.get_mut(&h.request){if h.offset!=w.start+w.received{send_fs_error(&ctrl,session,h.request,STATUS_IO_DEVICE_ERROR,"write offset mismatch");continue}match write_at(&w.file,h.offset,&b){Ok(n) if n==b.len()=>w.received+=n as u64,Ok(_)=>send_fs_error(&ctrl,session,h.request,STATUS_IO_DEVICE_ERROR,"short write"),Err(e)=>send_fs_error(&ctrl,session,h.request,io_status(&e),e.to_string())}}}
 DATA_WRITE_END=>{let mut map=writes.lock().unwrap();if let Some(w)=map.remove(&h.request){let result=(||->io::Result<RemoteMeta>{if w.received!=w.expected{return Err(io::Error::new(io::ErrorKind::WriteZero,"write length mismatch"))}if w.write_through{w.file.sync_data()?;}Ok(meta_from_fs(&w.file.metadata()?))})();match result{Ok(m)=>{let mut p=vec![];encode_meta(&mut p,&m);let _=ctrl.send(CtrlFrame::new(CTRL_WRITE_RESP,session,h.request,w.received,0,p));},Err(e)=>send_fs_error(&ctrl,session,h.request,io_status(&e),e.to_string())}}}
 _=>break
}})}

// ============================================================
// Remote cache and WinFsp context
// ============================================================

struct CacheState{generation:AtomicU64,meta:Mutex<HashMap<String,(Instant,RemoteMeta)>>,dirs:Mutex<HashMap<String,(Instant,Vec<RemoteEntry>)>>}
impl CacheState{
 fn new()->Self{Self{generation:AtomicU64::new(1),meta:Mutex::new(HashMap::new()),dirs:Mutex::new(HashMap::new())}}
 fn invalidate(&self,path:&str){
  self.generation.fetch_add(1,Ordering::AcqRel);
  let p=normalize_virtual_path(path);
  let parent=p.rsplit_once('\\').map(|(a,_)|if a.is_empty(){"\\".to_string()}else{a.to_string()});
  if let Ok(mut m)=self.meta.lock(){
   m.retain(|k,_|{
    let hit_path=eq_ci(k,&p)||k.starts_with(&(p.clone()+"\\"));
    let hit_parent=parent.as_ref().map(|q|eq_ci(k,q)).unwrap_or(false);
    !hit_path&&!hit_parent
   });
  }
  if let Ok(mut d)=self.dirs.lock(){d.clear();}
 }
}
struct ReadCache{generation:u64,start:u64,data:Vec<u8>}
// Serialize one page. The zero-sized record is an end-of-directory marker,
// not an end-of-buffer marker; callers may need to resume after the last name.
fn write_directory_page(entries:&[RemoteEntry],marker:Option<&str>,buffer:&mut[u8])->winfsp::Result<u32>{
 let start=marker.and_then(|m|entries.iter().position(|e|eq_ci(&e.name,m)).map(|x|x+1)).unwrap_or(0);
 let mut cursor=0u32;
 for e in entries.iter().skip(start){
  let mut d=DirInfo::<255>::new();
  *d.file_info_mut()=e.meta.to_file_info();
  // WinFsp directory names are counted UTF-16 strings, without a trailing NUL.
  // winfsp 0.13.1 set_name() includes that NUL in Size, breaking exact matches.
  let name:Vec<u16>=OsStr::new(&e.name).encode_wide().collect();
  d.set_name_raw(name.as_slice()).map_err(|_|nt_error(STATUS_INVALID_PARAMETER))?;
  if !d.append_to_buffer(buffer,&mut cursor){return Ok(cursor)}
 }
 let _=DirInfo::<255>::finalize_buffer(buffer,&mut cursor);
 Ok(cursor)
}

#[cfg(test)]
#[path = "directory_tests.rs"]
mod directory_tests;
#[cfg(test)]
#[path = "clipboard_file_tests.rs"]
mod clipboard_file_tests;

struct RemoteFileContext{path:Mutex<String>,meta:Mutex<RemoteMeta>,delete_requested:AtomicBool,deleted:AtomicBool,dir_cache:Mutex<Option<(u64,Vec<RemoteEntry>)>>,read_cache:Mutex<Option<ReadCache>>}
impl RemoteFileContext{
 fn new(p:String,m:RemoteMeta)->Self{Self{path:Mutex::new(p),meta:Mutex::new(m),delete_requested:AtomicBool::new(false),deleted:AtomicBool::new(false),dir_cache:Mutex::new(None),read_cache:Mutex::new(None)}}
 fn path(&self)->winfsp::Result<String>{self.path.lock().map(|x|x.clone()).map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))}
 fn update_meta(&self,m:RemoteMeta){if let Ok(mut x)=self.meta.lock(){*x=m;}}
 fn set_meta(&self,m:RemoteMeta){self.update_meta(m);if let Ok(mut c)=self.read_cache.lock(){*c=None;}}
}
#[derive(Clone)] struct RemoteFs{config:AppConfig,peer:Manifest,rpc:RpcClient,label:String,cache:Arc<CacheState>}
impl RemoteFs{
 fn mutable_path(&self,p:&str)->bool{let ps=components(p).unwrap_or_default();if ps.len()>=2&&(eq_ci(&ps[0],"Desktop")||eq_ci(&ps[0],"Downloads")||eq_ci(&ps[0],"Documents")){return true}if ps.len()>=3&&eq_ci(&ps[0],"Drives"){if let Some(l)=ps[1].chars().next(){return self.peer.drives.iter().any(|d|d.letter==l.to_ascii_uppercase())}}false}
 fn lookup(&self,p:&str)->winfsp::Result<RemoteMeta>{let p=normalize_virtual_path(p);if let Some(m)=synthetic_stat(&self.peer,&p){return Ok(m)}if self.config.cache{if let Ok(g)=self.cache.meta.lock(){if let Some((t,m))=g.get(&p){if t.elapsed()<CACHE_TTL{return Ok(m.clone())}}}}let m=self.rpc.stat(&p)?;if self.config.cache{if let Ok(mut g)=self.cache.meta.lock(){g.insert(p,(Instant::now(),m.clone()));}}Ok(m)}
 fn list(&self,p:&str)->winfsp::Result<Vec<RemoteEntry>>{let p=normalize_virtual_path(p);if let Some(v)=synthetic_list(&self.peer,&p){return Ok(v)}if self.config.cache{if let Ok(g)=self.cache.dirs.lock(){if let Some((t,v))=g.get(&p){if t.elapsed()<CACHE_TTL{return Ok(v.clone())}}}}let v=self.rpc.list(&p)?;if self.config.cache{if let Ok(mut g)=self.cache.dirs.lock(){g.insert(p,(Instant::now(),v.clone()));}}Ok(v)}
 fn refresh_context(&self,c:&RemoteFileContext)->winfsp::Result<RemoteMeta>{let p=c.path()?;let m=self.lookup(&p)?;c.update_meta(m.clone());Ok(m)}
 fn invalidate(&self,p:&str){self.cache.invalidate(p)}
 fn capacity(&self)->(u64,u64){(self.peer.drives.iter().map(|d|d.total).sum(),self.peer.drives.iter().map(|d|d.free).sum())}
}

impl FileSystemContext for RemoteFs{
 type FileContext=RemoteFileContext;
 fn get_security_by_name(&self,file_name:&U16CStr,_sd:Option<&mut[c_void]>,_resolver:impl FnOnce(&U16CStr)->Option<FileSecurity>)->winfsp::Result<FileSecurity>{self.rpc.check()?;let m=self.lookup(&u16c_to_string(file_name))?;Ok(FileSecurity{reparse:false,sz_security_descriptor:0,attributes:m.attributes})}
 fn open(&self,file_name:&U16CStr,_opts:u32,_access:FILE_ACCESS_RIGHTS,file_info:&mut OpenFileInfo)->winfsp::Result<Self::FileContext>{self.rpc.check()?;let p=normalize_virtual_path(&u16c_to_string(file_name));let m=self.lookup(&p)?;*file_info.as_mut()=m.to_file_info();Ok(RemoteFileContext::new(p,m))}
 fn create(&self,file_name:&U16CStr,create_options:u32,_access:FILE_ACCESS_RIGHTS,file_attributes:FILE_FLAGS_AND_ATTRIBUTES,_sd:Option<&[c_void]>,allocation:u64,_extra:Option<&[u8]>,_is_reparse:bool,file_info:&mut OpenFileInfo)->winfsp::Result<Self::FileContext>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let p=normalize_virtual_path(&u16c_to_string(file_name));if !self.mutable_path(&p){return Err(nt_error(STATUS_ACCESS_DENIED))}let m=self.rpc.create(&p,create_options&FILE_DIRECTORY_FILE!=0,file_attributes,allocation)?;self.invalidate(&p);*file_info.as_mut()=m.to_file_info();Ok(RemoteFileContext::new(p,m))}
 fn close(&self,_context:Self::FileContext){}
 fn cleanup(&self,context:&Self::FileContext,_file_name:Option<&U16CStr>,flags:u32){if self.config.writable&&flags&FSP_CLEANUP_DELETE!=0&&!context.deleted.swap(true,Ordering::AcqRel){if let Ok(p)=context.path(){if self.rpc.delete_commit(&p).is_ok(){self.invalidate(&p);}else{context.deleted.store(false,Ordering::Release);}}}}
 fn get_file_info(&self,context:&Self::FileContext,file_info:&mut FileInfo)->winfsp::Result<()>{self.rpc.check()?;let m=if context.deleted.load(Ordering::Acquire){context.meta.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.clone()}else{self.refresh_context(context)?};*file_info=m.to_file_info();Ok(())}
 fn read_directory(&self,context:&Self::FileContext,_pattern:Option<&U16CStr>,marker:DirMarker<'_>,buffer:&mut[u8])->winfsp::Result<u32>{self.rpc.check()?;let meta=context.meta.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.clone();if !meta.is_dir(){return Err(nt_error(STATUS_NOT_A_DIRECTORY))}let generation=self.cache.generation.load(Ordering::Acquire);let entries={let mut c=context.dir_cache.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?;if c.as_ref().map(|x|x.0!=generation).unwrap_or(true){*c=Some((generation,self.list(&context.path()?)?));}c.as_ref().unwrap().1.clone()};let marker_name=if marker.is_none(){None}else if marker.is_current(){Some(".".to_string())}else if marker.is_parent(){Some("..".to_string())}else{marker.inner().map(|w|{let v=w.iter().copied().take_while(|&x|x!=0).collect::<Vec<_>>();OsString::from_wide(&v).to_string_lossy().to_string()})};let mut all=vec![RemoteEntry{name:".".into(),meta:meta.clone()}];if context.path()? != "\\"{all.push(RemoteEntry{name:"..".into(),meta:synthetic_meta()});}all.extend(entries);write_directory_page(&all,marker_name.as_deref(),buffer)}
 fn read(&self,context:&Self::FileContext,buffer:&mut[u8],offset:u64)->winfsp::Result<u32>{
  self.rpc.check()?;
  let generation=self.cache.generation.load(Ordering::Acquire);
  let m=if self.config.cache{self.refresh_context(context)?}else{context.meta.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.clone()};
  if m.is_dir(){return Err(nt_error(STATUS_FILE_IS_A_DIRECTORY))}
  if offset>=m.size||buffer.is_empty(){return Ok(0)}
  let want=buffer.len().min((m.size-offset) as usize);
  let mut c=context.read_cache.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?;
  let hit=c.as_ref().map(|x|x.generation==generation&&offset>=x.start&&offset+want as u64<=x.start+x.data.len() as u64).unwrap_or(false);
  if !hit{
   let n=self.config.read_ahead.max(want).min((m.size-offset) as usize);
   *c=Some(ReadCache{generation,start:offset,data:self.rpc.read_range(&context.path()?,offset,n)?});
  }
  let x=c.as_ref().unwrap();
  let rel=(offset-x.start) as usize;
  if rel>=x.data.len(){return Ok(0)}
  let n=want.min(x.data.len()-rel);
  buffer[..n].copy_from_slice(&x.data[rel..rel+n]);
  Ok(n as u32)
 }
 fn write(&self,context:&Self::FileContext,buffer:&[u8],offset:u64,write_to_eof:bool,constrained_io:bool,file_info:&mut FileInfo)->winfsp::Result<u32>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let p=context.path()?;if !self.mutable_path(&p){return Err(nt_error(STATUS_ACCESS_DENIED))}let old=context.meta.lock().map_err(|_|nt_error(STATUS_IO_DEVICE_ERROR))?.clone();let mut data=buffer;if constrained_io&&!write_to_eof{if offset>=old.size{return Ok(0)}data=&data[..data.len().min((old.size-offset) as usize)];}let(n,m)=self.rpc.write_range(&p,offset,data,write_to_eof,self.config.write_through)?;context.set_meta(m.clone());self.invalidate(&p);*file_info=m.to_file_info();Ok(n)}
 fn overwrite(&self,context:&Self::FileContext,file_attributes:FILE_FLAGS_AND_ATTRIBUTES,replace:bool,allocation:u64,_extra:Option<&[u8]>,file_info:&mut FileInfo)->winfsp::Result<()>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let p=context.path()?;let m=self.rpc.overwrite(&p,file_attributes,replace,allocation)?;context.set_meta(m.clone());self.invalidate(&p);*file_info=m.to_file_info();Ok(())}
 fn flush(&self,context:Option<&Self::FileContext>,file_info:&mut FileInfo)->winfsp::Result<()>{if let Some(c)=context{let p=c.path()?;let m=self.rpc.flush(&p)?;c.set_meta(m.clone());*file_info=m.to_file_info();}Ok(())}
 fn rename(&self,context:&Self::FileContext,file_name:&U16CStr,new_file_name:&U16CStr,replace_if_exists:bool)->winfsp::Result<()>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let old=normalize_virtual_path(&u16c_to_string(file_name));let new=normalize_virtual_path(&u16c_to_string(new_file_name));if !self.mutable_path(&old)||!self.mutable_path(&new){return Err(nt_error(STATUS_ACCESS_DENIED))}let m=self.rpc.rename(&old,&new,replace_if_exists)?;if let Ok(mut p)=context.path.lock(){*p=new.clone();}context.set_meta(m);self.invalidate(&old);self.invalidate(&new);Ok(())}
 fn set_delete(&self,context:&Self::FileContext,file_name:&U16CStr,delete_file:bool)->winfsp::Result<()>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}if delete_file{let p=normalize_virtual_path(&u16c_to_string(file_name));if !self.mutable_path(&p){return Err(nt_error(STATUS_ACCESS_DENIED))}self.rpc.delete_check(&p)?;}else{context.deleted.store(false,Ordering::Release);}context.delete_requested.store(delete_file,Ordering::Release);Ok(())}
 fn set_file_size(&self,context:&Self::FileContext,new_size:u64,set_allocation:bool,file_info:&mut FileInfo)->winfsp::Result<()>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let p=context.path()?;let m=self.rpc.set_size(&p,new_size,set_allocation)?;context.set_meta(m.clone());self.invalidate(&p);*file_info=m.to_file_info();Ok(())}
 fn set_basic_info(&self,context:&Self::FileContext,file_attributes:u32,creation:u64,last_access:u64,last_write:u64,change:u64,file_info:&mut FileInfo)->winfsp::Result<()>{if !self.config.writable{return Err(nt_error(STATUS_ACCESS_DENIED))}let p=context.path()?;let m=self.rpc.set_basic(&p,file_attributes,creation,last_access,last_write,change)?;context.set_meta(m.clone());self.invalidate(&p);*file_info=m.to_file_info();Ok(())}
 fn get_volume_info(&self,out:&mut VolumeInfo)->winfsp::Result<()>{self.rpc.check()?;let(total,free)=self.capacity();out.total_size=total;out.free_size=free;out.set_volume_label(&self.label);Ok(())}
}


// ============================================================
// V10 Explorer file clipboard
// ============================================================

fn encode_clip_files(paths:&[String])->AppResult<Vec<u8>>{
 if paths.is_empty(){return Err("empty clipboard file list".into())}
 if paths.len()>CLIP_FILES_MAX_ITEMS{return Err("too many clipboard files".into())}
 let mut out=Vec::new();
 out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
 for p in paths{enc_string(&mut out,p)?;}
 if out.len()>MAX_CTRL_PAYLOAD{return Err("clipboard file list too large".into())}
 Ok(out)
}

fn decode_clip_files(mut payload:&[u8])->AppResult<Vec<String>>{
 let n=dec_u32(&mut payload)? as usize;
 if n==0||n>CLIP_FILES_MAX_ITEMS{return Err("invalid clipboard file count".into())}
 let mut out=Vec::with_capacity(n);
 for _ in 0..n{
  let p=normalize_virtual_path(&dec_string(&mut payload)?);
  let _=components(&p)?;
  out.push(p);
 }
 if !payload.is_empty(){return Err("trailing clipboard file payload".into())}
 Ok(out)
}

fn virtual_to_mount_path(drive:&str,virtual_path:&str)->AppResult<PathBuf>{
 let drive=drive.trim_end_matches('\\');
 if drive.len()!=2||!drive.ends_with(':'){return Err("invalid peer mount drive".into())}
 let p=normalize_virtual_path(virtual_path);
 let _=components(&p)?;
 Ok(PathBuf::from(format!("{drive}{p}")))
}

fn path_on_mount(path:&Path,drive:&str)->bool{
 let s=path.to_string_lossy();
 let d=drive.trim_end_matches('\\');
 if d.len()!=2{return false}
 let sb=s.as_bytes();let db=d.as_bytes();
 sb.len()>=2&&sb[0].eq_ignore_ascii_case(&db[0])&&sb[1]==b':'&&(sb.len()==2||sb.get(2)==Some(&b'\\')||sb.get(2)==Some(&b'/'))
}

fn open_clipboard_retry(owner:HWND)->io::Result<()>{
 for _ in 0..12{
  if unsafe{OpenClipboard(owner)}!=0{return Ok(())}
  thread::sleep(Duration::from_millis(10));
 }
 Err(io::Error::last_os_error())
}

fn preferred_drop_effect_format()->u32{
 let w=wide_null("Preferred DropEffect");
 unsafe{RegisterClipboardFormatW(w.as_ptr())}
}

fn read_clipboard_u32(format:u32)->Option<u32>{
 if format==0{return None}
 let h=unsafe{GetClipboardData(format)};
 if h.is_null(){return None}
 let p=unsafe{GlobalLock(h)} as *const u32;
 if p.is_null(){return None}
 let v=unsafe{*p};
 unsafe{let _=GlobalUnlock(h);}
 Some(v)
}

fn read_clipboard_files(owner:HWND)->io::Result<Option<(Vec<PathBuf>,u32)>>{
 if unsafe{IsClipboardFormatAvailable(CF_HDROP)}==0{return Ok(None)}
 open_clipboard_retry(owner)?;
 let result=(||->io::Result<Option<(Vec<PathBuf>,u32)>>{
  let drop=unsafe{GetClipboardData(CF_HDROP)};
  if drop.is_null(){return Ok(None)}
  let count=unsafe{DragQueryFileW(drop,u32::MAX,null_mut(),0)};
  if count==0{return Ok(None)}
  if count as usize>CLIP_FILES_MAX_ITEMS{return Err(io::Error::new(io::ErrorKind::InvalidData,"too many clipboard files"))}
  let mut paths=Vec::with_capacity(count as usize);
  for i in 0..count{
   let n=unsafe{DragQueryFileW(drop,i,null_mut(),0)};
   if n==0{continue}
   let mut buf=vec![0u16;n as usize+1];
   let got=unsafe{DragQueryFileW(drop,i,buf.as_mut_ptr(),buf.len() as u32)};
   if got>0{paths.push(PathBuf::from(OsString::from_wide(&buf[..got as usize])));}
  }
  let effect=read_clipboard_u32(preferred_drop_effect_format()).unwrap_or(DROPEFFECT_COPY);
  Ok(if paths.is_empty(){None}else{Some((paths,effect))})
 })();
 unsafe{let _=CloseClipboard();}
 result
}

fn alloc_global_bytes(bytes:&[u8])->io::Result<HANDLE>{
 let h=unsafe{GlobalAlloc(GMEM_MOVEABLE|GMEM_ZEROINIT,bytes.len())};
 if h.is_null(){return Err(io::Error::last_os_error())}
 let p=unsafe{GlobalLock(h)} as *mut u8;
 if p.is_null(){unsafe{let _=GlobalFree(h);}return Err(io::Error::last_os_error())}
 unsafe{std::ptr::copy_nonoverlapping(bytes.as_ptr(),p,bytes.len());let _=GlobalUnlock(h);}
 Ok(h)
}

fn alloc_global_u32(v:u32)->io::Result<HANDLE>{alloc_global_bytes(&v.to_le_bytes())}

fn set_clipboard_files(owner:HWND,paths:&[PathBuf])->io::Result<()>{
 if paths.is_empty(){return Err(io::Error::new(io::ErrorKind::InvalidInput,"empty clipboard file list"))}
 if paths.len()>CLIP_FILES_MAX_ITEMS{return Err(io::Error::new(io::ErrorKind::InvalidInput,"too many clipboard files"))}

 let header=std::mem::size_of::<DROPFILES>();
 let mut wide_paths:Vec<Vec<u16>>=Vec::with_capacity(paths.len());
 let mut bytes=header;
 for p in paths{
  let w:Vec<u16>=p.as_os_str().encode_wide().collect();
  if w.contains(&0){return Err(io::Error::new(io::ErrorKind::InvalidInput,"clipboard path contains NUL"))}
  bytes=bytes.checked_add((w.len()+1)*2).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"clipboard allocation overflow"))?;
  wide_paths.push(w);
 }
 bytes=bytes.checked_add(2).ok_or_else(||io::Error::new(io::ErrorKind::InvalidInput,"clipboard allocation overflow"))?;

 let h=unsafe{GlobalAlloc(GMEM_MOVEABLE|GMEM_ZEROINIT,bytes)};
 if h.is_null(){return Err(io::Error::last_os_error())}
 let p=unsafe{GlobalLock(h)} as *mut u8;
 if p.is_null(){unsafe{let _=GlobalFree(h);}return Err(io::Error::last_os_error())}

 unsafe{
  let drop=p as *mut DROPFILES;
  std::ptr::write(drop,DROPFILES{p_files:header as u32,pt:POINT::default(),f_nc:0,f_wide:1});
  let mut q=p.add(header) as *mut u16;
  for w in &wide_paths{
   std::ptr::copy_nonoverlapping(w.as_ptr(),q,w.len());
   q=q.add(w.len());
   *q=0;
   q=q.add(1);
  }
  *q=0;
  let _=GlobalUnlock(h);
 }

 if owner.is_null(){unsafe{let _=GlobalFree(h);}return Err(io::Error::new(io::ErrorKind::Other,"clipboard owner window unavailable"))}
 if let Err(e)=open_clipboard_retry(owner){unsafe{let _=GlobalFree(h);}return Err(e)}
 let result=(||->io::Result<()>{
  if unsafe{EmptyClipboard()}==0{unsafe{let _=GlobalFree(h);}return Err(io::Error::last_os_error())}
  if unsafe{SetClipboardData(CF_HDROP,h)}.is_null(){unsafe{let _=GlobalFree(h);}return Err(io::Error::last_os_error())}
  // Ownership of h transfers to the clipboard after successful SetClipboardData.
  let fmt=preferred_drop_effect_format();
  if fmt!=0{
   if let Ok(effect)=alloc_global_u32(DROPEFFECT_COPY){
    if unsafe{SetClipboardData(fmt,effect)}.is_null(){unsafe{let _=GlobalFree(effect);}}
   }
  }
  Ok(())
 })();
 unsafe{let _=CloseClipboard();}
 result
}

// ============================================================
// V10 clipboard
// ============================================================

enum ClipboardCmd{Text(String),Image{w:usize,h:usize,rgba:Vec<u8>},Files(Vec<String>)}

fn spawn_clipboard(
 session:u64,
 ctrl:mpsc::Sender<CtrlFrame>,
 cmd_rx:mpsc::Receiver<ClipboardCmd>,
 alive:Arc<AtomicBool>,
 connected:Arc<AtomicBool>,
 exports:LocalExports,
 mount_drive:Arc<Mutex<Option<String>>>,
 clipboard_owner_raw:usize,
 logger:Logger
)->thread::JoinHandle<()>{
 thread::spawn(move||{
  let clipboard_owner=clipboard_owner_raw as HWND;
  let mut cb=match Clipboard::new(){Ok(x)=>x,Err(e)=>{logln!(logger,"CLIPBOARD_INIT={e}");return}};
  logger.line("CLIPBOARD_READY text+image+files");
  let mut last_text:u32=0;
  let mut last_image:u32=0;
  let mut last_sequence=unsafe{GetClipboardSequenceNumber()};
  let mut pending_files:Option<Vec<String>>=None;

  while alive.load(Ordering::Acquire){
   while let Ok(cmd)=cmd_rx.try_recv(){
    match cmd{
     ClipboardCmd::Text(t)=>{
      last_text=crc32(t.as_bytes());
      if let Err(e)=cb.set_text(t){logln!(logger,"CLIP_TEXT_SET_FAILED={e}")}
      last_sequence=unsafe{GetClipboardSequenceNumber()};
     }
     ClipboardCmd::Image{w,h,rgba}=>{
      last_image=crc32(&rgba);
      if let Err(e)=cb.set_image(ImageData{width:w,height:h,bytes:Cow::Owned(rgba)}){logln!(logger,"CLIP_IMAGE_SET_FAILED={e}")}
      last_sequence=unsafe{GetClipboardSequenceNumber()};
     }
     ClipboardCmd::Files(paths)=>pending_files=Some(paths),
    }
   }

   if let Some(paths)=pending_files.take(){
    let drive=mount_drive.lock().ok().and_then(|x|x.clone());
    if let Some(drive)=drive{
     let mut local=Vec::with_capacity(paths.len());
     let mut bad=None;
     for p in &paths{
      match virtual_to_mount_path(&drive,p){Ok(x)=>local.push(x),Err(e)=>{bad=Some(e.to_string());break}}
     }
     if let Some(e)=bad{
      logln!(logger,"CLIP_FILES_RX_REJECTED={e}");
     }else{
      match set_clipboard_files(clipboard_owner,&local){
       Ok(())=>{
        last_sequence=unsafe{GetClipboardSequenceNumber()};
        logln!(logger,"CLIP_FILES_RX_READY count={} mount={}",local.len(),drive);
       }
       Err(e)=>{
        logln!(logger,"CLIP_FILES_SET_FAILED={e}");
        pending_files=Some(paths);
       }
      }
     }
    }else{
     pending_files=Some(paths);
    }
   }

   if !connected.load(Ordering::Acquire){thread::sleep(CLIPBOARD_POLL);continue}

   let sequence=unsafe{GetClipboardSequenceNumber()};
   if sequence!=0&&sequence!=last_sequence{
    last_sequence=sequence;
    match read_clipboard_files(clipboard_owner){
     Ok(Some((paths,effect)))=>{
      // Right-click Cut is deliberately NOT mirrored as Move. Cross-PC file
      // clipboard is copy-only in this version.
      if effect&DROPEFFECT_MOVE!=0&&effect&DROPEFFECT_COPY==0{
       logln!(logger,"CLIP_FILES_SKIP_MOVE count={}",paths.len());
       thread::sleep(CLIPBOARD_POLL);
       continue
      }

      let peer_drive=mount_drive.lock().ok().and_then(|x|x.clone());
      if peer_drive.as_ref().map(|d|paths.iter().any(|p|path_on_mount(p,d))).unwrap_or(false){
       // This clipboard was installed by the peer and points into our OTI
       // mount. Never reflect it back to the peer.
       thread::sleep(CLIPBOARD_POLL);
       continue
      }

      let mut virtuals=Vec::with_capacity(paths.len());
      let mut unsupported=None;
      for p in &paths{
       match local_to_virtual(&exports,p){
        Some(v)=>virtuals.push(v),
        None=>{unsupported=Some(p.clone());break}
       }
      }
      if let Some(p)=unsupported{
       logln!(logger,"CLIP_FILES_SKIP_UNEXPORTED path='{}'",p.display());
      }else{
       match encode_clip_files(&virtuals){
        Ok(payload)=>{
         if ctrl.send(CtrlFrame::new(CTRL_CLIP_FILES,session,0,0,0,payload)).is_ok(){
          logln!(logger,"CLIP_FILES_TX count={}",virtuals.len());
         }
        }
        Err(e)=>logln!(logger,"CLIP_FILES_ENCODE_FAILED={e}"),
       }
      }
      thread::sleep(CLIPBOARD_POLL);
      continue
     }
     Ok(None)=>{}
     Err(e)=>logln!(logger,"CLIP_FILES_READ_FAILED={e}"),
    }
   }

   if let Ok(t)=cb.get_text(){
    let h=crc32(t.as_bytes());
    if h!=last_text{
     last_text=h;
     if t.len()<=MAX_CTRL_PAYLOAD{
      let _=ctrl.send(CtrlFrame::new(CTRL_CLIP_TEXT,session,0,0,0,t.into_bytes()));
     }
    }
   }else if let Ok(im)=cb.get_image(){
    let bytes=im.bytes.into_owned();
    let h=crc32(&bytes);
    if h!=last_image&&bytes.len()+8<=MAX_CTRL_PAYLOAD{
     last_image=h;
     let mut p=Vec::with_capacity(bytes.len()+8);
     p.extend_from_slice(&(im.width as u32).to_le_bytes());
     p.extend_from_slice(&(im.height as u32).to_le_bytes());
     p.extend_from_slice(&bytes);
     let _=ctrl.send(CtrlFrame::new(CTRL_CLIP_IMAGE,session,0,0,0,p));
    }
   }
   thread::sleep(CLIPBOARD_POLL);
  }
 })
}

// ============================================================
// V10 KVM: Ctrl+Alt+F12 toggles local/remote input target.
// ============================================================

#[path = "kvm_mouse.rs"]
mod kvm_mouse;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct MouseSettings{threshold1:i32,threshold2:i32,acceleration:i32,speed:i32}
impl MouseSettings{
 fn encode(self)->Vec<u8>{
  let mut payload=Vec::with_capacity(16);
  for value in [self.threshold1,self.threshold2,self.acceleration,self.speed]{payload.extend_from_slice(&value.to_le_bytes());}
  payload
 }
 fn decode(payload:&[u8])->Result<Self,String>{
  if payload.len()!=16{return Err(format!("invalid mouse settings payload length {}",payload.len()))}
  let value=|offset|i32::from_le_bytes(payload[offset..offset+4].try_into().unwrap());
  let settings=Self{threshold1:value(0),threshold2:value(4),acceleration:value(8),speed:value(12)};
  if settings.threshold1<0||settings.threshold2<0||!(0..=2).contains(&settings.acceleration)||!(1..=20).contains(&settings.speed){
   return Err("mouse settings values out of range".into())
  }
  Ok(settings)
 }
}

fn read_mouse_settings()->Result<MouseSettings,String>{
 let(mut thresholds,mut speed)=([0i32;3],0i32);
 if unsafe{SystemParametersInfoW(SPI_GETMOUSE,0,thresholds.as_mut_ptr().cast(),0)}==0{
  return Err(format!("SPI_GETMOUSE failed win32={}",unsafe{GetLastError()}))
 }
 if unsafe{SystemParametersInfoW(SPI_GETMOUSESPEED,0,(&mut speed as *mut i32).cast(),0)}==0{
  return Err(format!("SPI_GETMOUSESPEED failed win32={}",unsafe{GetLastError()}))
 }
 let settings=MouseSettings{threshold1:thresholds[0],threshold2:thresholds[1],acceleration:thresholds[2],speed};
 MouseSettings::decode(&settings.encode())
}

fn set_mouse_settings(settings:MouseSettings)->Result<(),String>{
 MouseSettings::decode(&settings.encode())?;
 let previous=read_mouse_settings()?;
 let mut thresholds=[settings.threshold1,settings.threshold2,settings.acceleration];
 if unsafe{SystemParametersInfoW(SPI_SETMOUSE,0,thresholds.as_mut_ptr().cast(),SPIF_SENDCHANGE)}==0{
  return Err(format!("SPI_SETMOUSE failed win32={}",unsafe{GetLastError()}))
 }
 let speed=settings.speed as usize as *mut c_void;
 if unsafe{SystemParametersInfoW(SPI_SETMOUSESPEED,0,speed,SPIF_SENDCHANGE)}==0{
  let error=unsafe{GetLastError()};
  let mut old_thresholds=[previous.threshold1,previous.threshold2,previous.acceleration];
  let rollback_mouse=unsafe{SystemParametersInfoW(SPI_SETMOUSE,0,old_thresholds.as_mut_ptr().cast(),SPIF_SENDCHANGE)}!=0;
  let old_speed=previous.speed as usize as *mut c_void;
  let rollback_speed=unsafe{SystemParametersInfoW(SPI_SETMOUSESPEED,0,old_speed,SPIF_SENDCHANGE)}!=0;
  return Err(format!("SPI_SETMOUSESPEED failed win32={error}; rollback mouse={rollback_mouse} speed={rollback_speed}"))
 }
 Ok(())
}

struct MouseSettingsSync{original:Option<MouseSettings>,logger:Logger}
impl MouseSettingsSync{
 fn new(logger:Logger)->Self{Self{original:None,logger}}
 fn apply(&mut self,settings:MouseSettings)->Result<(),String>{
  if self.original.is_none(){self.original=Some(read_mouse_settings()?);}
  set_mouse_settings(settings)
 }
 fn restore(&mut self){
  if let Some(original)=self.original{
   match set_mouse_settings(original){
    Ok(())=>{self.original=None;self.logger.line("KVM_MOUSE_SETTINGS_RESTORED");}
    Err(e)=>logln!(self.logger,"KVM_MOUSE_SETTINGS_RESTORE_FAILED={e}"),
   }
  }
 }
}
impl Drop for MouseSettingsSync{fn drop(&mut self){self.restore();}}

#[derive(Clone)] struct HookState{
 session:u64,
 tx:mpsc::Sender<CtrlFrame>,
 remote:Arc<AtomicBool>,
 connected:Arc<AtomicBool>,
 status_tx:mpsc::Sender<bool>,
 logger:Logger,
 ctrl_physical:Arc<AtomicBool>,
 alt_physical:Arc<AtomicBool>,
 hotkey_latched:Arc<AtomicBool>,
}
static HOOK_STATE:OnceLock<Mutex<Option<HookState>>>=OnceLock::new();
static KVM_MOUSE_TX_COUNT:AtomicU64=AtomicU64::new(0);
static KVM_MOUSE_RX_COUNT:AtomicU64=AtomicU64::new(0);
static KVM_RAW_MOUSE_COUNT:AtomicU64=AtomicU64::new(0);
static KVM_RAW_READ_ERROR:AtomicU64=AtomicU64::new(0);
static KVM_MOUSE_BUTTONS:AtomicU64=AtomicU64::new(0);
static KVM_HOOK_THREAD_ID:AtomicU64=AtomicU64::new(0);
fn hook_slot()->&'static Mutex<Option<HookState>>{HOOK_STATE.get_or_init(||Mutex::new(None))}

fn is_ctrl_vk(vk:i32)->bool{vk==VK_CONTROL||vk==VK_LCONTROL||vk==VK_RCONTROL}
fn is_alt_vk(vk:i32)->bool{vk==VK_MENU||vk==VK_LMENU||vk==VK_RMENU}

unsafe extern "system" fn keyboard_hook(n:i32,w:WPARAM,l:LPARAM)->LRESULT{
 if n!=HC_ACTION{return unsafe{CallNextHookEx(null_mut(),n,w,l)}}
 let k=unsafe{&*(l as *const KBDLLHOOKSTRUCT)};
 if k.dwExtraInfo==KVM_TAG{return unsafe{CallNextHookEx(null_mut(),n,w,l)}}
 let down=w as u32==WM_KEYDOWN||w as u32==WM_SYSKEYDOWN;
 let up=w as u32==WM_KEYUP||w as u32==WM_SYSKEYUP;
 let slot=hook_slot().lock().ok().and_then(|g|g.clone());
 if let Some(s)=slot{
  let vk=k.vkCode as i32;
  if is_ctrl_vk(vk){
   if down{s.ctrl_physical.store(true,Ordering::Release);}
   if up{s.ctrl_physical.store(false,Ordering::Release);}
  }
  if is_alt_vk(vk){
   if down{s.alt_physical.store(true,Ordering::Release);}
   if up{s.alt_physical.store(false,Ordering::Release);}
  }

  // Swallow the F12 release belonging to our toggle chord on both transitions.
  if up&&vk==VK_F12&&s.hotkey_latched.swap(false,Ordering::AcqRel){return 1}

  let ctrl_down=s.ctrl_physical.load(Ordering::Acquire);
  let alt_down=s.alt_physical.load(Ordering::Acquire);
  if down&&vk==VK_F12&&ctrl_down&&alt_down{
   if !s.connected.load(Ordering::Acquire){return unsafe{CallNextHookEx(null_mut(),n,w,l)}}
   if s.hotkey_latched.swap(true,Ordering::AcqRel){return 1}
   let next=!s.remote.load(Ordering::Acquire);
   kvm_mouse::reset_capture();
   s.remote.store(next,Ordering::Release);
   let _=s.status_tx.send(next);
   let _=s.tx.send(CtrlFrame::new(CTRL_KVM_STATE,s.session,0,next as u64,0,vec![]));
   if next{
    match read_mouse_settings(){
     Ok(settings)=>{if s.tx.send(CtrlFrame::new(CTRL_KVM_MOUSE_SETTINGS,s.session,0,0,0,settings.encode())).is_err(){s.logger.line("KVM_MOUSE_SETTINGS_SEND_FAILED");}}
     Err(e)=>logln!(s.logger,"KVM_MOUSE_SETTINGS_READ_FAILED={e}"),
    }
   }
   let _=s.tx.send(CtrlFrame::new(CTRL_KVM_RESET,s.session,0,0,0,vec![]));
   // Release any synthetic state but DO NOT modify physical modifier tracking.
   inject_reset();
   return 1;
  }

  if !s.connected.load(Ordering::Acquire)&&s.remote.swap(false,Ordering::AcqRel){
   let _=s.status_tx.send(false);
   inject_reset();
  }

  if s.remote.load(Ordering::Acquire)&&(down||up){
   let mut p=vec![];
   p.extend_from_slice(&k.vkCode.to_le_bytes());
   p.extend_from_slice(&k.scanCode.to_le_bytes());
   p.extend_from_slice(&k.flags.to_le_bytes());
   p.push(down as u8);
   let _=s.tx.send(CtrlFrame::new(CTRL_KVM_KEY,s.session,0,0,0,p));
   return 1;
  }
 }
 unsafe{CallNextHookEx(null_mut(),n,w,l)}
}

unsafe extern "system" fn mouse_hook(n:i32,w:WPARAM,l:LPARAM)->LRESULT{
 if n!=HC_ACTION{return unsafe{CallNextHookEx(null_mut(),n,w,l)}}
 let m=unsafe{&*(l as *const MSLLHOOKSTRUCT)};
 if m.dwExtraInfo==KVM_TAG{return unsafe{CallNextHookEx(null_mut(),n,w,l)}}
 if let Some(s)=hook_slot().lock().ok().and_then(|g|g.clone()){
  if s.remote.load(Ordering::Acquire)&&s.connected.load(Ordering::Acquire){
   // Only suppress local legacy input here. WM_INPUT on the same thread
   // forwards movement/buttons/wheels once, using device deltas rather than
   // MSLLHOOKSTRUCT.pt - GetCursorPos() (which may be zero or DPI-scaled).
   return 1;
  }
 }
 unsafe{CallNextHookEx(null_mut(),n,w,l)}
}

fn start_kvm_hooks(logger:Logger)->thread::JoinHandle<()>{
 let(ready_tx,ready_rx)=mpsc::sync_channel(1);
 let worker=thread::spawn(move||unsafe{
  let hwnd=match kvm_mouse::create_capture_window(){
   Ok(hwnd)=>hwnd,
   Err(code)=>{logger.line(format!("KVM_RAW_INPUT_FAILED win32={code}"));let _=ready_tx.send(());return}
  };
  let instance=GetModuleHandleW(null());
  let kh=SetWindowsHookExW(WH_KEYBOARD_LL,keyboard_hook,instance,0);
  let key_error=if kh.is_null(){GetLastError()}else{0};
  let mh=SetWindowsHookExW(WH_MOUSE_LL,mouse_hook,instance,0);
  let mouse_error=if mh.is_null(){GetLastError()}else{0};
  if kh.is_null()||mh.is_null(){
   logger.line(format!("KVM_HOOK_FAILED keyboard_win32={key_error} mouse_win32={mouse_error}"));
   if !kh.is_null(){UnhookWindowsHookEx(kh);}
   if !mh.is_null(){UnhookWindowsHookEx(mh);}
   kvm_mouse::destroy_capture_window(hwnd);
   let _=ready_tx.send(());return
  }
  // The window has created the message queue before publishing the thread ID.
  KVM_HOOK_THREAD_ID.store(GetCurrentThreadId() as u64,Ordering::Release);
  logger.line("KVM_READY hotkey=Ctrl+Alt+F12 mouse=raw_input fix=2");
  let _=ready_tx.send(());
  let mut msg:MSG=std::mem::zeroed();
  while GetMessageW(&mut msg,null_mut(),0,0)>0{TranslateMessage(&msg);DispatchMessageW(&msg);}
  UnhookWindowsHookEx(kh);UnhookWindowsHookEx(mh);
  kvm_mouse::destroy_capture_window(hwnd);
  KVM_HOOK_THREAD_ID.store(0,Ordering::Release);
 });
 let _=ready_rx.recv();
 worker
}

fn inject_key(p:&[u8]){
 if p.len()<13{return}
 let vk=u32::from_le_bytes(p[0..4].try_into().unwrap()) as u16;
 let scan=u32::from_le_bytes(p[4..8].try_into().unwrap()) as u16;
 let source_flags=u32::from_le_bytes(p[8..12].try_into().unwrap());
 let down=p[12]!=0;
 let mut flags=if down{0}else{KEYEVENTF_KEYUP};
 let(w_vk,w_scan)=if scan!=0{
  flags|=KEYEVENTF_SCANCODE;
  if source_flags&LLKHF_EXTENDED!=0{flags|=KEYEVENTF_EXTENDEDKEY;}
  (0,scan)
 }else{(vk,0)};
 let input=INPUT{r#type:INPUT_KEYBOARD,u:INPUTUNION{ki:KEYBDINPUT{wVk:w_vk,wScan:w_scan,dwFlags:flags,time:0,dwExtraInfo:KVM_TAG}}};
 unsafe{let _=SendInput(1,&input,std::mem::size_of::<INPUT>() as i32);}
}

fn decode_mouse_input(p:&[u8])->Result<INPUT,u32>{
 if p.len()<16{return Err(87)}
 let msg=u32::from_le_bytes(p[0..4].try_into().unwrap());
 let dx=i32::from_le_bytes(p[4..8].try_into().unwrap());
 let dy=i32::from_le_bytes(p[8..12].try_into().unwrap());
 let data=u32::from_le_bytes(p[12..16].try_into().unwrap());
 let(flags,mouse_data,mx,my)=match msg{
  WM_MOUSEMOVE=>(MOUSEEVENTF_MOVE,0,dx,dy),
  WM_LBUTTONDOWN=>(MOUSEEVENTF_LEFTDOWN,0,0,0),
  WM_LBUTTONUP=>(MOUSEEVENTF_LEFTUP,0,0,0),
  WM_RBUTTONDOWN=>(MOUSEEVENTF_RIGHTDOWN,0,0,0),
  WM_RBUTTONUP=>(MOUSEEVENTF_RIGHTUP,0,0,0),
  WM_MBUTTONDOWN=>(MOUSEEVENTF_MIDDLEDOWN,0,0,0),
  WM_MBUTTONUP=>(MOUSEEVENTF_MIDDLEUP,0,0,0),
  WM_XBUTTONDOWN=>(MOUSEEVENTF_XDOWN,data,0,0),
  WM_XBUTTONUP=>(MOUSEEVENTF_XUP,data,0,0),
  WM_MOUSEWHEEL=>(MOUSEEVENTF_WHEEL,data,0,0),
  WM_MOUSEHWHEEL=>(MOUSEEVENTF_HWHEEL,data,0,0),
  _=>return Err(87),
 };
 Ok(INPUT{r#type:INPUT_MOUSE,u:INPUTUNION{mi:MOUSEINPUT{dx:mx,dy:my,mouseData:mouse_data,dwFlags:flags,time:0,dwExtraInfo:KVM_TAG}}})
}

fn inject_mouse(p:&[u8])->Result<(),u32>{
 let input=decode_mouse_input(p)?;
 let ok=unsafe{SendInput(1,&input,std::mem::size_of::<INPUT>() as i32)};
 if ok==0{return Err(unsafe{GetLastError()})}
 let mi=unsafe{input.u.mi};
 for(down,up,mask)in [(MOUSEEVENTF_LEFTDOWN,MOUSEEVENTF_LEFTUP,1),(MOUSEEVENTF_RIGHTDOWN,MOUSEEVENTF_RIGHTUP,2),(MOUSEEVENTF_MIDDLEDOWN,MOUSEEVENTF_MIDDLEUP,4),(MOUSEEVENTF_XDOWN,MOUSEEVENTF_XUP,if mi.mouseData==2{16}else{8})]{
  if mi.dwFlags&down!=0{KVM_MOUSE_BUTTONS.fetch_or(mask,Ordering::AcqRel);}
  if mi.dwFlags&up!=0{KVM_MOUSE_BUTTONS.fetch_and(!mask,Ordering::AcqRel);}
 }
 Ok(())
}

fn inject_reset(){
 let buttons=KVM_MOUSE_BUTTONS.swap(0,Ordering::AcqRel);
 for(mask,flags,data)in [(1,MOUSEEVENTF_LEFTUP,0),(2,MOUSEEVENTF_RIGHTUP,0),(4,MOUSEEVENTF_MIDDLEUP,0),(8,MOUSEEVENTF_XUP,1),(16,MOUSEEVENTF_XUP,2)]{
  if buttons&mask!=0{
   let input=INPUT{r#type:INPUT_MOUSE,u:INPUTUNION{mi:MOUSEINPUT{dx:0,dy:0,mouseData:data,dwFlags:flags,time:0,dwExtraInfo:KVM_TAG}}};
   unsafe{let _=SendInput(1,&input,std::mem::size_of::<INPUT>() as i32);}
  }
 }
 for vk in [0x10u16,0x11,0x12,0x5B,0x5C]{
  let input=INPUT{r#type:INPUT_KEYBOARD,u:INPUTUNION{ki:KEYBDINPUT{wVk:vk,wScan:0,dwFlags:KEYEVENTF_KEYUP,time:0,dwExtraInfo:KVM_TAG}}};
  unsafe{let _=SendInput(1,&input,std::mem::size_of::<INPUT>() as i32);}
 }
}

// ============================================================
// V10 tray application shell: hidden window, log/exit menu.
// ============================================================

#[derive(Clone)]
struct TrayContext{stop:Arc<AtomicBool>,system_shutdown:Arc<AtomicBool>,log_path:PathBuf,logger:Logger}
static TRAY_CONTEXT:OnceLock<Mutex<Option<TrayContext>>>=OnceLock::new();
fn tray_context()->&'static Mutex<Option<TrayContext>>{TRAY_CONTEXT.get_or_init(||Mutex::new(None))}

fn tray_icon_pixels()->Vec<u32>{
 let mut pixels=vec![0u32;(TRAY_ICON_SIZE*TRAY_ICON_SIZE) as usize];
 let mut rounded_rect=|x:i32,y:i32,w:i32,h:i32,r:i32,color:u32|{
  for py in y..y+h{for px in x..x+w{
   let cx=if px<x+r{x+r}else if px>=x+w-r{x+w-r-1}else{px};
   let cy=if py<y+r{y+r}else if py>=y+h-r{y+h-r-1}else{py};
   let dx=px-cx;let dy=py-cy;
   if dx*dx+dy*dy<=r*r{pixels[(py*TRAY_ICON_SIZE+px) as usize]=color;}
  }}
 };
 rounded_rect(1,1,30,30,7,0xFF31506B);
 rounded_rect(2,2,28,28,6,0xFF10243B);
 rounded_rect(3,6,11,11,2,0xFFB8E9F0);
 rounded_rect(5,8,7,6,1,0xFF20BFD0);
 rounded_rect(18,6,11,11,2,0xFFFFD19A);
 rounded_rect(20,8,7,6,1,0xFFF5A94F);
 rounded_rect(7,17,3,3,1,0xFFB8E9F0);
 rounded_rect(5,20,7,2,1,0xFFB8E9F0);
 rounded_rect(22,17,3,3,1,0xFFFFD19A);
 rounded_rect(20,20,7,2,1,0xFFFFD19A);
 rounded_rect(12,11,8,2,1,0xFFB9F36A);
 rounded_rect(9,25,14,2,1,0xFF63859D);
 rounded_rect(14,23,4,4,2,0xFFB9F36A);
 drop(rounded_rect);
 for (cx,cy) in [(13,12),(19,12)]{
  for py in cy-1..=cy+1{for px in cx-1..=cx+1{
   let dx=px-cx;let dy=py-cy;
   if dx*dx+dy*dy<=1{pixels[(py*TRAY_ICON_SIZE+px) as usize]=0xFFB9F36A;}
  }}
 }
 pixels
}

unsafe fn load_embedded_icon()->HICON{
 let hinst=unsafe{GetModuleHandleW(null())};
 if hinst.is_null(){return null_mut()}
 // build.rs embeds assets/oti_link.ico as resource ID 1.
 unsafe{LoadIconW(hinst,1usize as *const u16)}
}

unsafe fn create_tray_icon()->Result<HICON,u32>{
 let mut info:BITMAPINFO=unsafe{std::mem::zeroed()};
 info.header=BITMAPINFOHEADER{size:std::mem::size_of::<BITMAPINFOHEADER>() as u32,width:TRAY_ICON_SIZE,height:-TRAY_ICON_SIZE,planes:1,bit_count:32,compression:0,size_image:0,x_pels_per_meter:0,y_pels_per_meter:0,colors_used:0,colors_important:0};
 let mut bits=null_mut();
 let color=unsafe{CreateDIBSection(null_mut(),&info,0,&mut bits,null_mut(),0)};
 if color.is_null(){return Err(unsafe{GetLastError()})}
 let mask=unsafe{CreateBitmap(TRAY_ICON_SIZE,TRAY_ICON_SIZE,1,1,null())};
 if mask.is_null(){
  let error=unsafe{GetLastError()};unsafe{DeleteObject(color);}return Err(error)
 }
 let pixels=tray_icon_pixels();
 unsafe{std::ptr::copy_nonoverlapping(pixels.as_ptr(),bits.cast::<u32>(),pixels.len());}
 let icon_info=ICONINFO{is_icon:1,x_hotspot:0,y_hotspot:0,mask,color};
 let icon=unsafe{CreateIconIndirect(&icon_info)};
 let error=if icon.is_null(){unsafe{GetLastError()}}else{0};
 unsafe{DeleteObject(mask);DeleteObject(color);}
 if icon.is_null(){Err(error)}else{Ok(icon)}
}

fn shell_open(hwnd:HWND,path:&Path){
 unsafe{
  let op=wide_null("open");
  let p=wide_null(path.as_os_str());
  let _=ShellExecuteW(hwnd,op.as_ptr(),p.as_ptr(),null(),null(),SW_SHOWNORMAL);
 }
}

fn tray_open_log(hwnd:HWND){
 if let Some(c)=tray_context().lock().ok().and_then(|g|g.clone()){shell_open(hwnd,&c.log_path);}
}
fn tray_open_log_dir(hwnd:HWND){
 if let Some(c)=tray_context().lock().ok().and_then(|g|g.clone()){
  if let Some(p)=c.log_path.parent(){shell_open(hwnd,p);}
 }
}
fn tray_request_exit(){
 if let Some(c)=tray_context().lock().ok().and_then(|g|g.clone()){
  c.logger.line("TRAY_EXIT_REQUESTED");
  c.stop.store(true,Ordering::Release);
 }
}

unsafe fn tray_popup(hwnd:HWND){
 let menu=unsafe{CreatePopupMenu()};
 if menu.is_null(){return}
 let a=wide_null("打开日志");
 let b=wide_null("打开日志文件夹");
 let c=wide_null("退出 OTI-Link");
 unsafe{
  let _=AppendMenuW(menu,MF_STRING,TRAY_CMD_LOG,a.as_ptr());
  let _=AppendMenuW(menu,MF_STRING,TRAY_CMD_LOG_DIR,b.as_ptr());
  let _=AppendMenuW(menu,MF_SEPARATOR,0,null());
  let _=AppendMenuW(menu,MF_STRING,TRAY_CMD_EXIT,c.as_ptr());
  let mut p=POINT::default();let _=GetCursorPos(&mut p);let _=SetForegroundWindow(hwnd);
  let cmd=TrackPopupMenu(menu,TPM_RIGHTBUTTON|TPM_NONOTIFY|TPM_RETURNCMD,p.x,p.y,0,hwnd,null());
  let _=DestroyMenu(menu);
  match cmd as usize{
   TRAY_CMD_LOG=>tray_open_log(hwnd),
   TRAY_CMD_LOG_DIR=>tray_open_log_dir(hwnd),
   TRAY_CMD_EXIT=>{tray_request_exit();},
   _=>{}
  }
 }
}

fn tray_request_system_shutdown(){
 if let Some(c)=tray_context().lock().ok().and_then(|g|g.clone()){
  if !c.system_shutdown.swap(true,Ordering::AcqRel){c.logger.line("SYSTEM_ENDSESSION_REQUESTED");}
 }
}

unsafe extern "system" fn tray_wnd_proc(hwnd:HWND,msg:u32,w:WPARAM,l:LPARAM)->LRESULT{
 match msg{
  WM_QUERYENDSESSION=>{
   tray_request_system_shutdown();
   return 1
  }
  WM_ENDSESSION=>{
   if w!=0{
    tray_request_system_shutdown();
    if let Some(c)=tray_context().lock().ok().and_then(|g|g.clone()){c.stop.store(true,Ordering::Release);}
   }
   return 0
  }
  WM_TRAY_CALLBACK=>{
   let ev=l as u32;
   if ev==WM_RBUTTONUP||ev==WM_CONTEXTMENU{unsafe{tray_popup(hwnd)};return 0}
   if ev==WM_LBUTTONDBLCLK{tray_open_log(hwnd);return 0}
  }
  WM_DESTROY=>{unsafe{PostQuitMessage(0)};return 0}
  _=>{}
 }
 unsafe{DefWindowProcW(hwnd,msg,w,l)}
}

struct Tray{
 tip:Arc<Mutex<String>>,
 thread_id:u32,
 hwnd:HWND,
 join:Option<thread::JoinHandle<()>>,
}
impl Tray{
 fn new(tip:&str,log_path:PathBuf,stop:Arc<AtomicBool>,system_shutdown:Arc<AtomicBool>,logger:Logger)->Option<Self>{
  if let Ok(mut g)=tray_context().lock(){*g=Some(TrayContext{stop,system_shutdown,log_path,logger:logger.clone()});}
  let tip_shared=Arc::new(Mutex::new(tip.to_string()));
  let tip_thread=tip_shared.clone();
  let logger_thread=logger.clone();
  let(ready_tx,ready_rx)=mpsc::sync_channel::<Option<(u32,usize)>>(1);
  let join=thread::spawn(move||unsafe{
   let tid=GetCurrentThreadId();
   let class_name=wide_null("OTI_Link_Tray_Window");
   let hinst=GetModuleHandleW(null());
   let app_icon=load_embedded_icon();
   let wc=WNDCLASSW{style:0,lpfnWndProc:Some(tray_wnd_proc),cbClsExtra:0,cbWndExtra:0,hInstance:hinst,hIcon:app_icon,hCursor:null_mut(),hbrBackground:null_mut(),lpszMenuName:null(),lpszClassName:class_name.as_ptr()};
   if RegisterClassW(&wc)==0{let _=ready_tx.send(None);return}
   let hwnd=CreateWindowExW(0,class_name.as_ptr(),class_name.as_ptr(),0,0,0,0,0,null_mut(),null_mut(),hinst,null_mut());
   if hwnd.is_null(){let _=ready_tx.send(None);return}
   let mut d:NOTIFYICONDATAW=std::mem::zeroed();
   d.cbSize=std::mem::size_of::<NOTIFYICONDATAW>() as u32;
   d.hWnd=hwnd;d.uID=10;d.uFlags=NIF_MESSAGE|NIF_ICON|NIF_TIP;d.uCallbackMessage=WM_TRAY_CALLBACK;
   let (icon,owned_icon)=if !app_icon.is_null(){
    logger_thread.line("TRAY_ICON=embedded_resource id=1");
    (app_icon,false)
   }else{
    match create_tray_icon(){
     Ok(icon)=>{logger_thread.line("TRAY_ICON=runtime_fallback");(icon,true)},
     Err(code)=>{logln!(logger_thread,"TRAY_CUSTOM_ICON_FAILED win32={code}; using system icon");(LoadIconW(null_mut(),IDI_APPLICATION as *const u16),false)}
    }
   };
   if icon.is_null(){let _=DestroyWindow(hwnd);let _=ready_tx.send(None);return}
   d.hIcon=icon;
   if let Ok(t)=tip_thread.lock(){set_tip(&mut d,&t);}
   if Shell_NotifyIconW(NIM_ADD,&mut d)==0{if owned_icon{let _=DestroyIcon(icon);}let _=DestroyWindow(hwnd);let _=ready_tx.send(None);return}
   let _=ready_tx.send(Some((tid,hwnd as usize)));
   let mut msg:MSG=std::mem::zeroed();
   loop{
    let r=GetMessageW(&mut msg,null_mut(),0,0);
    if r<=0{break}
    if msg.hwnd.is_null()&&msg.message==WM_TRAY_UPDATE{
     if let Ok(t)=tip_thread.lock(){set_tip(&mut d,&t);let _=Shell_NotifyIconW(NIM_MODIFY,&mut d);}
     continue
    }
    if msg.hwnd.is_null()&&msg.message==WM_TRAY_SHUTDOWN{break}
    TranslateMessage(&msg);DispatchMessageW(&msg);
   }
   let _=Shell_NotifyIconW(NIM_DELETE,&mut d);
   if owned_icon{let _=DestroyIcon(icon);}
   let _=DestroyWindow(hwnd);
  });
  match ready_rx.recv_timeout(Duration::from_secs(3)){
   Ok(Some((thread_id,hwnd)))=>{logger.line("TRAY_READY right_click=menu double_click=open_log");Some(Self{tip:tip_shared,thread_id,hwnd:hwnd as HWND,join:Some(join)})}
   _=>{logger.line("TRAY_INIT_FAILED");let _=join.join();if let Ok(mut g)=tray_context().lock(){*g=None;}None}
  }
 }
 fn hwnd(&self)->HWND{self.hwnd}
 fn update(&mut self,tip:&str){
  if let Ok(mut t)=self.tip.lock(){*t=tip.to_string();}
  unsafe{let _=PostThreadMessageW(self.thread_id,WM_TRAY_UPDATE,0,0);}
 }
}
impl Drop for Tray{
 fn drop(&mut self){
  unsafe{let _=PostThreadMessageW(self.thread_id,WM_TRAY_SHUTDOWN,0,0);}
  if let Some(h)=self.join.take(){let _=h.join();}
  if let Ok(mut g)=tray_context().lock(){*g=None;}
 }
}
unsafe fn set_tip(d:&mut NOTIFYICONDATAW,s:&str){d.szTip=[0;128];for(i,c)in OsStr::new(s).encode_wide().take(127).enumerate(){d.szTip[i]=c;}}

pub fn show_error_dialog(text:&str){
 unsafe{
  let title=wide_null("OTI-Link");
  let body=wide_null(text);
  let _=MessageBoxW(null_mut(),body.as_ptr(),title.as_ptr(),MB_OK|MB_ICONERROR);
 }
}

// Force required system libraries into the linker set.
#[link(name="Kernel32")] unsafe extern "system"{}
#[link(name="Shell32")] unsafe extern "system"{}
#[link(name="Ole32")] unsafe extern "system"{}
#[link(name="User32")] unsafe extern "system"{}

// ============================================================
// Mount and session lifecycle
// ============================================================

type OtiHost = FileSystemHost<RemoteFs, FineGuard>;

fn choose_drive()->AppResult<String>{let mask=unsafe{GetLogicalDrives()};for l in ['R','S','T','U','V','W','X','Y','Z','Q','P','O','N','M']{if mask&(1<<((l as u8)-b'A'))==0{return Ok(format!("{l}:"))}}Err("no free drive letter".into())}
fn mount_peer(config:AppConfig,peer:Manifest,rpc:RpcClient,cache:Arc<CacheState>,session:u64,logger:&Logger)->AppResult<(OtiHost,MountState)>{let drive=choose_drive()?;let mut label=format!("OTI-{}",peer.hostname);label=label.chars().take(32).collect();let fs=RemoteFs{config,peer:peer.clone(),rpc,label:label.clone(),cache};let mut p=VolumeParams::new();p.filesystem_name("OTILINK").read_only_volume(!config.writable).case_sensitive_search(false).case_preserved_names(true).unicode_on_disk(true).persistent_acls(false).file_info_timeout(if config.cache{1000}else{0}).dir_info_timeout(if config.cache{1000}else{0}).volume_info_timeout(1000).flush_and_purge_on_cleanup(config.writable);let mut host:OtiHost=FileSystemHost::<RemoteFs,FineGuard>::new(p,fs)?;host.mount(&drive)?;host.start_with_threads(8)?;let state=MountState{drive:drive.clone(),label:label.clone(),peer:peer.hostname.clone(),session,pid:std::process::id()};save_state(config.version,&state)?;logln!(logger,"MOUNTED {} label='{}' writable={}",drive,label,config.writable);Ok((host,state))}
fn unmount(host:&mut Option<OtiHost>,config:AppConfig,logger:&Logger){if let Some(h)=host.as_mut(){h.stop();h.unmount();logger.line("UNMOUNTED");}*host=None;remove_state(config.version);}

fn run_connected(config:AppConfig,stop:Arc<AtomicBool>,system_shutdown:Arc<AtomicBool>,tray:&mut Option<Tray>,logger:&Logger)->AppResult<()> {
 let(local_manifest,exports)=build_manifest()?;let session=new_id();let manifest_payload=encode_manifest(&local_manifest)?;logln!(logger,"SESSION={session:016X} host={} write={} cache={} kvm={} clipboard={}",local_manifest.hostname,config.writable,config.cache,config.kvm,config.clipboard);
 let iface=open_interface(logger,&stop)?;let cw=ctrl_writer(&iface)?;let cr=ctrl_reader(&iface)?;let dw=data_writer(&iface)?;let dr=data_reader(&iface)?;drop(iface);
 let(event_tx,event_rx)=mpsc::channel();let(ctrl_tx,ctrl_rx)=mpsc::channel();let(server_tx,server_rx)=mpsc::channel();let(data_job_tx,data_job_rx)=mpsc::channel();let(watch_tx,watch_rx)=mpsc::channel();
 let pending:CtrlPending=Arc::new(Mutex::new(HashMap::new()));let data_pending:DataPending=Arc::new(Mutex::new(HashMap::new()));let writes:ActiveWrites=Arc::new(Mutex::new(HashMap::new()));let connected=Arc::new(AtomicBool::new(false));let alive=Arc::new(AtomicBool::new(true));
 let rpc=RpcClient{session,ctrl_tx:ctrl_tx.clone(),ctrl_pending:pending.clone(),data_pending:data_pending.clone(),data_tx:data_job_tx.clone(),next:Arc::new(AtomicU64::new(1)),connected:connected.clone()};
 let _cw=spawn_ctrl_writer(cw,ctrl_rx,event_tx.clone(),alive.clone());let _cr=spawn_ctrl_reader(cr,pending,data_pending.clone(),server_tx,event_tx.clone(),alive.clone(),logger.clone());let _srv=spawn_server(exports.clone(),config,session,server_rx,ctrl_tx.clone(),data_job_tx.clone(),writes.clone(),watch_tx,logger.clone());let _dw=spawn_data_writer(dw,data_job_rx,ctrl_tx.clone(),session,logger.clone(),alive.clone());let _dr=spawn_data_reader(dr,data_pending,writes,ctrl_tx.clone(),session,logger.clone(),alive.clone());
 let _watch=if config.change_sync{Some(spawn_change_watcher(exports.clone(),session,ctrl_tx.clone(),watch_rx,logger.clone(),alive.clone()))}else{None};
 let mount_drive:Arc<Mutex<Option<String>>>=Arc::new(Mutex::new(None));
 let clipboard_owner=tray.as_ref().map(|t|t.hwnd() as usize).unwrap_or(0);
 let(clip_cmd_tx,clip_cmd_rx)=mpsc::channel();let _clip=if config.clipboard{Some(spawn_clipboard(session,ctrl_tx.clone(),clip_cmd_rx,alive.clone(),connected.clone(),exports.clone(),mount_drive.clone(),clipboard_owner,logger.clone()))}else{None};
 let(kvm_status_tx,kvm_status_rx)=mpsc::channel::<bool>();let mut kvm_remote=false;
 let mut kvm_last_log=Instant::now();let mut kvm_last_tx=KVM_MOUSE_TX_COUNT.load(Ordering::Relaxed);
 if config.kvm{if let Ok(mut slot)=hook_slot().lock(){*slot=Some(HookState{session,tx:ctrl_tx.clone(),remote:Arc::new(AtomicBool::new(false)),connected:connected.clone(),status_tx:kvm_status_tx,logger:logger.clone(),ctrl_physical:Arc::new(AtomicBool::new(false)),alt_physical:Arc::new(AtomicBool::new(false)),hotkey_latched:Arc::new(AtomicBool::new(false))});}}
 if let Some(t)=tray.as_mut(){t.update("OTI-Link: synchronizing...");}
 let cache=Arc::new(CacheState::new());let mut host:Option<OtiHost>=None;let mut peer_session:Option<u64>=None;let mut peer_manifest:Option<Manifest>=None;let mut peer_ready=false;let mut last_seen=Instant::now();let mut handshake_progress=Instant::now();let mut last_announce=Instant::now()-Duration::from_secs(20);let mut last_hb=Instant::now()-Duration::from_secs(10);
 let mut mouse_settings_sync=MouseSettingsSync::new(logger.clone());

 // Drop only the WinFsp/peer state. Keep this local USB endpoint/session alive
 // whenever possible so the peer can reconnect without both sides chasing each
 // other through fresh USB sessions.
 let reset_peer = |host:&mut Option<OtiHost>,peer_session:&mut Option<u64>,peer_manifest:&mut Option<Manifest>,peer_ready:&mut bool,connected:&Arc<AtomicBool>,cache:&Arc<CacheState>,tray:&mut Option<Tray>,mouse_settings_sync:&mut MouseSettingsSync,mount_drive:&Arc<Mutex<Option<String>>>| {
  connected.store(false,Ordering::Release);
  if let Ok(mut d)=mount_drive.lock(){*d=None;}
  if let Ok(slot)=hook_slot().lock(){if let Some(h)=slot.as_ref(){h.remote.store(false,Ordering::Release);}}
  if config.kvm{inject_reset();}
  mouse_settings_sync.restore();
  unmount(host,config,logger);
  *peer_session=None;
  *peer_manifest=None;
  *peer_ready=false;
  cache.invalidate("\\");
  if let Some(t)=tray.as_mut(){t.update(&format!("OTI-Link {}: waiting for peer",config.version));}
 };

 loop{
  if system_shutdown.load(Ordering::Acquire){
   logger.line("SYSTEM_ENDSESSION_PREPARE: notifying peer and releasing OTI session");
   if let Some(ps)=peer_session{
    let _=ctrl_tx.send(CtrlFrame::new(CTRL_SESSION_ENDING,session,0,1,ps,vec![]));
    // Give the dedicated Lane-1 writer a short window to put the notice on USB
    // before this process releases the endpoints for Windows shutdown/restart.
    thread::sleep(Duration::from_millis(180));
   }
   connected.store(false,Ordering::Release);
   if let Ok(mut d)=mount_drive.lock(){*d=None;}
   if config.kvm{inject_reset();}
   mouse_settings_sync.restore();
   unmount(&mut host,config,logger);
   alive.store(false,Ordering::Release);
   logger.line("SYSTEM_ENDSESSION_RELEASED");
   return Ok(())
  }
  if stop.load(Ordering::Acquire){alive.store(false,Ordering::Release);connected.store(false,Ordering::Release);if let Ok(mut d)=mount_drive.lock(){*d=None;}if config.kvm{inject_reset();}unmount(&mut host,config,logger);return Ok(())}
  // Log on the session thread, never from a time-critical input hook.
  if config.kvm&&kvm_last_log.elapsed()>=Duration::from_secs(1){
   let count=KVM_MOUSE_TX_COUNT.load(Ordering::Relaxed);
   if count!=kvm_last_tx{logln!(logger,"KVM_MOUSE_TX count={count} raw_packets={}",KVM_RAW_MOUSE_COUNT.load(Ordering::Relaxed));kvm_last_tx=count;}
   let error=KVM_RAW_READ_ERROR.swap(0,Ordering::Relaxed);
   if error!=0{logln!(logger,"KVM_RAW_READ_FAILED win32={}",error-1);}
   kvm_last_log=Instant::now();
  }
  while let Ok(remote)=kvm_status_rx.try_recv(){
   kvm_remote=remote;
   logln!(logger,"KVM_LOCAL_TARGET={}",if remote{"REMOTE"}else{"LOCAL"});
   if let(Some(t),Some(peer))=(tray.as_mut(),peer_manifest.as_ref()){t.update(&format!("OTI-Link {}: {} KVM={}",config.version,peer.hostname,if remote{"REMOTE"}else{"LOCAL"}));}
  }

  // Discovery traffic is intentionally sparse while no peer is known. On this
  // bridge, an OUT transfer can wait until the remote IN endpoint is posted.
  // Sparse HELLOs avoid building a large stale FIFO during a cable unplug.
  let announce_interval=if peer_session.is_none(){Duration::from_secs(5)}else if host.is_none(){Duration::from_secs(1)}else{Duration::from_secs(10)};
  if last_announce.elapsed()>=announce_interval{
   let _=ctrl_tx.send(CtrlFrame::new(CTRL_HELLO,session,0,0,0,local_manifest.hostname.as_bytes().to_vec()));
   if peer_session.is_some()&&host.is_none(){let _=ctrl_tx.send(CtrlFrame::new(CTRL_MANIFEST,session,0,0,0,manifest_payload.clone()));}
   last_announce=Instant::now();
  }

  // Heartbeat is meaningful only after the filesystem has actually been
  // synchronized/mounted. Before that, HELLO/MANIFEST drive discovery.
  if host.is_some()&&last_hb.elapsed()>=HEARTBEAT_INTERVAL{
   let _=ctrl_tx.send(CtrlFrame::new(CTRL_HEARTBEAT,session,0,0,0,vec![]));
   last_hb=Instant::now();
  }

  // A peer heartbeat timeout no longer destroys a locally-valid USB endpoint.
  // Unmount immediately, clear peer identity, and wait for a fresh HELLO.
  if host.is_some()&&last_seen.elapsed()>PEER_STALE{
   logger.line("PEER_STALE: unmounting remote drive; keeping local USB session open for reconnect");
   reset_peer(&mut host,&mut peer_session,&mut peer_manifest,&mut peer_ready,&connected,&cache,tray,&mut mouse_settings_sync,&mount_drive);
   last_announce=Instant::now()-Duration::from_secs(20);
   last_seen=Instant::now();
   handshake_progress=Instant::now();
  }

  // A physical replug can leave an endpoint locally valid but no longer
  // connected to the bridge's new peer-side endpoint. If the handshake makes
  // no progress, force a fresh MI_05 claim instead of waiting forever.
  if host.is_none()&&handshake_progress.elapsed()>HANDSHAKE_REOPEN{
   if peer_session.is_none(){
    logger.line("HANDSHAKE_TIMEOUT=no peer HELLO; reopening MI_05");
   }else{
    logln!(logger,"HANDSHAKE_TIMEOUT=peer session {:016X} did not complete; reopening MI_05",peer_session.unwrap_or(0));
   }
   alive.store(false,Ordering::Release);
   connected.store(false,Ordering::Release);
   return Err("peer handshake timeout; reopen MI_05".into())
  }

  match event_rx.recv_timeout(Duration::from_millis(100)){
   Ok(MainEvent::Activity{session:s})=>{
    if peer_session==Some(s){last_seen=Instant::now();}
   }
   Ok(MainEvent::Hello{session:s,host:h})=>{
    last_seen=Instant::now();handshake_progress=Instant::now();
    if let Some(old)=peer_session{
     if old!=s{
      logln!(logger,"PEER_SESSION_CHANGED old={old:016X} new={s:016X} host={h}");
      if host.is_some(){reset_peer(&mut host,&mut peer_session,&mut peer_manifest,&mut peer_ready,&connected,&cache,tray,&mut mouse_settings_sync,&mount_drive);}
      else{peer_manifest=None;peer_ready=false;cache.invalidate("\\");}
     }
    }
    peer_session=Some(s);
    logln!(logger,"PEER_HELLO {s:016X} {h}");
    // Respond with manifest promptly after learning a peer identity.
    let _=ctrl_tx.send(CtrlFrame::new(CTRL_MANIFEST,session,0,0,0,manifest_payload.clone()));
    last_announce=Instant::now();
   }

   Ok(MainEvent::Manifest{session:s,m})=>{
    last_seen=Instant::now();handshake_progress=Instant::now();
    if peer_session.map(|x|x!=s).unwrap_or(false){
     logln!(logger,"PEER_MANIFEST_SESSION_CHANGED old={:016X} new={s:016X}",peer_session.unwrap_or(0));
     if host.is_some(){reset_peer(&mut host,&mut peer_session,&mut peer_manifest,&mut peer_ready,&connected,&cache,tray,&mut mouse_settings_sync,&mount_drive);}
     else{mouse_settings_sync.restore();peer_ready=false;cache.invalidate("\\");}
    }
    peer_session=Some(s);
    peer_manifest=Some(m);
    let _=ctrl_tx.send(CtrlFrame::new(CTRL_READY,session,0,s,0,vec![]));
   }

   Ok(MainEvent::Ready{session:s,accepted})=>{
    last_seen=Instant::now();handshake_progress=Instant::now();
    if accepted==session{
     if peer_session.map(|x|x==s).unwrap_or(true){
      peer_session=Some(s);
      peer_ready=true;
      logln!(logger,"PEER_READY {s:016X}");
     }else{
      logln!(logger,"STALE_READY_IGNORED session={s:016X} current={:016X}",peer_session.unwrap_or(0));
     }
    }
   }

   Ok(MainEvent::Heartbeat{session:s})=>{
    if peer_session==Some(s){last_seen=Instant::now();}
   }

   Ok(MainEvent::Change{session:s,path})=>{
    if peer_session==Some(s){last_seen=Instant::now();cache.invalidate(&path);logln!(logger,"REMOTE_CHANGE {path}");}
   }

   Ok(MainEvent::ClipText{session:s,text})=>{if config.clipboard&&peer_session==Some(s){let _=clip_cmd_tx.send(ClipboardCmd::Text(text));}}
   Ok(MainEvent::ClipImage{session:s,w,h,rgba})=>{if config.clipboard&&peer_session==Some(s){let _=clip_cmd_tx.send(ClipboardCmd::Image{w,h,rgba});}}
   Ok(MainEvent::ClipFiles{session:s,paths})=>{if config.clipboard&&peer_session==Some(s){let _=clip_cmd_tx.send(ClipboardCmd::Files(paths));}}
   Ok(MainEvent::KvmKey{session:s,payload})=>{if config.kvm&&peer_session==Some(s){inject_key(&payload)}}
   Ok(MainEvent::KvmMouse{session:s,payload})=>{if config.kvm&&peer_session==Some(s){let n=KVM_MOUSE_RX_COUNT.fetch_add(1,Ordering::Relaxed)+1;match inject_mouse(&payload){Ok(())=>{if n==1||n%512==0{logln!(logger,"KVM_MOUSE_RX count={n}");}},Err(code)=>logln!(logger,"KVM_MOUSE_INJECT_FAILED win32={code} count={n}")}}}
   Ok(MainEvent::KvmReset{session:s})=>{if config.kvm&&peer_session==Some(s){inject_reset()}}
   Ok(MainEvent::KvmState{session:s,remote})=>{if config.kvm&&peer_session==Some(s){logln!(logger,"KVM_PEER_TARGET={}",if remote{"REMOTE"}else{"LOCAL"});if !remote{mouse_settings_sync.restore();}}},
   Ok(MainEvent::KvmMouseSettings{session:s,payload})=>{if config.kvm&&peer_session==Some(s){match MouseSettings::decode(&payload){Ok(settings)=>match mouse_settings_sync.apply(settings){Ok(())=>logger.line("KVM_MOUSE_SETTINGS_APPLIED"),Err(e)=>logln!(logger,"KVM_MOUSE_SETTINGS_APPLY_FAILED={e}")},Err(e)=>logln!(logger,"KVM_MOUSE_SETTINGS_REJECTED={e}")}}},
   Ok(MainEvent::PeerSessionEnding{session:s,reason})=>{
    if peer_session==Some(s){
     logln!(logger,"PEER_SESSION_ENDING session={s:016X} reason={reason}; releasing mount and reopening MI_05");
     let _=ctrl_tx.send(CtrlFrame::new(CTRL_SESSION_END_ACK,session,0,s,0,vec![]));
     thread::sleep(Duration::from_millis(80));
     reset_peer(&mut host,&mut peer_session,&mut peer_manifest,&mut peer_ready,&connected,&cache,tray,&mut mouse_settings_sync,&mount_drive);
     alive.store(false,Ordering::Release);
     return Err("peer announced Windows shutdown/restart; reopen MI_05".into())
    }else{
     logln!(logger,"STALE_PEER_SESSION_ENDING_IGNORED session={s:016X} current={:016X}",peer_session.unwrap_or(0));
    }
   }
   Ok(MainEvent::PeerSessionEndAck{session:s,accepted})=>{
    if peer_session==Some(s){logln!(logger,"PEER_SESSION_END_ACK session={s:016X} accepted={accepted:016X}");}
   }

   Ok(MainEvent::Fatal(e))=>{
    alive.store(false,Ordering::Release);
    connected.store(false,Ordering::Release);
    if let Ok(mut d)=mount_drive.lock(){*d=None;}
    if config.kvm{inject_reset();}
    unmount(&mut host,config,logger);
    return Err(e.into())
   }

   Err(mpsc::RecvTimeoutError::Timeout)=>{},
   Err(_)=>{
    alive.store(false,Ordering::Release);
    connected.store(false,Ordering::Release);
    if let Ok(mut d)=mount_drive.lock(){*d=None;}
    if config.kvm{inject_reset();}
    unmount(&mut host,config,logger);
    return Err("event channel stopped".into())
   }
  }

  if host.is_none()&&peer_ready{
   if let(Some(peer),Some(ps))=(peer_manifest.clone(),peer_session){
    connected.store(true,Ordering::Release);
    let(h,state)=match mount_peer(config,peer.clone(),rpc.clone(),cache.clone(),session,logger){
     Ok(x)=>x,
     Err(e)=>{alive.store(false,Ordering::Release);connected.store(false,Ordering::Release);return Err(e)}
    };
    host=Some(h);
    if let Ok(mut d)=mount_drive.lock(){*d=Some(state.drive.clone());}
    last_seen=Instant::now();
    handshake_progress=Instant::now();
    last_hb=Instant::now()-HEARTBEAT_INTERVAL;
    logln!(logger,"SYNC_COMPLETE peer={} peer_session={ps:016X} drive={}",peer.hostname,state.drive);
    if let Some(t)=tray.as_mut(){t.update(&format!("OTI-Link {}: {} ({}) KVM={}",config.version,peer.hostname,state.drive,if kvm_remote{"REMOTE"}else{"LOCAL"}));}
   }
  }
 }
}

pub fn run(config:AppConfig)->AppResult<()> {
 let logger=Logger::new(config.version)?;
 logger.line(format!("OTI-Link {} starting",config.version));logln!(logger,"LOG={}",logger.path().display());
 logger.line("INIT_STAGE=single_instance");let _instance=acquire_single_instance()?;logger.line("SINGLE_INSTANCE=ACQUIRED scope=Global");
 let _keep_awake=KeepAwake::new(logger.clone())?;
 logger.line("INIT_STAGE=stale_cleanup");for stale in ["v8","8.1","9.0","9.1","10.0","10.0-FIX6"]{cleanup_stale(stale,&logger);}
 logger.line("INIT_STAGE=winfsp_runtime");let _fsp=init_winfsp_runtime(&logger)?;
 let stop=Arc::new(AtomicBool::new(false));
 let system_shutdown=Arc::new(AtomicBool::new(false));
 if unsafe{!GetConsoleWindow().is_null()}{
  logger.line("INIT_STAGE=signal_handler");
  let s=stop.clone();ctrlc::set_handler(move||s.store(true,Ordering::Release))?;
 }else{logger.line("SIGNAL_HANDLER=skipped(no_console)");}
 let kvm_thread=if config.kvm{Some(start_kvm_hooks(logger.clone()))}else{None};
 let mut tray=if config.tray{Tray::new(&format!("OTI-Link {}: disconnected",config.version),logger.path(),stop.clone(),system_shutdown.clone(),logger.clone())}else{None};
 while !stop.load(Ordering::Acquire){
  match run_connected(config,stop.clone(),system_shutdown.clone(),&mut tray,&logger){
   Ok(())=>break,
   Err(e)=>{
    logln!(logger,"SESSION_END={e}");remove_state(config.version);
    if let Some(t)=tray.as_mut(){t.update(&format!("OTI-Link {}: reconnecting",config.version));}
    if stop.load(Ordering::Acquire)||!config.reconnect{break}
    let es=e.to_string().to_lowercase();
    let delay=if es.contains("peer announced windows shutdown/restart"){Duration::from_millis(100)}else if es.contains("error 5")||es.contains("access is denied"){Duration::from_millis(2500+(new_id()%1500))}else{reconnect_backoff()};
    logln!(logger,"RECONNECT_BACKOFF_MS={}",delay.as_millis());
    let until=Instant::now()+delay;
    while Instant::now()<until&&!stop.load(Ordering::Acquire){thread::sleep(Duration::from_millis(50));}
   }
  }
 }
 if let Ok(mut s)=hook_slot().lock(){*s=None;}
 if config.kvm{
  inject_reset();
  let tid=KVM_HOOK_THREAD_ID.swap(0,Ordering::AcqRel) as u32;
  if tid!=0{unsafe{let _=PostThreadMessageW(tid,WM_QUIT,0,0);}}
 }
 if let Some(h)=kvm_thread{let _=h.join();}
 remove_state(config.version);
 logger.line("EXIT_CLEAN");
 drop(tray);
 Ok(())
}
