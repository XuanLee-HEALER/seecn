//! macOS 平台实现的汇总导出(对应 windows/mod.rs)。
//!
//! 导出 new_proc_scanner / new_tcp_snapshot / new_net_monitor / new_flyout 构造函数,
//! 以及 detect_privilege。net 走 `nettop` 常驻子进程(见 net.rs 顶部对 ntstat 直连的否决说明)。

mod net;
mod proc;
mod tcptable;

use crate::flyout::FlyoutView;
use crate::model::Privilege;
use crate::platform::{NetMonitor, ProcScanner, TcpSnapshot};
use tao::event_loop::EventLoopWindowTarget;
use tao::window::{Window, WindowBuilder, WindowId};

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

/// macOS 本期 flyout:**no-op 占位**(UI 留下一阶段)。
///
/// `tray.rs` 需要一个真实 `WindowId` 来匹配窗口事件,故建一个**隐藏**的 tao 窗口占位;
/// 不挂 webview、不显示。三态本期靠托盘图标/tooltip + 日志验证。
pub struct MacFlyout {
    // 隐藏占位窗口:仅为提供 window_id 给 tray 事件匹配;Drop 时随之销毁。
    _window: Window,
    window_id: WindowId,
}

impl MacFlyout {
    pub fn new<T>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("seecn-flyout")
            .with_visible(false)
            .build(target)
            .map_err(|e| anyhow::anyhow!("创建 flyout 占位窗口失败: {e}"))?;
        let window_id = window.id();
        Ok(Self {
            _window: window,
            window_id,
        })
    }

    /// 供 tray.rs 匹配窗口事件用(与 WinFlyout 同名固有方法)。
    pub fn window_id(&self) -> WindowId {
        self.window_id
    }
}

impl FlyoutView for MacFlyout {
    fn show(&mut self, _json: &str) {}
    fn hide(&mut self) {}
    fn update(&mut self, _json: &str) {}
    fn resize_for(&mut self, _session_count: usize) {}
    fn is_visible(&self) -> bool {
        false
    }
    fn poll_autohide(&mut self) {}
}

/// 在给定 event loop target 上构造 macOS flyout(本期 no-op 占位)。
pub fn new_flyout<T: 'static>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<MacFlyout> {
    MacFlyout::new(target)
}
