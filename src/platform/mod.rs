//! 平台抽象(trait 契约,锁定)。
//!
//! 通过 cfg 分发导出当前平台的 ProcScanner/NetMonitor/TcpSnapshot 构造函数。
//! 参见 DESIGN §8。

use crate::model::{ClaudeProc, ConnKey, NetEvent};
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// 进程扫描器:发现当前所有 Claude Code CLI 进程。
pub trait ProcScanner: Send {
    fn scan(&mut self) -> Vec<ClaudeProc>;
}

/// TCP 连接快照:给定关心的 PID 集合,返回它们当前已存在的连接。
pub trait TcpSnapshot: Send {
    fn snapshot(&self, pids: &HashSet<u32>) -> Vec<(u32, ConnKey)>;
}

/// 实时网络监控:后台监听,经 tx 推送 NetEvent。
/// claude_pids 为共享过滤集合,实现应在回调里据此过滤,只投递 Claude 进程事件。
pub trait NetMonitor: Send {
    /// 启动监听(阻塞式实现应在内部另起线程或要求调用方在独立线程调用)。
    /// 返回 Err 表示无法启动(如非管理员、provider 不可用)。
    fn start(
        &mut self,
        claude_pids: Arc<RwLock<HashSet<u32>>>,
        tx: Sender<NetEvent>,
    ) -> anyhow::Result<()>;
}

// —— 平台构造入口(cfg 分发)——
#[cfg(feature = "windows-platform")]
mod windows;

#[cfg(feature = "windows-platform")]
pub use windows::{new_flyout, new_net_monitor, new_proc_scanner, new_tcp_snapshot};

#[cfg(all(feature = "macos-platform", not(feature = "windows-platform")))]
mod macos;

#[cfg(all(feature = "macos-platform", not(feature = "windows-platform")))]
pub use macos::{new_flyout, new_net_monitor, new_proc_scanner, new_tcp_snapshot};

/// 检测当前进程权限 / 三态可用性(各平台实现见 platform/<os>)。
pub fn current_privilege() -> crate::model::Privilege {
    #[cfg(feature = "windows-platform")]
    {
        windows::detect_privilege()
    }
    #[cfg(all(feature = "macos-platform", not(feature = "windows-platform")))]
    {
        macos::detect_privilege()
    }
    #[cfg(not(any(feature = "windows-platform", feature = "macos-platform")))]
    {
        crate::model::Privilege::Standard
    }
}

/// 当前平台实时网络监听机制的展示名(用于日志/提示)。
///
/// 复用层(main.rs)不应硬编码具体机制;由各平台在此注入,保持复用层平台无关。
/// Windows = `ETW KernelNetwork`;macOS = `nettop`。
pub fn monitor_label() -> &'static str {
    #[cfg(feature = "windows-platform")]
    {
        "ETW KernelNetwork"
    }
    #[cfg(all(feature = "macos-platform", not(feature = "windows-platform")))]
    {
        "nettop"
    }
    #[cfg(not(any(feature = "windows-platform", feature = "macos-platform")))]
    {
        "网络监听"
    }
}
