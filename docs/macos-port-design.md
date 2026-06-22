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
| `flyout.rs` 真状态栏浮层 | flyout.rs | ⏳ 下一阶段 |

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

两道闸保证 Desktop 永不混入:① proc.rs 把 Claude.app 全部进程 deny,不进 `claude_pids`;② net.rs 全局跑 nettop,但解析每条连接行时按其归属进程行的 pid 查 `claude_pids`,Desktop 的 pid 不在集合 → 直接丢弃、**不组事件**。即便两者都在跑 claude code,也因 pid 不同天然分离(实测:CLI 走 utun10、Desktop 走 en0,零重叠)。

## 3. net.rs — nettop 常驻流 → NetEvent

`MacNetMonitor::start` fork 一个常驻 `nettop -n -x -d -s 1 -l 0`(数字地址 / 纯文本 / delta / 1s 间隔 / 无限),后台线程逐行解析 stdout:

- **表头行**(首字段 `time`)= 采样周期边界:周期末 diff `known` vs 本周期 `seen`,消失的连接 → `Disconnect`。
- **进程行**(`a.b.c.<pid>`,pid 在末段):更新"当前归属 pid" + 查 `claude_pids` 决定是否关心。
- **连接行**(`tcp4 LOCAL<->REMOTE iface state bin bout`):归属当前 pid;四元组 → `ConnKey`;首见 → `Connect`;`bin/bout`(delta)>0 → `Data{inbound,outbound}`。0/0 不发(Engine 对 alive 连接不 GC,无需保活事件)。

与 ETW 同契约:`Connect`/`Data`(增量)/`Disconnect` 经 `tx` 推送,pid 过滤走共享 `claude_pids`。解析全程尽力而为、失败跳过、不 panic。

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

## 6. 健壮性:孤儿 / 崩溃自救(已实测)

- **nettop 孤儿:不存在**。主进程一死,nettop 的 stdout 读端关闭,它下个采样周期(每秒)写 stdout 即吃 SIGPIPE 自杀。实测 SIGKILL 主进程后 nettop ~1s 内自动消失,无残留、无需主动 kill。
- **nettop 崩溃自救:监督循环**。`supervise()` 解析当前 nettop 直到流断 → kill+wait 回收 → 退避(1s 指数到 30s 上限,上条若稳定存活过则重置)后重启。实测 SIGKILL nettop 子进程后,主进程 1s 内重启出新 nettop、数据流恢复;崩溃期间 Engine 靠存量连接 + GC 维持。

## 7. 已知项 / 下一阶段

- **日志文案**:`main.rs` 复用层写死 "ETW",macOS 实为 nettop;为守"复用层不改"未动,留 UI 阶段统一做平台感知文案。
- **dead_code warning**:macOS no-op flyout 未用 `FLYOUT_HTML`/`hide`,做真 UI 后自然消失。
- **下一阶段**:`macos/flyout.rs` 真状态栏 flyout(对应 windows/flyout.rs),把 no-op 换成 wry webview 浮层。
