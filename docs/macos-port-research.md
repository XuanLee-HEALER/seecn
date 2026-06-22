# seecn macOS Port 调研笔记(handoff)

> 状态:**调研进行中,未编码**。本文件是给新 session 接手用的中间产物。
> 工作方式约定:先调研清楚 → 写文档 → 用户 review 确认 → 才开始写代码。
> 本机:Apple Silicon (arm64),macOS Darwin 25.x,非 root 用户。

## 0. 任务

把 seecn(当前仅 Windows)port 到 macOS,分两部分:
1. **core 平台相关层**:对应 Windows 的 ETW + netstat2 + sysinfo(实时网络监控 / 连接快照 / 进程发现)。
2. **UI**:macOS 状态栏应用(对应 Windows 的 tray-icon + flyout 窗口)。

## 1. 已确认的设计决策(用户已拍板)

- **单版本、普通权限、直接三态**:macOS 普通用户(非 sudo)就能拿到自己 Claude 进程的连接和实时字节(已实地验证),所以**不请求管理员权限,不做"提权三态/降级两态"的双版本**。`current_privilege()` 在 macOS 直接当普通态,普通态照样跑满三态。
- **按 Claude pid 判定,绝不校验 remote IP**:沿用项目既有哲学。官方接口、远程第三方中转都天然支持(都是 Claude pid 的一条公网 ESTABLISHED 外连)。
- **第三方接口形态**:用户两种都可能用 —— 远程公网中转(base_url 指向第三方公网)+ 本地代理(Claude 连 `127.0.0.1:端口`)。**底线:用官方接口时识别绝不能出错。**
- **loopback 处理方向**:默认**跳过 loopback**(保官方接口判定干净 —— Claude 用官方接口时,到本地 MCP server / IPC 的回环连接会被这条规则自动滤掉,不会把"在用本地 MCP"误判成"在和 LLM 通信")。本地代理形态作为**可选**支持(配置代理地址白名单,或自动读 Claude 的 `ANTHROPIC_BASE_URL`,只精确纳入那一条 loopback)。**这块的技术验证尚未完成,见 §5。**

## 2. 跨平台复用边界(读代码得出,已确认)

**完全复用,平台无关,不改**:
- `src/model.rs` — 类型/常量契约(LinkState 三态、ConnKey、NetEvent、Session、各时间常量、阈值)
- `src/state.rs` — 状态机纯函数(`is_effective_activity` / `classify` / `overall`)
- `src/monitor.rs` — Engine(单线程消费 EngineMsg,维护 conns/flows,评估三态)
- `src/flyout/mod.rs` — `FlyoutView` trait + `sessions_to_json`(手写 JSON)
- `assets/flyout.html` — 显示模板(两端共享,wry 渲染)
- `src/tray.rs` — 托盘 UI **用的是跨平台的 `tray-icon` + `muda`**(不是 tao system_tray),只有 `platform::new_flyout(&event_loop)` 是平台分支
- `src/main.rs` — wiring(起线程、跑 event loop)

**需要 port 的只有 `src/platform/macos/` 这层 glue**(对照 `platform/windows/`):
- `proc.rs` — 进程发现(`ProcScanner::scan`)
- `tcptable.rs` — 连接快照(`TcpSnapshot::snapshot`)
- `net.rs`(对应 windows `etw.rs`)— 实时字节遥测(`NetMonitor::start`)
- `flyout.rs` — 状态栏浮层窗口(`FlyoutView` 实现)
- `mod.rs` — 构造函数 `new_proc_scanner/new_tcp_snapshot/new_net_monitor/new_flyout` + `detect_privilege`

**平台抽象 trait(已锁定,`src/platform/mod.rs`)**:
```rust
trait ProcScanner: Send { fn scan(&mut self) -> Vec<ClaudeProc>; }
trait TcpSnapshot: Send { fn snapshot(&self, pids: &HashSet<u32>) -> Vec<(u32, ConnKey)>; }
trait NetMonitor: Send { fn start(&mut self, claude_pids: Arc<RwLock<HashSet<u32>>>, tx: Sender<NetEvent>) -> Result<()>; }
trait FlyoutView { fn show/hide/update/resize_for/is_visible/poll_autohide/window_id; }
```
- `platform/mod.rs` 目前 macOS 分支是 `compile_error!("macOS platform not implemented yet")`,要改成 cfg 分发到 `macos` 模块。
- `Cargo.toml` 的 `macos-platform` feature 当前是空占位。

**NetEvent 关键认识**:Engine 的 `apply_net` 对 `Data{pid,key,inbound,outbound}` 是"按 key 找桶、累加字节、刷 last_activity",评估时**按 pid 聚合**。所以 macOS 即使拿不到精确四元组,给每个 pid 造一个**合成 ConnKey** 也能工作(DESIGN 明示连接键精度是次要目标)。三种事件:
- `Connect`/`Disconnect` ← 连接快照维护连接存在性(判 Offline/Idle)
- `Data` ← 实时字节遥测(判 Active + 算速率)
