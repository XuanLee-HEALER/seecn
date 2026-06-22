//! seecn 入口:初始化、起线程、跑 tao event loop + tray(DESIGN §9.7)。
//!
//! see-claude-network:一个被动的网络状态传感器。
//!
//! 数据流(DESIGN §2):
//!   ProcScan ─┐
//!   ETW ──────┼──► EngineMsg channel(crossbeam,单消费者)──► Engine ──► proxy ──► 托盘
//!   (bridge)  ┘
//!
//! Engine 是 `EngineMsg` 的唯一消费者,conns 表无跨线程共享;唯一共享的是 claude_pids。
//! ETW 的 `NetMonitor::start` 契约要求 `Sender<NetEvent>`,因此用一根 net channel +
//! 一个 bridge 线程把 `NetEvent` 包成 `EngineMsg::Net` 转投,从而保持 Engine 单入口。

// 无控制台窗口(DESIGN §22.1):release 走 GUI 子系统,无控制台窗口;debug 保留控制台便于看日志。
// 必须是 crate 级 inner 属性(#![...]),且写在任何 item 之前,否则编译失败。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod flyout;
mod model;
mod monitor;
mod platform;
mod state;
mod tray;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;

use crossbeam_channel::unbounded;
use tracing::{info, warn};

use crate::model::{Privilege, PROC_SCAN_INTERVAL};
use crate::monitor::{Engine, EngineMsg};
use crate::tray::UserEvent;

fn main() {
    // 1. 初始化 tracing:文件日志(info+,用户数据目录)+ debug 下兼写控制台(DESIGN §22.2)。
    //    日志目录:%LOCALAPPDATA%\seecn\logs\,无 LOCALAPPDATA 时回退当前目录;best-effort 建目录。
    let log_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("seecn")
        .join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    // 同步 RollingFileAppender:按天滚动写 seecn.log。它实现了 MakeWriter,可直接 with_writer,
    //   无需 non_blocking + WorkerGuard,从而 process::exit 时不丢尾部日志(DESIGN §22.2)。
    let file_appender = tracing_appender::rolling::daily(&log_dir, "seecn.log");

    // filter:默认 info,仍尊重 RUST_LOG 覆盖。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // debug:控制台 + 文件双写(MakeWriterExt::and);release:纯文件(无控制台)。
    //   两分支都 with_ansi(false):文件不要 ANSI 颜色码。
    #[cfg(debug_assertions)]
    {
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(file_appender.and(std::io::stdout))
            .init();
    }
    #[cfg(not(debug_assertions))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(file_appender)
            .init();
    }

    info!("seecn 启动:see-claude-network 被动网络状态传感器");

    // 1.5 安装 panic hook:在起任何线程之前装好,确保后台线程(ProcScan/ETW/bridge/Engine)
    //     的静默 panic 也能在日志里看到(后台线程默认 panic 后静默终止,易被忽略,DESIGN §19.2)。
    //     回调里打印线程名 + panic 的 payload(向下转型为 &str/String)+ location。
    std::panic::set_hook(Box::new(|info| {
        // 线程名:无名线程用占位串。
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");

        // payload 向下转型:panic! 的载荷常见为 &'static str 或 String。
        let payload = info.payload();
        let msg: &str = if let Some(s) = payload.downcast_ref::<&str>() {
            s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "<non-string panic payload>"
        };

        // location:panic 发生的文件:行:列(可能缺失)。
        match info.location() {
            Some(loc) => tracing::error!(
                thread = thread_name,
                location = %loc,
                "线程 panic:{msg}"
            ),
            None => tracing::error!(thread = thread_name, "线程 panic(无 location):{msg}"),
        }
    }));

    // 2. 检测权限,日志提示三态 / 两态。
    let privilege = platform::current_privilege();
    match privilege {
        Privilege::Elevated => info!(
            "{} 监听可用,启用三态监控(Offline/Idle/Active)",
            platform::monitor_label()
        ),
        Privilege::Standard => warn!(
            "{} 监听不可用,降级为两态(Offline/Idle)模式",
            platform::monitor_label()
        ),
    }

    // 3. 共享状态与 channel。
    //    - claude_pids:跨线程共享的过滤集合,ETW 回调据此过滤,Engine 写回。
    //    - eng channel:所有事件源(ProcScan / ETW-bridge / Quit)汇入,Engine 单消费。
    //    - net channel:ETW 专用,产 NetEvent;bridge 线程转成 EngineMsg::Net 投 eng channel。
    let claude_pids: Arc<RwLock<HashSet<u32>>> = Arc::new(RwLock::new(HashSet::new()));
    let (eng_tx, eng_rx) = unbounded::<EngineMsg>();
    let (net_tx, net_rx) = unbounded::<model::NetEvent>();

    // 4. 跑 run_tray:在 event loop 就绪后,通过 spawn_engine 闭包起后台线程。
    //    spawn_engine 拿到 proxy(可 Clone + Send),交给 Engine 线程用于推送托盘更新。
    //    所有需要被 move 进闭包的资源在此先克隆/转移好。
    let claude_pids_eng = claude_pids.clone();
    let claude_pids_etw = claude_pids.clone();

    tray::run_tray(privilege, move |proxy| {
        // —— ProcScan 线程 ——
        // 每 PROC_SCAN_INTERVAL 扫描一次,把结果发 EngineMsg::Procs。
        // Engine 在 refresh_procs 里写回 claude_pids,并对新增 pid 调 snapshot 补连接。
        {
            let eng_tx = eng_tx.clone();
            thread::Builder::new()
                .name("seecn-procscan".into())
                .spawn(move || {
                    let mut scanner = platform::new_proc_scanner();
                    loop {
                        let procs = scanner.scan();
                        // 发送失败说明 Engine 已退出,扫描线程随之结束。
                        if eng_tx.send(EngineMsg::Procs(procs)).is_err() {
                            break;
                        }
                        thread::sleep(PROC_SCAN_INTERVAL);
                    }
                })
                .expect("无法启动 ProcScan 线程");
        }

        // —— ETW-bridge 线程 ——
        // 把 net channel 的 NetEvent 包成 EngineMsg::Net 转投 eng channel,
        // 让 Engine 保持单一入口(EngineMsg)。ETW 通过 net_tx 推送 NetEvent。
        {
            let eng_tx = eng_tx.clone();
            thread::Builder::new()
                .name("seecn-net-bridge".into())
                .spawn(move || {
                    // net_tx 仍被 ETW 线程持有;只要还有发送端,recv 会一直阻塞等待。
                    while let Ok(ev) = net_rx.recv() {
                        if eng_tx.send(EngineMsg::Net(ev)).is_err() {
                            break;
                        }
                    }
                })
                .expect("无法启动 net-bridge 线程");
        }

        // —— ETW 线程 ——
        // net_monitor.start(...) 在两态模式(非管理员)或 provider 不可用时返回 Err,
        // 此时仅 log 警告并退出本线程:proc + snapshot 仍能给出 Offline/Idle 两态。
        {
            thread::Builder::new()
                .name("seecn-etw".into())
                .spawn(move || {
                    let mut net_monitor = platform::new_net_monitor();
                    match net_monitor.start(claude_pids_etw, net_tx) {
                        Ok(()) => info!("{} 监听已启动(三态模式)", platform::monitor_label()),
                        Err(e) => warn!(
                            "{} 监听启动失败,进入两态(Offline/Idle)模式:{:#}",
                            platform::monitor_label(),
                            e
                        ),
                    }

                    // start() 若是阻塞式实现,返回时 trace 已结束;若是非阻塞(内部另起线程),
                    // 本线程在此自然退出,ETW 后台线程随进程存活。两种语义均无需额外处理。
                })
                .expect("无法启动 ETW 线程");
        }

        // —— Engine 线程 ——
        // 单线程串行消费 EngineMsg,独占 conns 表;每次评估通过 proxy 推送 TrayUpdate。
        {
            thread::Builder::new()
                .name("seecn-engine".into())
                .spawn(move || {
                    let snapshot = platform::new_tcp_snapshot();
                    let engine = Engine::new(claude_pids_eng, snapshot);
                    engine.run(eng_rx, move |sessions, overall| {
                        // send_event 失败说明 event loop 已退出(Quit),忽略即可。
                        proxy
                            .send_event(UserEvent::TrayUpdate { sessions, overall })
                            .ok();
                    });
                })
                .expect("无法启动 Engine 线程");
        }

        // 注意:eng_tx 的最后一个克隆在本闭包结束时 drop。Engine 线程仍持有 eng_rx,
        // ProcScan / net-bridge 线程各自持有 eng_tx 克隆,因此 channel 不会过早关闭。
    });
    // run_tray 永不返回(-> !),进程随 event loop 退出。
}
