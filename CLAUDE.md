# seecn — see-claude-network

被动的网络状态传感器:常驻系统托盘 / 状态栏,用三态圆点(Offline 灰 / Idle 蓝 / Active 绿)实时表达本机每个 **Claude Code CLI** 会话与服务器的连接状态。**纯传感器**——只读连接元数据(进程 / 字节计数 / 连接存在性),绝不碰流量内容、不解密、不联网上报、不持久化。跨平台:Windows(ETW)+ macOS(nettop)。

## 架构一句话

平台无关核心(三态状态机 / Engine / JSON 数据契约 / flyout HTML 模板)+ `platform` trait 抽象出的平台 glue(进程发现 / 网络监控 / 托盘浮层):两平台**共享前者、只换后者**。权威设计见 `docs/DESIGN.md`(Windows)与 `docs/macos-port-design.md`(macOS)。

## 目录树

```
src/
  main.rs            入口:装日志 + 起后台线程 + 跑 tao event loop / tray
  model.rs           核心类型契约(NetEvent / Session / LinkState / 阈值常量),已锁定
  state.rs           三态判定纯函数(is_effective_activity / classify / overall)
  monitor.rs         Engine:单线程消费事件、独占连接表、按节拍评估三态
  tray.rs            托盘图标 + tooltip + 右键菜单 + flyout 显隐(tao + tray-icon)
  flyout/mod.rs      平台无关 flyout:FlyoutView trait + JSON 序列化(模板见 assets)
  platform/
    mod.rs           平台 trait(ProcScanner / NetMonitor / TcpSnapshot)+ cfg 分发 + monitor_label / log_base_dir
    windows/         ETW(etw.rs)/ netstat2(tcptable)/ sysinfo(proc)/ WebView2 flyout
    macos/           nettop 常驻流(net.rs)/ netstat2 / sysinfo / WKWebView flyout
assets/flyout.html   内嵌 flyout 模板(两平台共用,window.seecnRender(json) 渲染)
docs/                DESIGN.md(Win)/ macos-port-design.md(Mac)/ 调研笔记
packaging/macos/     Info.plist(.app bundle,CI 打包用)
scripts/             Windows 登录自启 安装 / 卸载(计划任务)
.github/workflows/   release.yml(打 v* tag → 各平台 CI 编译 → GitHub Release)
tools/               ntstat-probe.rs(否决 ntstat 直连的实测证据)/ flyout-demo(throwaway)
```

## 构建 / 校验

feature 互斥:默认 `windows-platform`;macOS 要关默认、启 `macos-platform`。

```sh
cargo build --release                                                  # Windows
cargo build --release --no-default-features --features macos-platform  # macOS(universal 见 CI)
cargo test                                                             # 平台无关单测
cargo clippy --no-default-features --features macos-platform -- -D warnings  # macOS lint
cargo fmt
```

## just recipe(justfile,shell=pwsh)

| recipe | 作用 |
|---|---|
| `build` / `release` | 编译 |
| `check` / `lint` / `fmt` | cargo check / clippy -D warnings / fmt |
| `ci` | fmt + check + lint + build(全量本地校验) |
| `run` / `run-admin` | 运行(非管理员两态 / 提权三态,带 debug 日志) |
| `install` / `uninstall` | Windows 登录自启 注册 / 移除(计划任务,弹 UAC) |

## 关键约束

- **复用层一行不改**:`model / state / monitor / flyout / tray / main` 跨平台共享,平台差异只塞进 `platform/<os>`;连 "ETW" 文案、日志路径都经 `platform::monitor_label` / `log_base_dir` 注入而非硬编码。
- **commit message 用英文**(Opus 4.8 长 CJK tool-call 已知 bug);代码 / 注释 / UI 的中文照旧。
- **三态阈值平台化**:nettop 字节口径含协议开销,keepalive 基线(~850-1300 B/s)远高于 Windows ETW(数十),`DOWN_RATE_ACTIVE_THRESHOLD` 按平台分(Win 256 / mac 2048,待真实 SSE 校准)。
- **发布**:bump `Cargo.toml` + `packaging/macos/Info.plist` 的 version → 打 `vX.Y.Z` tag → CI 自动出 Release(exe + macOS `.app` zip)→ 更新 `XuanLee-HEALER/homebrew-tap` 的 cask sha256。
