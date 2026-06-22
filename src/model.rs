//! 核心数据模型(接口契约,锁定)。
//!
//! 本文件中的所有类型与常量是各模块之间的接口契约,实现者只填函数体,
//! 不得擅自修改这些已锁定的签名(参见 DESIGN §7)。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

// —— 常量 ——
/// 距上次收发数据多久内视为 Active。
pub const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);
/// 进程扫描间隔。
pub const PROC_SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// 状态评估 / 托盘刷新节拍。
pub const EVAL_INTERVAL: Duration = Duration::from_millis(500);
/// 连接在无事件且进程消失后保留多久再清理。
pub const CONN_GC_TTL: Duration = Duration::from_secs(30);

// —— v2:下行流识别(L7 语义逼近)相关常量(DESIGN §14.2)——
//
// 阈值是逼近的关键,初值保守,**待 e2e 校准**:用 `RUST_LOG=seecn=debug` 观察空闲 keepalive
// 与 SSE 流的实际下行速率量级,再据实调(keepalive 平均通常 < 数十 B/s,SSE 流一般
// 数百 B/s ~ 数 KB/s,阈值取其间)。

/// 下行速率滑动窗口时长(决定 SSE 流识别的平滑度)。窗口桶数 = RATE_WINDOW / EVAL_INTERVAL。
pub const RATE_WINDOW: Duration = Duration::from_secs(2);
/// 下行平均速率 ≥ 此阈值(B/s)→ 判定「正在流式接收」(SSE)。**待 e2e 校准**。
///
/// 字节口径平台相关,故阈值分平台:Windows ETW 报 TCP payload(keepalive 仅数十 B/s,256 够);
/// macOS nettop 含协议开销,keepalive 基线**实测 ~850~1300 B/s**,256 会被冲穿、空闲误判 Active,
/// 故用更高阈值(2048 为占位,待真实 SSE 活跃样本 e2e 校准)。
#[cfg(not(feature = "macos-platform"))]
pub const DOWN_RATE_ACTIVE_THRESHOLD: f64 = 256.0;
#[cfg(feature = "macos-platform")]
pub const DOWN_RATE_ACTIVE_THRESHOLD: f64 = 2048.0;
/// 单个评估周期上行字节 ≥ 此阈值 → 判定「刚发出请求」(请求体突发)。**待 e2e 校准**。
pub const REQUEST_BURST_MIN: u64 = 1024;

/// 三态链路状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Offline,
    Idle,
    Active,
}

/// 一条 TCP 连接的唯一键(四元组)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnKey {
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

/// 网络事件:由 ETW 回调或 TCP 快照产生,汇入 Engine。
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// 新连接建立(ETW Connect 或快照补发)。
    Connect { pid: u32, key: ConnKey },
    /// 连接断开。
    Disconnect { pid: u32, key: ConnKey },
    /// 一次数据收发(增量字节)。inbound/outbound 至少一个 > 0。
    Data {
        pid: u32,
        key: ConnKey,
        inbound: u64,
        outbound: u64,
    },
}

/// 单条连接的运行时状态(Engine 内部维护)。
#[derive(Debug, Clone)]
pub struct ConnState {
    pub pid: u32,
    // `key` 与所在 `HashMap<ConnKey, ConnState>` 的键冗余,但属 DESIGN §7 锁定契约字段
    // (便于调试与未来按连接维度展示),当前读路径只用 map 的键,故显式允许 dead_code。
    #[allow(dead_code)]
    pub key: ConnKey,
    pub bytes_in: u64,  // 累计
    pub bytes_out: u64, // 累计
    pub last_activity: Instant,
    pub alive: bool,        // Disconnect 后置 false,等 GC
    pub last_seen: Instant, // 任意事件刷新,用于 GC
}

/// 进程发现结果。
#[derive(Debug, Clone)]
pub struct ClaudeProc {
    pub pid: u32,
    pub cmdline: String, // 用于 tooltip 展示与调试
}

/// 一个 Claude session 的对外快照(评估产物,喂给托盘)。
#[derive(Debug, Clone)]
pub struct Session {
    pub pid: u32,
    // `cmdline` 属 DESIGN §7 锁定契约字段(用于 tooltip 展示与调试)。Windows tooltip 仅
    // ~127 字符,放整条命令行不现实,故 UI 当前不渲染它,仅保留供日志/未来用,显式允许 dead_code。
    #[allow(dead_code)]
    pub cmdline: String,
    pub state: LinkState,
    pub conn_count: usize,
    pub rate_in: u64,  // bytes/s(最近一个评估周期)
    pub rate_out: u64, // bytes/s
}

/// 进程是否以管理员运行(影响是否启用 ETW)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    Elevated, // 管理员,ETW 可用 → 三态
    Standard, // 普通用户,ETW 不可用 → 退化为 Offline/Idle 两态
}

/// 托盘图标的屏幕矩形(物理像素),供 flyout 锚定到图标附近。
///
/// 由 tray 的点击事件(`tray_icon::Rect`)填充,平台无关地经 `FlyoutView::set_anchor`
/// 传给 flyout。用固定屏幕角落的平台(如 Windows 右下角)可忽略它。
#[derive(Debug, Clone, Copy)]
pub struct TrayAnchor {
    /// 图标矩形左上角 x(物理像素,屏幕坐标)。
    pub x: f64,
    /// 图标矩形左上角 y。
    pub y: f64,
    /// 图标宽。
    pub width: f64,
    /// 图标高。
    pub height: f64,
}
