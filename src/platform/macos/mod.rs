//! macOS 平台实现的汇总导出(对应 windows/mod.rs)。
//!
//! 导出 new_proc_scanner / new_tcp_snapshot / new_net_monitor / new_flyout 构造函数,
//! 以及 detect_privilege。net 走 `nettop` 常驻子进程(见 net.rs 顶部对 ntstat 直连的否决说明)。

mod flyout;
mod net;
mod proc;
mod tcptable;

use crate::model::Privilege;
use crate::platform::{NetMonitor, ProcScanner, TcpSnapshot};
use tao::event_loop::EventLoopWindowTarget;

pub use flyout::MacFlyout;

/// 构造 macOS 进程扫描器(基于 sysinfo)。
pub fn new_proc_scanner() -> Box<dyn ProcScanner> {
    Box::new(proc::MacProcScanner::new())
}

/// 构造 macOS TCP 快照器(基于 netstat2)。
pub fn new_tcp_snapshot() -> Box<dyn TcpSnapshot> {
    Box::new(tcptable::MacTcpSnapshot::new())
}

/// 构造 macOS 实时网络监控器(基于 nettop 常驻流)。
pub fn new_net_monitor() -> Box<dyn NetMonitor> {
    Box::new(net::MacNetMonitor::new())
}

/// 检测「三态是否可用」。
///
/// macOS 的 per-pid 实时字节经 `nettop` 取得,**非 root 即可**(借 nettop 的私有 entitlement),
/// 故三态恒可用。这里返回 `Elevated`(语义=三态可用),让上层日志/tooltip 不误报「两态模式」。
/// 与 Windows 语义不同:Windows 的 `Elevated/Standard` 对应「是否管理员 → ETW 是否可用」。
pub fn detect_privilege() -> Privilege {
    Privilege::Elevated
}

/// 在给定 event loop target 上构造 macOS flyout(tao 无边框窗口 + wry webview)。
pub fn new_flyout<T: 'static>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<MacFlyout> {
    MacFlyout::new(target)
}
