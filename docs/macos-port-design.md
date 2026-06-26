# seecn macOS Port 实现设计(v1 / macOS)

> 配套 `docs/DESIGN.md`(v1 / Windows)。本文是 macOS port 的权威实现设计,取代 `docs/macos-port-research.md`(调研笔记)成为编码依据。
> 本期范围:`src/platform/macos/` 的 core 三件套(proc / tcptable / net)+ mod 接线,打通 Offline/Idle/Active 三态 + 本地代理白名单。UI(状态栏 flyout)留下一阶段。
> 工作方式:本文档 review 通过后才编码。
> 本机:Apple Silicon(arm64),macOS Darwin 25.x,非 root 用户。

## 0. 本机实测结论(6 个真实 CLI session 在跑)

| 项 | 实测结论 | 对设计的影响 |
|---|---|---|
| CLI 进程指纹 | comm=`claude`;args 形如 `claude --dangerously-skip-permissions`;装在 `~/.local/share/claude/versions/<ver>/` | proc.rs allow=`claude`;deny=`/Applications/Claude.app/` + `--type=` |
| 公网连接 | 每 session 3~4 条 :443;远端混官方 `160.79.104.10` + 国内中转 IP;源 `172.19.0.1` 走 utun10 | 实锤"按 pid 判定、不看 remote IP" |
| nettop -d | per-connection 行带远端四元组 + bytes_in/out;`-d` delta 模式**由 nettop 自己算好增量** | net.rs 用 `nettop -n -x -d -s 1 -l 0` 常驻流,delta 直接当 NetEvent::Data |
| 解析坑 | -x 打平后连接行不带 pid,靠行序归属上方进程行 | **不加 -p**:全局监控,解析时按共享 `claude_pids` 过滤(贴 ETW 模型,支持 pid 集合动态增减) |
| loopback 现状 | 当前无人用本地代理 | 白名单逻辑+单测就绪,E2E 待靶 |

## 0.5 net 机制选型:nettop 而非 ntstat 直连(实测定论)

更"优雅"的方案是直连私有 `com.apple.network.statistics`(ntstat)kernel control —— 事件推送、进程内、无子进程,对标 Windows ETW。**但本机实测否决了它**:

- socket / `ioctl(CTLIOCGINFO)` / connect 全成功,`NSTAT_MSG_TYPE_ADD_ALL_SRCS` 对全部 4 个 provider(TCP/UDP × KERNEL/USERLAND)一律回 `error=2 (ENOENT)`。
- 根因:`nettop` 自带 Apple **私有** entitlement `com.apple.private.network.statistics`(codesign 实证),并链接私有 `NetworkStatistics.framework`。未签名/第三方签名二进制拿不到该 entitlement(amfi 拒),故直连订阅被 ENOENT 挡死。
- 探针留存 `tools/ntstat-probe.rs`(实测证据)。

反推:`nettop` **非 root** 就能拿 per-pid 字节,正因它替我们持有那个 entitlement。所以 net.rs 借 nettop —— fork 一个常驻 `nettop -d` 流,解析 stdout 组 NetEvent,等价于借 nettop 的 entitlement 拿 ntstat 的推送数据。

(更"原生"的官方公开替代是 NetworkExtension/`NEFilterDataProvider`,但要 System Extension 打包 + 用户批准 + 开发者签名,重型,留给正式产品形态。)

## 1. 范围与复用边界

复用层一行不改:`model.rs` / `state.rs` / `monitor.rs`(Engine + §14 三态)/ `flyout/mod.rs` / `tray.rs` / `main.rs`。

本期新写 `src/platform/macos/`:

| 文件 | 对应 windows | 本期 |
|---|---|---|
| `proc.rs` `MacProcScanner` | proc.rs | ✅ |
| `tcptable.rs` `MacTcpSnapshot` | tcptable.rs | ✅ |
| `net.rs` `MacNetMonitor`(nettop) | etw.rs | ✅ |
| `mod.rs` 构造 + detect_privilege + no-op flyout | mod.rs | ✅ |
| `flyout.rs` `MacFlyout` 真状态栏浮层 | flyout.rs | ✅ |

接线:`platform/mod.rs` 去掉 `compile_error!` 改 cfg 分发;`Cargo.toml` 的 `macos-platform` 启用 `dep:netstat2`。UI 本期用 no-op flyout 让 `main.rs` 原样编译,三态靠 `RUST_LOG=debug` 日志 + 托盘验证。

## 2. proc.rs — 进程发现 + CLI/Desktop 隔离

sysinfo 跨平台,结构照搬 `WinProcScanner`。命中两步(先 deny 后 allow):

- deny:exe 路径含 `/applications/claude.app/` 或 args 含 `--type=` → 跳过(Desktop/Electron)。
- allow:`comm=="claude"`(macOS CLI 即此)或 node+`claude-code` 标记。

抽 `is_desktop_or_electron()` 纯函数 + 单测(Desktop 主进程/helper→true;CLI `.local/share/claude`→false)。

### CLI 与 Desktop 如何隔离(你反复问的点)

`nettop` 不是抓包(tcpdump),是按进程统计 —— 每条连接出生即带 pid 归属。两类流量天生在不同 pid 名下:

| | 进程 | 出口(实测) | 是否监控 |
|---|---|---|---|
| CLI 的 claude code | 独立 `claude` 进程 | 172.19.0.1 / utun10 | ✅ |
| Desktop 的 claude code | Claude.app 的 NetworkService(单一 pid 如 65594) | 192.168.77.81 / en0 | ❌ |

两道闸保证 Desktop 永不混入:① proc.rs 把 Claude.app 全部进程 deny,不进 `claude_pids`;② net.rs 每轮用 `-p <仅 Claude pid>` 锁定 nettop,只采 Claude 进程,Desktop 的 pid 不在 -p 列表、**连采都不采**。即便两者都在跑 claude code,也因 pid 不同天然分离(实测:CLI 走 utun10、Desktop 走 en0,零重叠)。

## 3. net.rs — nettop 单次快照轮询 → NetEvent

> ⚠️ 原设计是 `nettop -l 0` 持续流,但实测 `nettop -x` 在采样间隔里 **busy-spin**(`-s 5` 也烧满核 ~138% CPU、根本没 sleep)。改为**单次快照轮询**:`nettop -l 1` 实测 ~40ms 退出、不空转,每秒 fork 一次、占空比实测 ~1%。

`MacNetMonitor::start` 起轮询线程,每 `POLL_INTERVAL`(1s):

1. 取当前 `claude_pids` → `Command::output()` 跑 `nettop -n -x -l 1 -p <pids>`(单次快照,阻塞到结束)。`-p` 锁定 Claude 进程,每轮取最新集合 → 支持动态增减,也不再扫全系统连接。
2. 解析快照:进程行更新归属 pid;连接行 `tcp4 LOCAL<->REMOTE iface state cum_in cum_out` → `ConnKey` + **累计**字节。
3. 自己算 delta:维护 `(pid,key) → 上次累计`;首见发 `Connect` 记基线、本轮不发 Data;已知则 `cum - prev`(饱和减防回绕)>0 发 `Data{inbound,outbound}`。
4. 本轮 `seen` 与 `known` diff,消失的连接 → `Disconnect`。
5. `thread::sleep(POLL_INTERVAL)` **真正休眠**(关键:不依赖 nettop 的 busy 间隔)。

与 ETW 同契约:`Connect`/`Data`(增量)/`Disconnect` 经 `tx` 推送。解析全程尽力而为、失败跳过、不 panic。

## 4. 接线与权限

- `platform/mod.rs`:`compile_error!` → cfg 分发 macos 模块 + 导出构造函数;`current_privilege` 加 macos 分支。
- `detect_privilege` 返回 `Elevated`(语义=三态可用):nettop 非 root 即三态,以此让上层日志/tooltip 不误报两态。
- `Cargo.toml`:`macos-platform = ["dep:netstat2"]`。
- `mod.rs` 的 `MacFlyout`:no-op,仅建一个隐藏 tao 窗口提供 `WindowId` 给 tray 事件匹配。

## 5. 本机端到端实测(已验证,三态 = ETW 等同)

`RUST_LOG=seecn=debug` 跑主程序实测:

- proc:7 个 CLI `claude` 命中;所有 `claude helper`/crashpad/`Claude.app/Contents/MacOS/claude` 精准排除。
- net:心跳 `net_events≈80~120`/周期,`conns=32 procs=7`。
- 三态:`state=Active`(down_rate 39 万~56 万 B/s)、`state=Idle`(流量间隙)均如实判出;Offline 因 session 都在跑未触发(预期)。
- cmdline:改用 `refresh_processes_specifics(.with_cmd/.with_exe)` 后填上 `claude --dangerously-skip-permissions`。

## 6. 健壮性 + CPU(已实测)

- **CPU ~1%**:原 `-l 0` 持续流因 nettop `-x` 的 busy-spin 烧满核(~138%,与 `-s`/`-d`/`-p` 都无关);换单次快照轮询后 seecn 实测 **1.2%**、无常驻 nettop 进程。
- **无孤儿**:轮询的 `nettop -l 1` 每次 ~40ms 自行退出,根本没有长驻子进程,孤儿问题不存在。
- **崩溃自救**:某轮 `nettop -l 1` 失败只是本轮 `ERROR_BACKOFF`(5s)后重试,不影响后续轮次;期间 Engine 靠存量连接 + GC 维持。
- **无 Claude 进程时**:`claude_pids` 空 → 不跑 nettop、把存量连接全 `Disconnect`,几乎零开销。

## 7. flyout — 状态栏浮层(已实现)

`MacFlyout`(flyout.rs)照搬 windows/flyout.rs 模式:tao 无边框 + 透明 + always-on-top 窗口,内挂 wry WebView 加载同一个 `FLYOUT_HTML`,`evaluate_script("window.seecnRender(json)")` 渲染;显隐而非销毁,UI 线程独占。

三处 macOS 差异全用 tao 内建 API 在本模块内解决,**零改复用层**:
- **定位**:右上角(菜单栏下),对称 Windows 的右下角——各自系统状态区所在的屏幕角落,不需要图标 rect。
- **light-dismiss**:`poll_autohide` 用 `Window::is_focused()` 查询(WKWebView 是 NSView、不夺窗口焦点,焦点可靠);省去 Windows 的 `GetForegroundWindow` 轮询。
- **activation policy**:`new` 里 `set_activation_policy_at_runtime(Accessory)`(不占 Dock),不碰 main/tray。

可操作性先用独立 demo(`tools/flyout-demo/`)验证:webview IPC 回传证明卡片渲染(264×88、圆角 14px、半透明)、`Focused` 事件触发、`Accessory` 生效。正式集成后主程序 flyout 创建成功、三态不回归、0 warning。

## 8. 已知项 / 下一阶段

- **日志文案**:`main.rs` 复用层写死 "ETW",macOS 实为 nettop;为守"复用层不改"未动,可后续做平台感知文案。
- **flyout 定位/尺寸**:`MENUBAR_RESERVE` / 行高等是保守初值,待 e2e 微调。
- **截图视觉验证**:本机 ghostty 未授屏幕录制,flyout 像素级外观靠 demo webview 自验证 + 肉眼确认,未留截图。
