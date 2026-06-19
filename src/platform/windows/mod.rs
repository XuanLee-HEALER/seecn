//! Windows 平台实现的汇总导出。
//!
//! 导出 new_proc_scanner / new_tcp_snapshot / new_net_monitor 构造函数,
//! 以及 detect_privilege(管理员检测)。参见 DESIGN §8。

mod etw;
mod flyout;
mod proc;
mod tcptable;

use crate::model::Privilege;
use crate::platform::{NetMonitor, ProcScanner, TcpSnapshot};
use tao::event_loop::EventLoopWindowTarget;

/// 构造 Windows 进程扫描器(基于 sysinfo)。
pub fn new_proc_scanner() -> Box<dyn ProcScanner> {
    Box::new(proc::WinProcScanner::new())
}

/// 构造 Windows TCP 快照器(基于 netstat2)。
pub fn new_tcp_snapshot() -> Box<dyn TcpSnapshot> {
    Box::new(tcptable::WinTcpSnapshot::new())
}

/// 构造 Windows 实时网络监控器(基于 ferrisetw / ETW KernelNetwork)。
pub fn new_net_monitor() -> Box<dyn NetMonitor> {
    Box::new(etw::WinNetMonitor::new())
}

/// 在给定 event loop target 上构造 Windows flyout(tao 窗口 + wry WebView2)。
///
/// UI 线程独占:必须在 event loop 闭包内调用。失败(窗口 / webview 创建出错)返回 Err,
/// 由调用方决定降级(无 flyout 仍可只用托盘 + 单行 tooltip)。
pub fn new_flyout<T: 'static>(
    target: &EventLoopWindowTarget<T>,
) -> anyhow::Result<flyout::WinFlyout> {
    flyout::WinFlyout::new(target)
}

/// 检测当前进程是否以管理员(elevated)身份运行。
///
/// 通过打开当前进程 token 并查询 `TokenElevation` 信息判定。
/// 任何系统调用失败都保守地返回 `Standard`(不假设有管理员权限)。
pub fn detect_privilege() -> Privilege {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: 仅做只读的进程 token 查询;所有指针指向本地栈变量,
    // 句柄在使用后立即 CloseHandle,失败路径不解引用未初始化内存。
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Privilege::Standard;
        }

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        let size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut core::ffi::c_void,
            size,
            &mut ret_len,
        );
        CloseHandle(token);

        if ok != 0 && elevation.TokenIsElevated != 0 {
            Privilege::Elevated
        } else {
            Privilege::Standard
        }
    }
}
