# seecn 实现设计文档（v1 / Windows）

> see-claude-network：一个被动的网络状态传感器。它不读取/不处理任何流量内容，
> 只回答「这台机器上的每个 Claude Code CLi 会话此刻和 Claude server 的连接处于什么状态」，
> 并以系统托盘图标 + tooltip 的方式呈现。

本文件是实现蓝图，是各模块之间的**接口契约**。所有跨模块的 `struct` / `enum` / `trait` /
公共函数签名以本文为准；实现者（workflow agent）只负责填充函数体，**不得擅自修改已锁定的签名**。
若发现签名确有问题，必须在产出里显式说明，不能静默改动导致其它模块编译失败。

---

## 1. 目标与状态定义

### 1.1 监控范围（已确认）
- **只监控 Claude Code CLI**：Windows 上通常是 `node.exe` 运行 npm 包（命令行含 `claude` 脚本路径，
  如 `...\@anthropic-ai\claude-code\cli.js`），新版可能是原生 `claude.exe`。
- 不监控 Claude Desktop、不监控浏览器 claude.ai。

### 1.2 精度（已确认）
- 要求**毫秒级实时**感知「是否正在收发数据」。→ 主信号来自 ETW 实时事件。

### 1.3 权限（已确认）
- 允许要求**管理员权限**运行（ETW Kernel-Network provider 需要）。
- 进程发现、TCP 快照不需要管理员；ETW 需要。无管理员时应优雅降级为「在线/离线」两态并提示。

### 1.4 三态状态机
每个 Claude 进程聚合其全部连接得到一个 `LinkState`：

| 状态 | 含义 | 判定 |
|---|---|---|
| `Offline` | 没有到 server 的活跃连接 | 该进程当前无任何 ESTABLISHED 外连 |
| `Idle` | 长连接在，但静默 | 有连接，但距上次 send/recv 超过 `ACTIVE_WINDOW` |
| `Active` | 正在收发数据 | 有连接，且 `now - last_activity < ACTIVE_WINDOW` |

常量（写入 `model.rs`）：
- `ACTIVE_WINDOW = Duration::from_millis(1500)`
- `PROC_SCAN_INTERVAL = Duration::from_secs(2)`
- `EVAL_INTERVAL = Duration::from_millis(500)`（状态评估 / 托盘刷新节拍）
- `CONN_GC_TTL = Duration::from_secs(30)`（连接在无事件且进程消失后保留多久再清理）

---

## 2. 总体架构与数据流

```
                       Arc<RwLock<HashSet<u32>>>  (claude_pids，ETW 回调用它过滤)
                              ▲                 │
   ProcScanner(sysinfo) ──────┘                 ▼ 过滤
   每 2s scan → Vec<ClaudeProc> ──┐      ETW KernelNetwork 回调
                                   │      Connect/Disconnect/Send/Recv
   TcpSnapshot(netstat2) 给新PID   │             │  NetEvent
   补已存在连接 → NetEvent::Connect │             ▼
                                   └──────►  EngineMsg channel (crossbeam, 单消费者)
                                                  │
                                          Engine 线程(单线程，无锁拥有 conns 表)
                                          apply_event / refresh_procs / evaluate
                                                  │ Vec<Session> → 聚合 LinkState + tooltip
                                                  ▼ EventLoopProxy.send_event
                                          主线程 tao EventLoop + tray-icon
                                          更新图标颜色 + tooltip + 右键菜单
```

要点：
- **ETW 监听全量 TCP 事件，但在回调里用共享的 `claude_pids` 集合过滤**，只把 Claude 进程的事件投递进 channel，控制流量。
- `conns` 连接表只被 Engine 线程独占访问，**无需加锁**；唯一跨线程共享的是 `claude_pids`（`Arc<RwLock<…>>`）。
- 所有事件源（ETW、进程扫描、定时 tick、托盘退出）统一汇入一个 `EngineMsg` channel，Engine 单线程串行消费，逻辑简单无数据竞争（Rob Pike：用对数据结构，算法自然简单）。

---

## 3. 线程模型

| 线程 | 职责 | 关键点 |
|---|---|---|
| 主线程 | tao `EventLoop` 跑 tray-icon，处理 `UserEvent::TrayUpdate` / 菜单事件 | event loop 必须在主线程 |
| Engine | 消费 `EngineMsg`，维护 `conns`，定时 `evaluate`，经 proxy 推送托盘更新 | `recv_timeout(EVAL_INTERVAL)` 兼当 tick，超时即评估，免单独 tick 线程 |
| ETW | `ferrisetw` trace `start_and_process()`（阻塞），回调过滤后发 `NetEvent` | 需管理员；失败则该线程退出并通过 channel 报告降级 |
| ProcScan | 每 `PROC_SCAN_INTERVAL` 用 sysinfo 扫描，发 `Vec<ClaudeProc>`；对新 PID 调 TcpSnapshot 补连接 | 普通权限 |

退出：托盘菜单 Quit → 主线程置停止标志并 `ControlFlow::Exit`；其余线程为 daemon 线程随进程退出（ETW trace 句柄 drop 时 stop）。

---

## 4. 项目结构

单 crate（不是 workspace，保持简单），模块化 + feature 区分平台。

```
seecn/
├── Cargo.toml
├── justfile
├── README.md
├── docs/
│   └── DESIGN.md            # 本文件
└── src/
    ├── main.rs              # 入口：初始化、起线程、跑 tao event loop + tray
    ├── model.rs             # 接口契约：所有共享类型 + 常量
    ├── state.rs             # 状态机：ConnTable → Vec<Session> 聚合逻辑（平台无关）
    ├── monitor.rs           # Engine：消费 EngineMsg、协调、调用 state
    ├── tray.rs              # 托盘 UI：图标生成、tooltip、菜单（基于 tao + tray-icon）
    └── platform/
        ├── mod.rs           # cfg 分发：导出当前平台的 ProcScanner/NetMonitor/TcpSnapshot 构造函数
        └── windows/
            ├── mod.rs       # 汇总导出
            ├── proc.rs      # WinProcScanner: sysinfo 进程发现 + 命令行匹配
            ├── tcptable.rs  # WinTcpSnapshot: netstat2 取 TCP 表快照
            └── etw.rs       # WinNetMonitor: ferrisetw 监听 KernelNetwork
```

macOS（未来）：`platform/macos/`，由 `macos-platform` feature 开启，本期只留 `platform/mod.rs` 里的 cfg 分支占位与 `// TODO`。

---

## 5. Cargo.toml

```toml
[package]
name = "seecn"
version = "0.1.0"
edition = "2021"
description = "see-claude-network: passive network status sensor for Claude Code CLI sessions"

[features]
default = ["windows-platform"]
# Windows 平台实现所需的平台专属依赖
windows-platform = ["dep:ferrisetw", "dep:netstat2"]
# macOS 占位，本期不实现
macos-platform = []

[dependencies]
# 跨平台
tray-icon = "0.21"            # 系统托盘（agent 按实际可用最新 0.2x 版本锁定）
tao = "0.35"                  # 事件循环（与 tray-icon 配套，agent 确认兼容版本）
sysinfo = "0.37"             # 进程发现（pid / name / exe / cmd）
crossbeam-channel = "0.5"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Windows 平台专属（仅 windows-platform feature）
ferrisetw = { version = "1", optional = true }   # ETW 用户态 trace
netstat2 = { version = "0.11", optional = true } # TCP 表快照（内部 GetExtendedTcpTable）

[target.'cfg(windows)'.dependencies]
windows-sys = { version = "0.60", features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_System_Threading",
] }   # 仅用于「是否管理员」检测，无第三方依赖也可

[profile.release]
opt-level = "z"
lto = true
strip = true
```

> 版本号是参考值。**agent 在 Phase1 必须用 `cargo add` 让 cargo 解析出实际可用版本**，
> 不要硬写一个不存在的版本号导致解析失败。tray-icon 与 tao 的版本要互相兼容
> （tray-icon 的 README/示例用哪个 tao 版本就跟随）。

---

## 6. justfile

```just
set shell := ["pwsh", "-NoLogo", "-NoProfile", "-Command"]

# 列出所有任务
default:
    @just --list

# 编译检查
check:
    cargo check

build:
    cargo build

release:
    cargo build --release

# 直接运行（非管理员，ETW 会降级为两态）
run:
    cargo run

# 以管理员身份运行（ETW 三态需要）——会弹 UAC
run-admin:
    Start-Process -Verb RunAs -FilePath pwsh -ArgumentList '-NoExit','-NoProfile','-Command',"cd '{{justfile_directory()}}'; cargo run"

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets -- -D warnings

# 全量本地校验
ci: fmt check lint
    cargo build
```

---

## 7. 核心数据模型 `src/model.rs`（接口契约，锁定）

```rust
use std::net::SocketAddr;
use std::time::{Duration, Instant};

// —— 常量 ——
pub const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);
pub const PROC_SCAN_INTERVAL: Duration = Duration::from_secs(2);
pub const EVAL_INTERVAL: Duration = Duration::from_millis(500);
pub const CONN_GC_TTL: Duration = Duration::from_secs(30);

/// 三态链路状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Offline,
    Idle,
    Active,
}

/// 一条 TCP 连接的唯一键（四元组）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnKey {
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

/// 网络事件：由 ETW 回调或 TCP 快照产生，汇入 Engine
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// 新连接建立（ETW Connect 或快照补发）
    Connect { pid: u32, key: ConnKey },
    /// 连接断开
    Disconnect { pid: u32, key: ConnKey },
    /// 一次数据收发（增量字节）。inbound/outbound 至少一个 > 0
    Data { pid: u32, key: ConnKey, inbound: u64, outbound: u64 },
}

/// 单条连接的运行时状态（Engine 内部维护）
#[derive(Debug, Clone)]
pub struct ConnState {
    pub pid: u32,
    pub key: ConnKey,
    pub bytes_in: u64,        // 累计
    pub bytes_out: u64,       // 累计
    pub last_activity: Instant,
    pub alive: bool,          // Disconnect 后置 false，等 GC
    pub last_seen: Instant,   // 任意事件刷新，用于 GC
}

/// 进程发现结果
#[derive(Debug, Clone)]
pub struct ClaudeProc {
    pub pid: u32,
    pub cmdline: String,      // 用于 tooltip 展示与调试
}

/// 一个 Claude session 的对外快照（评估产物，喂给托盘）
#[derive(Debug, Clone)]
pub struct Session {
    pub pid: u32,
    pub cmdline: String,
    pub state: LinkState,
    pub conn_count: usize,
    pub rate_in: u64,         // bytes/s（最近一个评估周期）
    pub rate_out: u64,        // bytes/s
}

/// 进程是否以管理员运行（影响是否启用 ETW）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    Elevated,    // 管理员，ETW 可用 → 三态
    Standard,    // 普通用户，ETW 不可用 → 退化为 Offline/Idle 两态
}
```

> `Session` 的整体（机器级）聚合状态由 `state.rs` 提供函数计算（见 §10），不放在 model。

---

## 8. 平台抽象 `src/platform/mod.rs`（trait 契约，锁定）

```rust
use crossbeam_channel::Sender;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use crate::model::{ClaudeProc, ConnKey, NetEvent};

/// 进程扫描器：发现当前所有 Claude Code CLI 进程
pub trait ProcScanner: Send {
    fn scan(&mut self) -> Vec<ClaudeProc>;
}

/// TCP 连接快照：给定关心的 PID 集合，返回它们当前已存在的连接
pub trait TcpSnapshot: Send {
    fn snapshot(&self, pids: &HashSet<u32>) -> Vec<(u32, ConnKey)>;
}

/// 实时网络监控：后台监听，经 tx 推送 NetEvent。
/// claude_pids 为共享过滤集合，实现应在回调里据此过滤，只投递 Claude 进程事件。
pub trait NetMonitor: Send {
    /// 启动监听（阻塞式实现应在内部另起线程或要求调用方在独立线程调用）。
    /// 返回 Err 表示无法启动（如非管理员、provider 不可用）。
    fn start(
        &mut self,
        claude_pids: Arc<RwLock<HashSet<u32>>>,
        tx: Sender<NetEvent>,
    ) -> anyhow::Result<()>;
}

// —— 平台构造入口（cfg 分发）——
#[cfg(feature = "windows-platform")]
mod windows;

#[cfg(feature = "windows-platform")]
pub use windows::{new_net_monitor, new_proc_scanner, new_tcp_snapshot};

#[cfg(all(feature = "macos-platform", not(feature = "windows-platform")))]
compile_error!("macOS platform not implemented yet");

/// 检测当前进程是否管理员（Windows 实现见 platform/windows）
pub fn current_privilege() -> crate::model::Privilege {
    #[cfg(feature = "windows-platform")]
    { windows::detect_privilege() }
    #[cfg(not(feature = "windows-platform"))]
    { crate::model::Privilege::Standard }
}
```

构造函数签名（windows/mod.rs 导出，锁定）：

```rust
pub fn new_proc_scanner() -> Box<dyn ProcScanner>;
pub fn new_tcp_snapshot() -> Box<dyn TcpSnapshot>;
pub fn new_net_monitor() -> Box<dyn NetMonitor>;
pub fn detect_privilege() -> crate::model::Privilege;
```

---

## 9. 模块详细设计

### 9.1 `platform/windows/proc.rs` — `WinProcScanner`
- 依赖 `sysinfo`。持有一个复用的 `sysinfo::System` 以减少分配。
- `scan()`：
  1. `sys.refresh_processes(ProcessesToUpdate::All, true)`（API 以实际 sysinfo 版本为准）。
  2. 遍历 `sys.processes()`，对每个进程取 `name()`、`exe()`、`cmd()`。
  3. 命中规则（大小写不敏感，匹配任一）：
     - 进程名为 `claude.exe`；或
     - `cmd()` 任意一段路径包含子串 `claude`（覆盖 `node cli.js` 形态，典型为含 `claude-code` 或 `\claude\`）。
  4. 排除自身进程（`std::process::id()`）与明显误伤（可选：要求 cmd 同时包含 `node`/`claude.exe` 才算，
     避免把本项目或编辑器里打开的 claude 路径误判——实现者自行加一个保守过滤，宁缺毋滥，并把匹配子串集中为常量便于调整）。
  5. 返回 `Vec<ClaudeProc>{ pid, cmdline=cmd().join(" ") }`。
- 匹配关键词集中成 `const CLAUDE_MARKERS: &[&str] = &["claude-code", "claude.exe", r"\claude\"];` 之类，便于后续调参。

### 9.2 `platform/windows/tcptable.rs` — `WinTcpSnapshot`
- 依赖 `netstat2`。
- `snapshot(pids)`：
  1. `get_sockets_info(AddressFamilyFlags::IPV4 | IPV6, ProtocolFlags::TCP)`。
  2. 对每个 `SocketInfo`：取 `associated_pids`，与 `pids` 求交；只保留 `state == Established` 的 TCP socket。
  3. 组装 `ConnKey{ local: SocketAddr(local_addr, local_port), remote: SocketAddr(remote_addr, remote_port) }`，
     返回 `Vec<(pid, ConnKey)>`。
- 仅用于「补已存在连接」，所以只关心 Established。

### 9.3 `platform/windows/etw.rs` — `WinNetMonitor`（核心）
- 依赖 `ferrisetw`。需要管理员。
- Provider：`Microsoft-Windows-Kernel-Network`，GUID `7DD42A49-5329-4832-8DFD-43D979153A88`。
- `start(claude_pids, tx)`：
  1. 另起一个线程跑 `UserTrace::new().named("seecn-net").enable(provider).start_and_process()`（阻塞）。
     - `start()` 失败（权限不足等）→ 返回 `Err`，让上层降级。建议先在调用线程尝试创建 trace，
       成功后再把 `process` 循环移入后台线程；具体以 ferrisetw 当前 API 为准（实现者查 docs.rs/ferrisetw 确认）。
  2. provider 的回调 `move |record, schema_locator|`：
     - 由 `schema_locator.event_schema(record)` 得到 schema，用 `Parser` 取字段。
     - 取 `pid`（事件自带 `record.process_id()` 或字段 `PID`）。**先用 `claude_pids.read()` 判断 pid 是否关心，不关心直接 return**（热路径，尽量轻）。
     - 按 `record.event_id()` 分派（TcpIp 经典事件 ID，IPv4 / IPv6 都要处理）：

       | EventId | 含义 | 映射 |
       |---|---|---|
       | 10 / 26 | TcpIp Send (v4/v6) | `NetEvent::Data{ outbound = size }` |
       | 11 / 27 | TcpIp Recv (v4/v6) | `NetEvent::Data{ inbound = size }` |
       | 12 / 28 | TcpIp Connect (v4/v6) | `NetEvent::Connect` |
       | 13 / 29 | TcpIp Disconnect (v4/v6) | `NetEvent::Disconnect` |
       | 14 / 30 | TcpIp Retransmit | 可并入 Data（outbound=size）或忽略 |
       | 15 / 31 | TcpIp Accept | 可当 Connect（入站，Claude 一般无）|

       > 事件 ID 是 TcpIp 经典 manifest 的常见取值，**实现者必须用 schema 字段名解析数据并核对 ID**
       > （字段常见为 `PID`/`size`/`daddr`/`saddr`/`dport`/`sport`/`connid`）。若某 ID 解析不到预期字段，
       > 以 schema 实际为准，宁可少映射也不要 panic。
     - 组装 `ConnKey`（saddr:sport = local，daddr:dport = remote；Send/Recv 方向以本机视角统一）。
       Send/Recv 事件用于 `Data`，字节数取 `size` 字段。
     - `tx.send(event)`；send 失败（接收端关闭）说明要退出，回调可忽略错误。
  3. 回调里**禁止阻塞 / 禁止 panic**：所有解析失败都 early-return。
- 健壮性：地址解析不到时跳过该事件；只处理 TCP（provider 已是 Kernel-Network，过滤 TcpIp 任务即可）。

> 若 ferrisetw 的事件粒度无法直接拿到「连接四元组 + 字节」，退一步：用 Send/Recv 的 `size` 累计判断 Active，
> 用 Connect/Disconnect 维护连接存在性；ConnKey 即使只用 remote 端也可（Engine 用 (pid,key) 索引）。
> 保证「能判断三态」是硬目标，连接键的精确度是次要目标。

### 9.4 `state.rs` — 状态机（平台无关，纯函数，**最易单测**）
```rust
use crate::model::*;
use std::collections::HashMap;

/// 由连接表派生出每个 Claude 进程的 Session（含三态与速率）。
/// `procs`: 当前存活的 pid -> cmdline。
/// `now`: 评估时刻（注入便于测试）。
/// `dt`: 距上次评估的间隔，用于算速率（首次可传 EVAL_INTERVAL）。
/// 返回值按 pid 排序，稳定输出。
pub fn evaluate(
    conns: &HashMap<ConnKey, ConnState>,
    procs: &HashMap<u32, String>,
    now: Instant,
    dt: Duration,
    // 速率需要上次累计值；实现可在 Engine 侧维护，或本函数接收 prev 累计表。
    prev_totals: &HashMap<u32, (u64, u64)>,
) -> (Vec<Session>, HashMap<u32, (u64, u64)>);

/// 机器级整体状态：任一 Active → Active；否则任一 Idle → Idle；否则 Offline。
pub fn overall(sessions: &[Session]) -> LinkState;
```
- 单进程状态判定：取该 pid 所有 `alive` 连接；无 → `Offline`；
  有且 `now - max(last_activity) < ACTIVE_WINDOW` → `Active`；否则 `Idle`。
- 速率：`rate = (now_total - prev_total) as f64 / dt.as_secs_f64()`，返回新的累计表供下次用。
- **必须配最少 2~3 个单元测试**（Offline/Idle/Active 各一），这是全项目最适合测试的部分。

### 9.5 `monitor.rs` — `Engine`
```rust
pub enum EngineMsg {
    Net(NetEvent),
    Procs(Vec<ClaudeProc>),
    Quit,
}

pub struct Engine {
    conns: HashMap<ConnKey, ConnState>,
    procs: HashMap<u32, String>,                 // 当前存活 Claude 进程
    claude_pids: Arc<RwLock<HashSet<u32>>>,      // 与 ETW 共享
    prev_totals: HashMap<u32, (u64, u64)>,
    last_eval: Instant,
    snapshot: Box<dyn TcpSnapshot>,              // 给新 pid 补连接
}

impl Engine {
    pub fn new(claude_pids: Arc<RwLock<HashSet<u32>>>, snapshot: Box<dyn TcpSnapshot>) -> Self;

    fn apply_net(&mut self, ev: NetEvent);        // 更新 conns（Connect 插入/Data 累加+刷新 last_activity/Disconnect 置 alive=false）
    fn refresh_procs(&mut self, list: Vec<ClaudeProc>); // 更新 procs + 写回 claude_pids；对新增 pid 调 snapshot 补 Connect
    fn gc(&mut self, now: Instant);               // 清理 dead 进程的连接、超 CONN_GC_TTL 的 !alive 连接
    pub fn evaluate(&mut self, now: Instant) -> (Vec<Session>, LinkState); // 调 state::evaluate + overall

    /// 主循环：在独立线程调用。rx 收 EngineMsg，recv_timeout(EVAL_INTERVAL) 超时即评估并 push 托盘。
    pub fn run(self, rx: Receiver<EngineMsg>, on_update: impl FnMut(Vec<Session>, LinkState));
}
```
- `run`：循环 `rx.recv_timeout(EVAL_INTERVAL)`：
  - 收到 `Net`/`Procs` → 调对应 apply，并不立即评估（除非想更实时）。
  - 超时 / 到达评估节拍 → `gc` + `evaluate` → 调 `on_update(sessions, overall)`。
  - 收到 `Quit` → break。
- `on_update` 回调由 main 提供，内部用 `EventLoopProxy.send_event` 投递到托盘。

### 9.6 `tray.rs` — 托盘 UI
- 基于 `tao`（event loop）+ `tray-icon`。
- **图标用代码生成纯色 RGBA**（避免打包 .ico 资源）：写一个 `fn icon_for(state: LinkState) -> tray_icon::Icon`，
  16x16 或 32x32，按状态填色：
  - `Offline` → 灰 `#7A7A7A`
  - `Idle` → 蓝 `#3B82F6`
  - `Active` → 绿/橙 `#22C55E`（活跃）
  画一个实心圆点（中心实心、边缘透明）即可，简单美观。
- 公共类型：
  ```rust
  /// 主线程通过 EventLoop 的 UserEvent 接收的更新
  pub enum UserEvent {
      TrayUpdate { sessions: Vec<Session>, overall: LinkState },
      MenuQuit,
  }

  /// 构建托盘并运行 event loop（阻塞，直到退出）。
  /// `spawn_engine`: 在 event loop 启动后、拿到 proxy 后调用，用于把 proxy 交给 Engine 侧起线程。
  pub fn run_tray(privilege: Privilege, spawn_engine: impl FnOnce(EventLoopProxy<UserEvent>) + Send + 'static) -> !;
  ```
- `run_tray`：
  1. 建 `EventLoopBuilder::<UserEvent>::with_user_event().build()`。
  2. 建初始 `TrayIcon`（图标 Offline，tooltip "seecn: starting…"），右键菜单含 `Quit`（用 `tray-icon::menu`）。
     若 `privilege == Standard`，tooltip 追加 "(no admin: 2-state mode)"。
  3. `let proxy = event_loop.create_proxy(); spawn_engine(proxy);`
  4. `event_loop.run`：
     - `Event::UserEvent(TrayUpdate{sessions, overall})` → `tray.set_icon(icon_for(overall))` + `tray.set_tooltip(render_tooltip(&sessions))`。
     - 菜单事件（tray-icon 的 `MenuEvent::receiver()`）匹配 Quit → `*control_flow = Exit`。
  5. `render_tooltip`：多行，如
     ```
     seecn — 2 sessions
     ● Active  pid 12345  ↓3.2KB/s ↑1.1KB/s
     ● Idle    pid 23456
     ```
     （Windows tooltip 有长度上限 ~127 字符，超出则汇总为「N active / M idle / K offline」。）

### 9.7 `main.rs` — wiring
```rust
mod model; mod state; mod monitor; mod tray; mod platform;
```
流程：
1. 初始化 `tracing_subscriber`（env-filter，默认 info）。
2. `let priv = platform::current_privilege();` 打印日志：管理员 → 三态；否则警告两态。
3. 创建：
   - `claude_pids = Arc::new(RwLock::new(HashSet::new()))`
   - `(net_tx, net_rx)`、`(eng_tx, eng_rx)` 两组 crossbeam channel；
     约定：ETW/快照产 `NetEvent` 进 `net_tx`；另起一根「桥接」把 `net_rx` 的 `NetEvent` 包成 `EngineMsg::Net` 转投 `eng_tx`（或直接让 ETW 发 `EngineMsg`，二选一，保持 Engine 单入口）。
4. `tray::run_tray(priv, move |proxy| { … spawn 线程 … })`：
   在 `spawn_engine` 闭包里（此时 event loop 已就绪）启动后台线程：
   - **ProcScan 线程**：循环 `scanner.scan()` → 发 `EngineMsg::Procs`；sleep `PROC_SCAN_INTERVAL`。
   - **ETW 线程**：`net_monitor.start(claude_pids.clone(), net_tx)`；失败则 log 警告（两态模式仍可用 snapshot/proc 走 Idle/Offline）。
   - **Engine 线程**：`engine.run(eng_rx, move |sessions, overall| { proxy.send_event(UserEvent::TrayUpdate{…}).ok(); })`。
5. `run_tray` 永不返回（`-> !`），进程随 event loop 退出。

> wiring 的 channel 取舍由实现者定，但**必须保证 Engine 是单一消费者、conns 表无跨线程共享**。
> 若觉得「桥接线程」多余，可让 ETW 回调与 ProcScan 都直接持有 `eng_tx` 发 `EngineMsg`（推荐，最简单）。

---

## 10. 状态机精确定义（实现 + 测试基准）

```
单进程 pid 的状态：
  conns_of_pid = { c in conns | c.pid == pid && c.alive }
  if conns_of_pid 为空:            Offline
  else if now - max(c.last_activity) < ACTIVE_WINDOW:  Active
  else:                            Idle

机器整体 overall(sessions):
  if 任一 session.state == Active:  Active
  else if 任一 == Idle:             Idle
  else:                             Offline   // 含没有任何 session 的情况

速率（每个评估周期）：
  rate_in  = (Σ bytes_in  of pid - prev_in)  / dt_secs
  rate_out = (Σ bytes_out of pid - prev_out) / dt_secs
```
单测用例（`state.rs` 内 `#[cfg(test)]`）：
1. 无连接 → Offline。
2. 有连接、`last_activity` 在窗口内 → Active，速率 > 0。
3. 有连接、`last_activity` 早于窗口 → Idle，速率 0。

---

## 11. 风险与实现注意

1. **ETW provider 唯一性**：`Microsoft-Windows-Kernel-Network` 是 manifest provider，可被普通 user-mode trace session 订阅，不与「NT Kernel Logger」冲突，可多开。仍需管理员。
2. **不强校验远程 IP**：以「Claude PID + ESTABLISHED 外连」为判定依据，不去猜 Anthropic 的 Cloudflare IP（避免 IP 漂移导致误判）。如需加强，可选地把 remote_port==443 作为软过滤，但不作为硬条件。
3. **回调热路径**：ETW 回调里先按 `claude_pids` 过滤再做任何重活；解析失败一律 early-return，绝不 panic（panic 跨 FFI 边界是 UB）。
4. **进程匹配宁缺毋滥**：`claude` 子串匹配可能误伤；保守加「node 或 claude.exe」前置条件，并记 debug 日志列出命中进程，方便实测校准。
5. **权限降级**：非管理员时 ETW 起不来，仅靠 proc + snapshot 可给出 Offline/Idle（无法判断 Active，因为快照看不到实时字节）。tooltip 标注两态模式。
6. **不持久化、不联网上报、不读取流量内容**：seecn 只读本机连接元数据，符合「纯状态传感器」定位。
7. **跨线程类型**：`NetEvent` / `EngineMsg` 派生 `Clone`，且只含 `Copy`/`String`/`u64`，可安全跨 channel。`Instant` 不进 channel（在 Engine 侧用 `Instant::now()` 标记），避免时钟语义混乱。

---

## 12. 验收标准（Review / E2E 依据）

- [ ] `cargo build`（默认 `windows-platform` feature）零错误；`cargo clippy -- -D warnings` 通过。
- [ ] `state.rs` 单测通过（`cargo test`）。
- [ ] 以管理员运行后出现托盘图标；打开一个 Claude Code CLI 会话并发消息时，图标变 Active（绿），消息收发结束回落 Idle（蓝），关闭会话回 Offline（灰）。
- [ ] tooltip 正确列出 session 数量与每个 pid 的状态/速率。
- [ ] 右键菜单 Quit 能干净退出。
- [ ] 非管理员运行时不崩溃，进入两态模式并在 tooltip/日志提示。
- [ ] 代码无 `unwrap()` 在热路径 / FFI 回调中导致 panic 的风险；`unsafe` 仅限必要（管理员检测）。

---

## 13. 给实现 workflow 的分工建议

- **Phase 1（骨架，单 agent，串行）**：建目录、`cargo init`、按 §5 用 `cargo add` 落实依赖、写 `model.rs` 全部类型与常量、`platform/mod.rs` 与 `windows/mod.rs` 的 trait 与构造函数签名、各模块文件用 `todo!()`/最小 stub 占位，使 `cargo check`（windows-platform）**通过**。锁定全部签名。
- **Phase 2（并行填充 body）**：每个 agent 认领一个文件，**只填函数体、不改签名**：`proc.rs`、`tcptable.rs`、`etw.rs`、`state.rs`(+测试)、`monitor.rs`、`tray.rs`、`main.rs` wiring。各自 `cargo check` 自己能编过的部分。
- **Phase 3（集成修复，单 agent，串行）**：`cargo build` → 逐个修编译错误 → `cargo clippy` → `cargo test` → 写 `README.md`。迭代直到 §12 的编译类验收项全绿。运行类验收项（托盘/实机）留给用户 E2E。

---

## 14. 三态精度增强 v2:L7 语义逼近(下行流识别)

> 背景:ETW 拿到的是 **L4(TCP)** 的收发字节,而三态想表达的是 **L7** 语义——「此刻有没有一次进行中的请求 / SSE 流式响应」。TLS 加密让我们无法直接读 L7,只能用 L4 的**可见特征**(速率 / 方向 / 持续性)去逼近。v1 的「窗口内有任何 Data 事件即 Active」会把 keepalive / HTTP2 PING 等保活流量误判成 Active。本节用 **L1(阈值过滤)+L2(下行速率窗口 + 上行突发)** 修正,**不引入新依赖、不解密、不破坏纯传感器定位**。

### 14.1 Claude 的 L7 行为在 L4 上的指纹

一次 agent turn 的 L4 波形:
1. **上行突发**:请求体(prompt+历史,通常 ≥ 数 KB)一次性发出。
2. **下行持续流**:SSE 响应 token-by-token,下行小包**持续**到达数秒~数十秒,下行字节稳定增长。
3. **归于平静**:仅剩 keepalive(小、低频、双向)。

判别核心:**「下行是否在持续流动」= Active 的最强 L4 代理信号**;keepalive 是「小且周期」,SSE 是「持续且高频」,在**速率维度**上可分。上行突发用来覆盖「请求已发出、首 token 未到」的空档。

### 14.2 新增常量(`model.rs`)

```rust
/// 下行速率滑动窗口时长(决定 SSE 流识别的平滑度)。窗口桶数 = RATE_WINDOW / EVAL_INTERVAL。
pub const RATE_WINDOW: Duration = Duration::from_secs(2);
/// 下行平均速率 ≥ 此阈值(B/s)→ 判定「正在流式接收」(SSE)。**待 e2e 校准**。
pub const DOWN_RATE_ACTIVE_THRESHOLD: f64 = 256.0;
/// 单个评估周期上行字节 ≥ 此阈值 → 判定「刚发出请求」(请求体突发)。**待 e2e 校准**。
pub const REQUEST_BURST_MIN: u64 = 1024;
```

> 阈值是逼近的关键,初值保守。e2e 时用 `RUST_LOG=seecn=debug` 观察空闲 keepalive 与 SSE 流的实际下行速率量级,再据实调(keepalive 平均通常 < 数十 B/s,SSE 流一般数百 B/s ~ 数 KB/s,阈值取其间)。

### 14.3 跨周期状态(`monitor.rs` 的 `Engine` 持有)

v1 的 `prev_totals: HashMap<u32,(u64,u64)>` 升级为 per-pid 流量状态:

```rust
/// 每个 Claude 进程的跨评估周期流量状态(Engine 独占,单线程无锁)。
struct PidFlow {
    prev_in: u64,                 // 上次评估时该 pid 全部连接的累计入字节
    prev_out: u64,                // 上次累计出字节
    down_buckets: VecDeque<u64>,  // 最近 N=RATE_WINDOW/EVAL_INTERVAL 个周期各自的下行 delta
    last_effective: Instant,      // 最后一次「有效活动」时刻(驱动 Active 窗口)
}
```

Engine 用 `HashMap<u32, PidFlow>` 替换 `prev_totals`;pid 不在 `procs` 时随 gc 一并移除。新 pid 首次出现时 `last_effective` 初始化为 `now - ACTIVE_WINDOW`(即默认非 Active,避免新进程一出现就误判 Active)。

### 14.4 判定算法(每次 `evaluate(now, dt)`,对每个存活 pid)

```
1. 聚合该 pid 全部连接 → (alive_conn_count, total_in, total_out)   // 同 v1
2. delta_in  = total_in.saturating_sub(flow.prev_in)
   delta_out = total_out.saturating_sub(flow.prev_out)
   flow.prev_in/out = total_in/out
3. flow.down_buckets.push_back(delta_in); 若超过 N 个则 pop_front
   down_rate = sum(down_buckets) / RATE_WINDOW.as_secs_f64()        // B/s,平均
4. effective = is_effective_activity(down_rate, delta_out)
              = down_rate >= DOWN_RATE_ACTIVE_THRESHOLD || delta_out >= REQUEST_BURST_MIN
   if effective { flow.last_effective = now }
5. state = classify(alive_conn_count, now - flow.last_effective)
          = if alive_conn_count == 0        → Offline
            else if (now - last_effective) < ACTIVE_WINDOW → Active
            else                            → Idle
6. 展示速率 rate_in/out:本周期瞬时 = delta / dt(沿用 v1 语义,仅用于 tooltip)
```

回落时序(自洽性核对):SSE 流停止后,`down_buckets` 在 `RATE_WINDOW`(2s)内逐渐清空 → `down_rate` 掉到阈值下 → `last_effective` 停止刷新 → 再过 `ACTIVE_WINDOW`(1.5s)回落 Idle。总回落延迟约 2~3.5s,既不抖动(抹平 token 间隙)又不长期粘滞。启动延迟由「上行突发」兜底:请求体一发出即 Active,无需等下行窗口积累。

### 14.5 `state.rs` 重构(判定纯函数化,便于单测)

把判定逻辑抽成纯函数留在 `state.rs`(Engine 调用它们;状态仍由 Engine 维护):

```rust
/// 本周期是否构成「有效活动」(下行流 或 上行请求突发)。
pub fn is_effective_activity(down_rate: f64, up_burst: u64) -> bool;
/// 由存活连接数 + 距上次有效活动的时长判三态。
pub fn classify(alive_conn_count: usize, since_effective: Duration) -> LinkState;
/// 机器级聚合(不变)。
pub fn overall(sessions: &[Session]) -> LinkState;
```

v1 的 `evaluate(...)` 大函数由 Engine 内部循环取代(因为需要跨周期状态 `PidFlow`,不再是纯函数)。**测试相应调整**,至少覆盖:
- `is_effective_activity`:keepalive 速率(如 20 B/s,无上行突发)→ false;SSE 速率(如 800 B/s)→ true;上行突发(delta_out=4096)→ true;两者都低 → false。
- `classify`:无连接→Offline;`since < ACTIVE_WINDOW`→Active;`since >= ACTIVE_WINDOW`→Idle。
- `overall`:优先级 Active>Idle>Offline + 空列表→Offline(沿用 v1 用例)。

### 14.6 不改动的部分

- **`etw.rs`**:保留原始字节计数(inbound=Recv、outbound=Send;Retransmit 归 outbound,合理——重传不应触发"下行流")。**不在 etw 做阈值过滤**:单包大小区分不了「持续小包流(SSE)」与「零星小包(keepalive)」,L1/L2 的阈值判定必须放在有 `dt` 的窗口聚合层(Engine),而非单事件层。
- **`tray.rs` / `main.rs` / `proc.rs` / `tcptable.rs`**:不变。tooltip 仍展示三态 + 瞬时速率。

### 14.7 验收增量

- [ ] `cargo build` / `clippy -D warnings` / `test` 仍全绿;新增/调整的 `state.rs` 单测通过。
- [ ] e2e:空闲(仅 keepalive)的 Claude 会话稳定显示 **Idle**(不再因保活跳 Active);发消息→请求突发即 **Active**→SSE 流式输出期间保持 **Active**→输出结束 2~3.5s 内回落 **Idle**。
- [ ] `RUST_LOG=seecn=debug` 能打印每 pid 的 `down_rate` / `delta_out` / 判定结果,便于校准阈值。

---

## 15. ETW session 生命周期与残留清理(bugfix)

### 15.1 问题与 root cause

**现象**:seecn 关闭后再次启动报错「session 已存在」(`StartTraceW` → `ERROR_ALREADY_EXISTS` 0xB7)。

**root cause**:ETW realtime trace session 是**系统级持久化**对象——一旦 `StartTraceW` 创建,它就活在内核里,**不随创建进程自动消亡**,必须显式 `ControlTrace(STOP)` 或由 `UserTrace` 的 `Drop` 去 stop。而 v1 的 `etw.rs::start` 把 `UserTrace` move 进了 daemon 后台线程(`seecn-etw`),进程退出时(尤其 tao 的 `ControlFlow::Exit` 会直接 `std::process::exit`,主线程与所有后台线程都**不 unwind、不跑 Drop**)该线程被强杀,`drop(trace)`(本应 stop session)从未执行 → `seecn-net` session 泄漏,下次同名启动即冲突。

这是 ETW 工具的通病,业界标准解法是**启动时先停同名残留再重建**(self-heal),而不是指望退出时回收。

### 15.2 修复 B(主方案,必做):启动自愈清理

在 `WinNetMonitor::start()` 的**最开头**,无条件调用 `stop_stale_trace(TRACE_NAME)`,停掉任何已存在的同名 session,再走原有 `UserTrace::...start()`。无论上次进程是正常退出、panic 还是被强杀,这次启动都先清后建,彻底自愈。

**依赖**:`Cargo.toml` 的 `windows-sys` 增加 feature `Win32_System_Diagnostics_Etw`(并保留 `Win32_Foundation`)。

**`stop_stale_trace` 实现要点**(unsafe,以 windows-sys 0.61 实际类型/常量为准,实现者查 docs.rs 核对):

```text
- ControlTraceW(STOP) 按 session 名停止,需要一块 EVENT_TRACE_PROPERTIES 缓冲,
  其后预留 LoggerName 与 LogFileName 两段 wide 缓冲(各留足够字节,如 ≥ (名字长+1)*2,
  简单起见各给固定 1024 字节)。布局:[EVENT_TRACE_PROPERTIES][LoggerName buf][LogFileName buf]。
- 初始化:Wnode.BufferSize = 整块字节数;Wnode.Flags = WNODE_FLAG_TRACED_GUID;
  LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>();
  LogFileNameOffset = LoggerNameOffset + LoggerName buf 大小。
- 调用:ControlTraceW(0 /*按名*/, name_wide.as_ptr() /*InstanceName*/, props, EVENT_TRACE_CONTROL_STOP)。
  (ControlTraceW 第一个 handle 参数类型在 windows-sys 0.61 可能是 0 或一个 CONTROLTRACE_HANDLE newtype,按实际写。)
- 返回码处理:ERROR_SUCCESS(0) = 已停止;ERROR_WMI_INSTANCE_NOT_FOUND(4201) = 本就不存在 → 忽略;
  其它错误仅 tracing::debug! 记录,**绝不阻断后续 start**(best-effort)。
- 全程禁止 panic;函数签名建议 `fn stop_stale_trace(name: &str)`,返回 () 即可。
```

**双保险(可选但推荐)**:若 `start()` 仍返回 `ERROR_ALREADY_EXISTS`,再 `stop_stale_trace` 一次后 retry 一次;仍失败才返回 Err 降级。

> `stop_stale_trace` 需要管理员权限(ControlTrace),但 `start()` 本就要管理员,前置条件一致;非管理员场景 `start()` 整体返回 Err 走两态降级,不受影响。

### 15.3 退出回收(A):为何暂不做

干净退出当然更好,但 tao 的 `EventLoop::run` 在 `ControlFlow::Exit` 时直接 `std::process::exit(0)`,**主线程也不 unwind**,无法靠 Drop 回收;要显式 stop 须把 ETW 的 stop 句柄经 `run_tray` 签名传到托盘 `Quit` 分支(或在 `Event::LoopDestroyed` 里处理),跨模块耦合上升。鉴于 **B 已让残留在下次启动被复用且不累积**(同名只占一个 logger slot,启动即复用),A 的边际收益小,本次不做,仅在 `etw.rs` 注释说明。若后续要做,推荐在 `tray.rs` 的 `Event::LoopDestroyed` 分支调用一个传入的 `on_shutdown` 清理回调。

### 15.4 验收增量

- [ ] 连续两次启动/关闭 seecn(管理员),第二次**不再**报 `ERROR_ALREADY_EXISTS`,能正常起 ETW(日志「ETW KernelNetwork 监听已启动」)。
- [ ] 强杀 seecn(任务管理器结束进程)后再启动,仍能正常起(自愈生效);`logman query -ets | findstr seecn` 在重启后只有一个、且是新进程的 session。
- [ ] `cargo build` / `clippy -D warnings` / `test` / `fmt` 仍全绿;`stop_stale_trace` 的 unsafe 块有边界/返回码处理,无 panic 路径。
- [ ] 只动 `etw.rs` 与 `Cargo.toml`,不影响其它模块与既有三态逻辑。

---

## 16. 进程过滤修正:排除 Claude Desktop(bugfix)

### 16.1 问题与证据

**现象**:开着 Claude Desktop 时,托盘 session 数从 3 暴涨到 12。

**实测**(同名 `claude.exe` 进程枚举):

| 来源 | 数量 | 路径 | 特征 |
|---|---|---|---|
| Claude Desktop(Electron) | 9 | `C:\Program Files\WindowsApps\Claude_<ver>_x64__<id>\app\Claude.exe` | 1 主进程 + 8 个 `--type=`(crashpad/gpu/renderer×3/utility×3),均为主进程的子进程 |
| Claude Code CLI(目标) | 3 | `C:\Users\<user>\.local\bin\claude.exe` | 独立顶层进程 |

**root cause**:§9.1 的命中规则里有「进程名为 `claude.exe` 即命中」一条,**只看名字、不看路径**。Claude Desktop 的可执行文件同样叫 `Claude.exe`,且作为 Electron 应用会派生一批**同名**辅助进程,全部被这条 path-blind 规则误纳。`node` 分支不受影响(需带 `claude-code`/`cli.js` 等命令行标记)。

### 16.2 修复:排除 Desktop / Electron

在 §9.1 的命中判定**之前**加一道 deny 闸:命中以下任一即判定为「**非 CLI**」,`continue` 跳过(并 `tracing::debug!` 记录被排除的 pid/path/原因,便于实测核对):

1. **Electron 辅助进程**:`cmd()` 拼接串(小写)包含 `--type=`。通用 Electron 多进程特征,与版本/路径无关,覆盖全部 helper。
2. **Desktop 安装位**:`exe()` 路径(小写)包含 Desktop 安装标记。覆盖没有 `--type=` 的 Desktop 主进程。

```rust
/// Electron 辅助进程命令行特征(--type=renderer / gpu-process / utility …)。
const ELECTRON_TYPE_MARKER: &str = "--type=";
/// Claude Desktop 的安装位标记(用于排除 Desktop;CLI 在用户 bin/node_modules,不沾这些)。
/// - windowsapps\claude:MSIX/Store 安装(C:\Program Files\WindowsApps\Claude_…)。
/// - anthropicclaude:旧版 Squirrel 安装(%LocalAppData%\AnthropicClaude\…)。
const DESKTOP_DENY_PATH_MARKERS: &[&str] = &[r"windowsapps\claude", "anthropicclaude"];
```

判定抽成**纯函数**便于单测(沿用项目风格):

```rust
/// 是否为 Claude Desktop / Electron 进程(应被排除)。入参均为小写。
fn is_desktop_or_electron(exe_path_lower: &str, cmd_lower: &str) -> bool {
    cmd_lower.contains(ELECTRON_TYPE_MARKER)
        || DESKTOP_DENY_PATH_MARKERS.iter().any(|m| exe_path_lower.contains(m))
}
```

`scan()` 流程调整:取 `name`/`exe()`/`cmd()` 后,**先** `is_desktop_or_electron` → 命中则 `continue`;**再**走原有正向命中(`name==claude.exe` 或 `node + claude 标记`)。`exe()` 取不到(`None`)时按空串处理(不因路径缺失而漏排除 `--type=` 那条;同时也不误伤——空路径不含 Desktop 标记)。

> 选 deny Desktop 而非「只正向匹配 CLI 路径」:CLI 安装位多变(native `~/.local/bin`、npm `node_modules`、winget/scoop 等),deny 法只排除明确的非 CLi(Desktop),对各种 CLI 安装方式更宽容。

### 16.3 验收增量

- [ ] Claude Desktop 运行时,`scan()` 只返回 CLI 进程(本机为 3),不含任何 `WindowsApps\Claude…` 路径或 `--type=` 进程。
- [ ] `RUST_LOG=seecn=debug` 能看到被排除的 Desktop/Electron 进程(pid + 原因)。
- [ ] 新增 `is_desktop_or_electron` 单测:Desktop MSIX 路径→true、`--type=renderer` 命令行→true、CLI 路径 `\.local\bin\claude.exe` 且无 `--type=`→false。
- [ ] `cargo build`/`clippy -D warnings`/`test`/`fmt` 全绿;只动 `proc.rs`。

---

## 17. 托盘 tooltip 截断修正(配合 flyout)

**问题**:Windows 托盘 tooltip(`NOTIFYICONDATA.szTip`)是有长度上限的纯字符串,多行 per-session 文本会被 Windows 从中间截断(实测把 `pid 16244` 砍成 `pid 16`,看起来像凭空多了个会话)。`render_tooltip` 的 `TOOLTIP_MAX=127` 定得过高、没触发汇总回退。

**改法**:tooltip 退回为**一行、必定不截断的紧凑摘要**(始终 ≤ 60 字符),例如 `seecn: 1 active / 2 idle / 0 offline (3)`;**逐 session 明细全部交给 §18 的 flyout**。icon 颜色仍表达整体状态。这条与 §18 一起实现。

---

## 18. Flyout:富显示面板(替代 tooltip 承载明细;跨平台模板)

> 目标:点击托盘弹出一个无边框小面板,展示每个 Claude session 的 pid/状态/连接数/速率。
> 用 **HTML 模板 + webview** 实现,使「显示模板」在 Windows / macOS **同一份照抄**。

### 18.1 架构(tao 窗口 + wry webview + HTML 模板)

```
Engine on_update(sessions, overall)
   └─► main: proxy.send_event(TrayUpdate{...})
          ├─► tray: set_icon(overall) + set_tooltip(紧凑摘要)         (§17)
          └─► flyout(若可见): webview.evaluate_script("window.seecnRender(<json>)")
                                  └─► assets/flyout.html 重绘
```

- **窗口壳**:`tao` 建一个 undecorated / always-on-top / 不进任务栏 / 不可缩放 / 背景透明的小窗(圆角由 HTML 卡片提供)。
- **内容**:`wry` 的 `WebView` 挂到该窗口(`WebViewBuilder` + raw-window-handle;wry 0.35+ 用 `RawWindowHandle` 构造),`with_html(include_str!("../assets/flyout.html"))` 内嵌模板(编译进二进制,无运行时文件依赖)。
- **刷新**:Rust 把 `(sessions, overall)` 序列化成 JSON,调 `webview.evaluate_script(&format!("window.seecnRender({json})"))`。模板里 `window.seecnRender(data)` 已就绪(见 `assets/flyout.html`)。

### 18.2 数据契约(与 `model::Session` 对齐)

```json
{ "overall": "active|idle|offline",
  "sessions": [ { "pid": 12432, "state": "active", "conn_count": 3,
                  "rate_in": 3276, "rate_out": 1126, "cmdline": "…" } ] }
```

序列化:schema 固定且简单,**手写一个 `fn sessions_to_json(sessions, overall) -> String`**(转义 cmdline 即可),避免引入 serde 依赖(Rob Pike:数据简单就别上框架)。`LinkState` → 小写字符串 `active/idle/offline`。

### 18.3 交互与窗口行为

- **显示**:监听 tray 左键单击(`tray-icon` 的 `TrayIconEvent::Click`)→ 切换 flyout 显隐。
- **定位**:弹在托盘附近(屏幕右下、任务栏之上;可用 click 事件里的坐标或屏幕工作区右下角推算)。
- **隐藏**:`WindowEvent::Focused(false)`(点窗口外)即 `window.set_visible(false)`,模拟状态栏弹层的"失焦自动收起"。
- **不退出**:隐藏 ≠ 销毁;窗口与 webview 常驻,反复显隐(避免重建 webview 的开销)。
- 注意规避 [tao 托盘右键空窗口的坑](https://github.com/tauri-apps/tao/issues/506):flyout 是我们自建窗口,不要复用 tao 内部 tray 窗口。

### 18.4 跨平台抽象(macOS 照抄的关键)

```rust
/// 平台无关的 flyout 接口;HTML 模板 assets/flyout.html 两端共享。
/// **UI 线程独占**:flyout 持有 tao 窗口 + wry webview,二者非 Send,只在
/// event loop 的闭包里创建与使用,绝不跨线程;故 **不要求 Send**。
pub trait FlyoutView {
    fn toggle(&mut self, json: &str); // 左键托盘:显/隐切换(打开时用 json 立即渲染)
    fn hide(&mut self);
    fn update(&mut self, json: &str); // 推送最新数据(可见时才需重绘)
    fn is_visible(&self) -> bool;
}
```

- **Windows**:tao 窗口 + wry(WebView2)。
- **macOS(未来)**:`NSPanel`/`NSPopover` + wry(WKWebView),**同一份 `flyout.html`**;只有窗口创建/定位/显隐这层 Rust 不同。数据契约与模板完全复用。
- 放到 `src/flyout/`(平台无关 trait + JSON 序列化)与 `platform/windows/flyout.rs`(窗口 glue),`#[cfg(feature)]` 分发,沿用 §8 的平台抽象风格。

### 18.5 依赖与权衡

- 新增 `wry`(版本需与 `tao 0.34` 兼容,实测 `wry 0.53.x` 可搭 `tao 0.34.5`;以 `cargo add` 解析为准)。
- Windows 渲染依赖 **WebView2**(Edge Chromium),**Win11 预装**,无需额外安装。
- 代价:多一个 webview 进程、二进制略大。**收益**:显示模板声明式、可热改、跨平台照抄——契合"做 macOS 时直接抄"的诉求。备选(原生绘制 softbuffer/egui)无 webview 依赖,但"模板"变成 Rust 代码,无法照抄,故不选。

### 18.6 验收

- [ ] 左键点击托盘弹出 flyout,完整列出所有 session(pid/状态/连接数/Active 速率),**无截断**。
- [ ] 点击面板外自动收起;面板打开时数据随评估节拍实时刷新。
- [ ] tooltip 同步简化为紧凑摘要(§17),不再多行截断。
- [ ] `cargo build`/`clippy -D warnings`/`test`/`fmt` 全绿;`flyout.html` 经 `include_str!` 内嵌。
- [ ] HTML 模板与 JSON 契约未写任何 Windows 专属逻辑(保证 macOS 照抄)。

---

## 19. Flyout 闪退修正 + "日志停止" 诊断(bugfix / diagnostics)

### 19.1 Flyout 点击后立即闪退

**现象**:左键托盘弹出后立刻自行关闭。

**可能成因(待日志判定,不预设)**:
1. **双触发**:一次物理点击产生两次 `TrayClick` → `toggle` 两次 → 显示又隐藏。
2. **显示后立即失焦**:`show()` 后窗口未能稳定获得前台焦点(前台抢夺被 Windows 限制),或焦点落入 wry 的 WebView2 子窗口,触发宿主窗口 `Focused(false)` → 被失焦处理器隐藏。

**本轮处理(先止血 + 加观测)**:
- 托盘左键由 `toggle` 改为 **`show`(幂等)**:即便收到两次 `TrayClick` 也只是重复显示,消除双触发闪烁。`FlyoutView::toggle` → `FlyoutView::show(&mut self, json)`。
- 保留 `WindowEvent::Focused(false)` → `hide` 作为"点击 flyout 外部关闭"。
- **加日志**:`TrayClick` 收到、flyout `show`/`hide`、flyout 窗口 `Focused(true|false)` 各打一条 `tracing::debug!`,用于判定真因。

**若仍闪退**:日志将显示 `show` 后随即 `Focused(false)` → 下一轮改用可靠的 light-dismiss:Win32 `AttachThreadInput` 强制前台获取焦点,或安装 `WH_MOUSE_LL` 低层鼠标钩子检测"点击落在 flyout 矩形外"再隐藏(不依赖窗口焦点)。

### 19.2 "运行一段时间后日志停止" 诊断

**不猜测,加观测**以区分三种可能(事件源停 / 监听逻辑停 / 判定不变):
1. **Panic hook**:`main` 起线程前 `std::panic::set_hook`,任何线程 panic 都 `tracing::error!` 出线程名 + panic 信息(后台线程默认静默终止,易被忽略)。
2. **Engine 心跳**:`Engine::run` 每 `HEARTBEAT_INTERVAL`(~5s)打一条 `engine 心跳: +N net事件, conns=X, procs=Y`(N=自上次心跳以来处理的 `EngineMsg::Net` 数)。

**判读**:
- 心跳**停** → Engine 线程已死;panic hook 给出原因。
- 心跳**在但 N=0** → ETW 事件不再到达(事件源/ETW 会话/net-bridge 侧),监听逻辑仍活。
- 心跳**在且 N>0 但状态不再变 Active** → 三态判定 / 阈值问题(§14)。

### 19.3 验收

- [ ] 编译类四绿;不破坏既有逻辑。
- [ ] e2e:点托盘弹出不再立即闪退;日志能看到 `TrayClick`/`show`/`hide`/`flyout 焦点` 序列与 `engine 心跳`。
- [ ] 据 e2e 日志定位两 bug 真因,再做后续针对性修复。

---

## 20. Flyout 失焦闪退:最终修法(前台轮询 light-dismiss)

### 20.1 真因(e2e 日志证实)

```
flyout show
flyout 焦点变化 focused=true      ← 窗口拿到焦点
flyout 焦点变化 focused=false     ← 约 0.1ms 后立刻丢失
flyout hide
```

窗口拿到焦点后瞬间被 **wry 的 WebView2 子窗口**夺走。tao 的 `WindowEvent::Focused` 是**窗口级**(WM_SETFOCUS/KILLFOCUS),无法区分「焦点进了自家 webview 子窗」与「点了外部」,故 `show` 后必然立即误触发失焦 → hide(闪退)。且子窗夺焦后父窗口已处于失焦态,后续点外部不会再产生父窗口失焦事件——**焦点事件无法实现「点外部关闭」**。

### 20.2 修法:前台窗口轮询

洞察:webview 子窗夺的是**窗口焦点**,但顶层**前台窗口**(`GetForegroundWindow`)仍是 flyout;只有切到别的 app/桌面/任务栏,前台窗口才变。

- **移除** `Focused(false) → hide`(`Focused` 仅保留 `tracing::debug!` 便于观测,不再触发隐藏)。
- `WinFlyout` 在构造时从 tao 窗口的 raw-window-handle(rwh 0.6 `Win32WindowHandle.hwnd`)取出顶层 `HWND` 存好;`show()` 记录 `shown_at: Instant`。
- 新增 `FlyoutView::poll_autohide(&mut self)`;Windows 实现:
  ```text
  若 !visible → 返回
  若 now - shown_at < DISMISS_GRACE(~600ms) → 返回(刚弹出,跳过一轮,避免前台未稳就被收起)
  若 GetForegroundWindow() != self.hwnd → hide()   // 前台已不是本窗口 = 点了外部
  ```
- `tray.rs` 在每次 `UserEvent::TrayUpdate`(Engine ~500ms 节拍)末尾调 `flyout.poll_autohide()`。

依赖:`Cargo.toml` 的 `windows-sys` 增补 feature `Win32_UI_WindowsAndMessaging`(`GetForegroundWindow`);HWND 取自 raw-window-handle(tao/wry 已带 rwh 0.6,如需显式 `use` 以 `cargo add raw-window-handle` 解析与之一致的版本,或经 tao 的再导出)。

### 20.3 验收

- [ ] 编译四绿;`tray-click → show` 仍幂等;`poll_autohide` 仅 Win32 `GetForegroundWindow` 一次调用,无全局钩子/子类化。
- [ ] e2e:点托盘弹出**不再闪退**;在 flyout 内/操作其内容**不收起**;点外部(另一窗口/桌面/任务栏)~0.5s 内收起。

---

## 21. Flyout 渲染修正:无白底 + 按内容自适应高度 + 速率平滑

### 21.1 白色缝隙(已在 assets/flyout.html 完成)

根因:`html,body{background:transparent}` 依赖 WebView2 窗口透明(本环境不生效→白底);`#card` 固定宽 + 圆角 + 外边框,内容撑高小于固定窗高 → 下方与四角缝隙露白。
修法(CSS):`html,body{height:100%;background:var(--bg)}` 不透明深色填满;`#card` 改 full-bleed(`display:flex;flex-direction:column;min-height:100vh`,去掉圆角/外边框);`ul#list{flex:1;overflow-y:auto}`。**无任何白底**。

### 21.2 窗口高度按 session 数自适应

**问题**:窗口固定 300×220,session 少则下方留深色空白、多则裁切。Rust 已知 session 数,直接据此调窗高(无需 webview 回传)。

- `FlyoutView` 新增 `fn resize_for(&mut self, session_count: usize);`
- Windows 实现:逻辑高度 `h = HEADER_H + clamp(max(1, n), 1..=MAX_ROWS) * ROW_H`
  - 常量(逻辑像素,与 flyout.html 实测对齐,可 e2e 微调):`HEADER_H ≈ 46`、`ROW_H ≈ 50`、`MAX_ROWS = 8`(超过则窗口封顶、列表内部滚动)。
  - `self.window.set_inner_size(LogicalSize::new(300.0, h))`(tao 按 DPI 折算物理像素),随后重新 `position_bottom_right()` 重锚右下角。
- `position_bottom_right()` 改用 `self.window.outer_size()` 实际尺寸定位(不再用固定 `FLYOUT_H` 常量),保证 resize 后锚点正确。`FLYOUT_H` 退化为初始高度。
- `tray.rs`:维护 `last_count`(每次 `TrayUpdate` 取 `sessions.len()`);在 `TrayClick` 的 `show` 之后、`TrayUpdate` 可见时的 `update` 之后,调 `f.resize_for(last_count)`。

### 21.3 速率显示平滑(Active 不再抖成 0)

**问题**:`Session.rate_in/out` 是**瞬时** `delta/dt`;SSE 阵发,许多 500ms 拍为 0,Active 时显示 `↓0 ↑0` 误导(状态是 1.5s 粘滞,但这一拍确实 0 字节)。

**修法**:展示改用 **2 秒窗口平均**(与 active 判定同口径)。
- `PidFlow` 增 `up_buckets: VecDeque<u64>`(对称 `down_buckets`);`evaluate` 每拍 `push_back(delta_out)` 并维持 N 桶。
- 展示速率:`rate_in = sum(down_buckets)/RATE_WINDOW`(即已有的 `down_rate`)、`rate_out = sum(up_buckets)/RATE_WINDOW`。
- **burst 检测仍用瞬时 `delta_out`**(§14.4 不变),只改"展示用"速率口径。

### 21.4 验收

- [ ] flyout 无任何白底;窗口高度贴合 session 数(2 个 ≈ 2 行高,不留大片空白、不裁切)。
- [ ] Active 会话速率稳定显示 2s 窗口平均(不再瞬时抖成 0);静默(Idle)不显示速率。
- [ ] `cargo build`/`clippy -D warnings`/`test`/`fmt` 全绿;只动 flyout.html / flyout(mod+windows)/ tray.rs / monitor.rs。
