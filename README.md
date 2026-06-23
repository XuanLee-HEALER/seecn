# seecn — see-claude-network

> 一个**被动的网络状态传感器**:不读取、不处理、不上报任何流量内容,只回答一个问题——
>
> **这台机器上的每个 Claude Code CLI 会话,此刻和 Claude 服务器的连接是什么状态?**

它常驻系统托盘,用一个圆点图标实时表达整体状态(灰 / 蓝 / 绿),左键点开是一个名为 **cc monitor** 的浮层,逐个列出每个会话的状态、连接数与收发速率。

---

## 项目目的

同时开着多个 Claude Code 窗口跑任务时,你很难一眼看出**哪个还在干活、哪个挂着等输入、哪个断了**。`seecn` 把这件事做成一个零打扰的状态灯:

- 长任务时不用盯终端——**看托盘颜色**就知道 agent 还在跑(绿)还是卡住了(蓝);
- 多会话时点开浮层,**逐个会话**看状态与实时速率;
- 作为「Claude 到底有没有在和服务器通信」的 ground truth(排查网络 / 代理问题)。

它的定位是**纯粹的传感器**:只读本机连接的元数据(进程、字节计数、连接存在性),**绝不碰流量内容、不解密、不联网上报、不持久化**。

**监控范围**:只盯 Claude Code CLI(Windows 上是 `~/.local/bin/claude.exe`,或 `node` 跑 `@anthropic-ai/claude-code/cli.js`)。**不**监控 Claude Desktop,**不**监控浏览器里的 claude.ai。

---

## 三态含义

每个 Claude 进程聚合它的全部 TCP 连接,得到一个三态链路状态:

| 状态 | 托盘颜色 | 含义 |
|---|---|---|
| **Offline** | 灰 `#7A7A7A` | 没有到服务器的活跃连接 |
| **Idle** | 蓝 `#3B82F6` | 长连接在,但静默(无有意义的数据流) |
| **Active** | 绿 `#22C55E` | 正在进行一次请求 / SSE 流式响应 |

机器整体状态:**任一会话 Active → 整体 Active;否则任一 Idle → Idle;否则 Offline。**

界面:

- **托盘图标**:按整体状态着色;**右键 → Quit** 干净退出。
- **托盘 tooltip**:一行紧凑摘要 `cc monitor — N active / M idle / K offline`。
- **cc monitor 浮层**:**左键点托盘**弹出,逐会话显示 `pid / 状态 / 连接数 / Active 速率`;**点浮层外部**自动收起。浮层用内嵌的 HTML 模板渲染,窗口高度随会话数自适应。

---

## 安装

### macOS — Homebrew(推荐)

```sh
brew install XuanLee-HEALER/tap/seecn
```

直接 `seecn` 启动,常驻状态栏。**非 root 即跑满三态**(借 nettop 的 entitlement,不像 Windows 要管理员)。

> 二进制未签名,首次打开若被 Gatekeeper 拦,解除隔离即可:
> `xattr -dr com.apple.quarantine "$(brew --prefix)/bin/seecn"`

### Windows — 下载 exe(免安装)

从 [Releases](https://github.com/XuanLee-HEALER/seecn/releases) 下 `seecn.exe`,**双击即用**(portable,无需安装)。三态需管理员(右键 → 以管理员身份运行);非管理员自动降级两态。

### 手动下载(macOS)

也可从 Releases 下 `seecn-macos.tar.gz`(universal 二进制,Intel + Apple Silicon),解压即用。

---

## Windows 实现原理

### 核心难点:L4 的「字节」 vs L7 的「语义」

三态想表达的是 **L7 语义**(此刻有没有一次进行中的请求 / SSE 流),但能拿到的只是 **L4(TCP)**的收发字节;中间隔着 TLS 加密。所以在**不解密**的前提下只能用 L4 的可见特征(速率 / 方向 / 持续性)去**逼近** L7。`seecn` 的判定:

- **下行持续流** = SSE 正在流式返回 → 把最近 2 秒的下行字节做窗口平均,超过阈值即「有效活动」;keepalive / HTTP2 PING 这类「小而周期」的保活流量被阈值过滤掉。
- **上行突发** = 刚发出请求体 → 单拍上行字节超过阈值即「有效活动」,覆盖「请求已发、首 token 未到」的空档。
- 命中任一即把状态刷成 **Active**,并粘滞 `ACTIVE_WINDOW`(1.5s)抹平 token 间隙的抖动。

### 数据来源:ETW(实时、带外、需管理员)

实时的收发事件来自 **ETW** 的 `Microsoft-Windows-Kernel-Network` provider——这是网络栈**带外吐出的遥测**,不在数据通路上(consumer 慢了/崩了都不影响真实流量),正好契合「纯传感器」定位。

- **需要管理员权限**(ETW Kernel-Network provider 的硬性要求)。
- keyword 收窄到 **IPv4 + IPv6**(`0x10 | 0x20`,经 `logman query providers` 核对的 manifest 值),不订阅用不到的 Analytic 通道。
- 回调是**热路径**:先按「Claude 进程集合」过滤再做任何解析,失败一律 early-return,**绝不 panic**(panic 跨 FFI 边界是 UB)。
- **会话自愈**:ETW realtime session 是系统级持久对象,进程被强杀时不会自动回收;故每次启动先停掉同名残留 session 再重建,避免 `ERROR_ALREADY_EXISTS`。

### 进程发现 + 连接引导

- **进程发现**(`sysinfo`):按可执行名 + 命令行匹配 Claude CLI;**显式排除 Claude Desktop**——它同样叫 `Claude.exe` 且是 Electron 多进程(主进程 + 一堆 `--type=` 子进程),靠「命令行含 `--type=`」与「路径在 `WindowsApps\Claude` / `AnthropicClaude`」两道 deny 闸踢掉。
- **TCP 快照**(`netstat2`):ETW 是增量事件,补不上「启动前就已存在的连接」;故对新发现的 pid 拍一次 `GetExtendedTcpTable` 快照,把已有的 ESTABLISHED 连接补进表。
- **不强校验远程 IP**:以「Claude PID + ESTABLISHED 外连」为判定依据,不去猜 Anthropic 的服务器 IP(它走 Cloudflare,IP 多变;实测本机流量还可能先过本地代理)。

### 引擎与界面

```
   ProcScan(sysinfo,每 2s) ─┐         ETW KernelNetwork 回调(实时)
   TcpSnapshot(netstat2) ────┤         Connect/Disconnect/Send/Recv
   给新 pid 补已存在连接       │              │ 按 claude_pids 过滤
                              ▼              ▼
                         EngineMsg channel(crossbeam,单消费者)
                              │
                      Engine 线程(单线程独占 conns 表,无锁)
                              │ Vec<Session> → 三态 + 速率 + JSON
                              ▼
                    主线程 tao EventLoop + tray-icon + wry 浮层
```

- 所有事件源汇入**一个 channel**,Engine **单线程串行消费**;连接表只被 Engine 独占,**无需加锁**,唯一跨线程共享的是 ETW 回调用来过滤的 `claude_pids` 集合。
- **界面**:`tray-icon`(纯色圆点)+ `tao` 事件循环 + `wry`(WebView2)浮层。浮层用 `GetForegroundWindow` 轮询实现「点外部关闭」(webview 子窗会瞬间夺走窗口焦点,故不用焦点事件判定)。

> 各模块的类型 / 函数签名以 `docs/DESIGN.md` 为接口契约。

---

## 构建

需要 Rust 工具链。默认 feature 为 `windows-platform`;macOS 需关掉默认、启用 `macos-platform`。

```sh
# Windows(默认 feature)
cargo build --release

# macOS(Intel / Apple Silicon)
cargo build --release --no-default-features --features macos-platform

cargo test  # 平台无关单测(状态机 / JSON 序列化)
```

release profile 已开 `opt-level=z + lto + strip`(体积优先)。

装了 [`just`](https://github.com/casey/just) 的话:`just build` / `just release` / `just check` / `just lint` / `just fmt` / `just ci`。

---

## 运行

### 以管理员运行(推荐,启用三态)

```pwsh
just run-admin
```

会用 `Start-Process -Verb RunAs` 拉起一个提权的 PowerShell 来 `cargo run`(弹一次 UAC),并带上 `RUST_LOG=seecn=debug,info` 便于观察。等价手动做法:**以管理员打开 PowerShell**,在项目目录执行 `cargo run`。

启动后托盘出现圆点图标:打开一个 Claude Code 会话发消息 → 变**绿(Active)**;收发结束 → 回落**蓝(Idle)**;关闭会话 → 回**灰(Offline)**。左键点托盘弹出 **cc monitor** 浮层。

### 直接运行(非管理员,降级两态)

```pwsh
just run    # 或 cargo run
```

非管理员时 ETW 起不来,程序**不崩溃**,自动降级:仅靠进程发现 + TCP 快照工作,只能区分 **Offline / Idle**(看不到实时字节,无法判 Active)。tooltip 会标注 `(no admin: 2-state mode)`。权限是启动时自动检测的(查 token 的 `TokenElevation`),无需手动指定。

---

## macOS 实现原理

已支持(macOS 11+,Apple Silicon / Intel)。**平台无关的核心完全复用**——三态状态机、`(sessions, overall) → JSON` 数据契约、`FlyoutView` trait、`assets/flyout.html` 模板两端通用(`wry` 在 macOS 用 WKWebView,同一份 HTML 浮层);只换了探测与窗口两层 glue。

### 数据来源:nettop(借 entitlement,无需 root)

macOS 的 per-pid 实时字节走 `nettop` 常驻子进程(`nettop -n -x -d -s 1 -l 0`,逐行解析 delta 流组成 `NetEvent`,对应 Windows 的 ETW)。

- **为什么不直连内核**:per-pid 字节的内核源是私有 `com.apple.network.statistics`(ntstat)control socket,直连订阅需 Apple **私有** entitlement `com.apple.private.network.statistics`(未签名二进制实测对 `ADD_ALL_SRCS` 一律 `ENOENT`)。`nettop` 自带该 entitlement,故借它——等于借 nettop 拿 ntstat 的推送数据,**非 root 即可跑满三态**(不像 Windows ETW 必须管理员)。
- **健壮性**:nettop 崩溃由监督循环退避重启(1s 指数到 30s 上限);主进程退出时 nettop 因 stdout 读端关闭吃 SIGPIPE 自动消失,**无孤儿残留**。
- **进程隔离**:`comm=="claude"` 命中原生 CLI;`/Applications/Claude.app/` 路径 + `--type=` 两道 deny 闸排除 Desktop / Electron。连接快照走跨平台的 `netstat2`。

### 托盘 / 浮层

`tray-icon`(NSStatusItem)+ `tao` 无边框透明窗口 + `wry`(WKWebView),复用同一份 `flyout.html`。三处 macOS 差异都用 tao 内建 API 在平台层内解决、**零改复用层**:

- **定位**:flyout 锚定到状态栏图标正下方(`TrayIconEvent::Click.rect`)。
- **light-dismiss**:`Window::is_focused()` 查询即可(WKWebView 是 NSView、不夺窗口焦点,比 Windows 的 `GetForegroundWindow` 轮询更省心)。
- **不占 Dock**:`set_activation_policy(Accessory)`,状态栏 app 标准形态。

### 三态阈值的平台差异

nettop 的字节口径含协议开销,keepalive 基线比 Windows ETW 的 payload 口径高(实测 ~850–1300 B/s vs 数十 B/s),故 `DOWN_RATE_ACTIVE_THRESHOLD` 按平台分(Windows 256 / macOS 2048,后者待真实 SSE 流量 e2e 校准)。

> macOS 实现设计见 `docs/macos-port-design.md`。

---

## 已知限制

- **三态在 Windows 依赖管理员权限**:Windows 上非管理员只有两态(无 Active),是 ETW 的硬性要求;**macOS 借 nettop 的 entitlement,非 root 即三态**。
- **进程匹配是启发式的**:按可执行名 + 命令行子串匹配,「宁缺毋滥」;极端命名下可能漏判 / 误判,命中与排除都会写 debug 日志便于校准(`RUST_LOG=seecn=debug`)。
- **L7 逼近不是精确还原**:不解密 TLS 就只能用 L4 特征逼近「是否在请求 / 流式」;阈值需按实际环境校准(`src/model.rs` 的 `DOWN_RATE_ACTIVE_THRESHOLD` / `REQUEST_BURST_MIN`)。
- **ETW 连接四元组可能不精确**:不同 Windows 版本的 KernelNetwork schema 形态不一,地址 / 端口解析为「尽力而为」——设计上**「能判断三态」是硬目标,连接键精确度是次要目标**。
- **不持久化、不联网上报、不读取流量内容**:只读本机连接元数据,是一个纯粹的状态传感器。

---

## 许可

待定(TBD)。
