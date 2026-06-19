//! 托盘 UI:图标生成、tooltip、菜单 + flyout 富显示面板(基于 tao + tray-icon + wry,
//! DESIGN §9.6 / §17 / §18)。
//!
//! 实测依赖版本:tao 0.34.x + tray-icon 0.24.x + wry 0.53.x(菜单事件经 muda 的
//! `MenuEvent::set_event_handler`、托盘左键经 `TrayIconEvent::set_event_handler` 各自
//! 转发到 tao 的 `EventLoopProxy`,统一在主线程事件循环里消费,避免在 run 闭包里轮询)。
//!
//! tooltip 退化为**单行紧凑摘要**(§17,永不截断);逐 session 明细交给 flyout(§18):
//! 左键托盘弹出无边框 webview 小窗,失焦自动收起。flyout 全程 UI 线程独占(非 Send),
//! 只在 run 闭包内创建与使用。

use crate::flyout::{sessions_to_json, FlyoutView};
use crate::model::{LinkState, Privilege, Session};
use crate::platform;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// 主线程通过 EventLoop 的 UserEvent 接收的更新。
pub enum UserEvent {
    TrayUpdate {
        sessions: Vec<Session>,
        overall: LinkState,
    },
    /// 托盘左键单击:切换 flyout 显隐(DESIGN §18.3)。
    TrayClick,
    MenuQuit,
}

/// 图标边长(像素)。32x32 在大多数 DPI 下都足够清晰。
const ICON_SIZE: u32 = 32;

/// 按状态返回圆点颜色(RGB)。
/// Offline→灰、Idle→蓝、Active→绿(见 DESIGN §9.6)。
fn color_for(state: LinkState) -> [u8; 3] {
    match state {
        LinkState::Offline => [0x7A, 0x7A, 0x7A], // #7A7A7A 灰
        LinkState::Idle => [0x3B, 0x82, 0xF6],    // #3B82F6 蓝
        LinkState::Active => [0x22, 0xC5, 0x5E],  // #22C55E 绿
    }
}

/// 按状态生成纯色 RGBA 图标(避免打包 .ico 资源)。
/// 画一个居中实心圆点:圆内不透明填色,圆外完全透明。
///
/// 不会 panic:`Icon::from_rgba` 对 32x32 的合法缓冲区必然成功;
/// 万一构造失败也只 `expect`(尺寸/缓冲区是常量,逻辑上不可能出错)。
fn icon_for(state: LinkState) -> Icon {
    let [r, g, b] = color_for(state);
    let size = ICON_SIZE;
    let mut rgba = vec![0u8; (size * size * 4) as usize];

    // 圆心与半径:留出 ~1px 边缘抗锯齿余量。
    let center = (size as f32 - 1.0) / 2.0;
    let radius = (size as f32) / 2.0 - 1.5;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            // 简单的 1px 软边:dist <= radius 内全实心,radius..radius+1 线性淡出。
            let alpha = if dist <= radius {
                255.0
            } else if dist <= radius + 1.0 {
                255.0 * (radius + 1.0 - dist)
            } else {
                0.0
            };

            let idx = ((y * size + x) * 4) as usize;
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = alpha as u8;
        }
    }

    Icon::from_rgba(rgba, size, size).expect("32x32 RGBA 缓冲区构造图标不应失败")
}

/// 把 sessions 渲染成**单行**紧凑摘要(DESIGN §17)。
///
/// Windows 托盘 tooltip(`NOTIFYICONDATA.szTip`)有长度上限且会从中间截断多行文本,
/// 故 tooltip 只承载一行必短、永不截断的摘要;逐 session 明细全部交给 flyout(§18)。
///
/// 格式示例:
/// - 有会话:`cc monitor — 1 active / 2 idle / 0 offline`
/// - 无会话:`cc monitor — no Claude sessions`
fn render_tooltip(sessions: &[Session]) -> String {
    if sessions.is_empty() {
        return "cc monitor — no Claude sessions".to_string();
    }

    let mut active = 0usize;
    let mut idle = 0usize;
    let mut offline = 0usize;
    for s in sessions {
        match s.state {
            LinkState::Active => active += 1,
            LinkState::Idle => idle += 1,
            LinkState::Offline => offline += 1,
        }
    }
    // 始终 ≤ 60 字符,永不触发 Windows 的中段截断。
    format!("cc monitor — {active} active / {idle} idle / {offline} offline")
}

/// 构建托盘并运行 event loop(阻塞,直到退出)。
///
/// `spawn_engine`: 在 event loop 启动后、拿到 proxy 后调用,
/// 用于把 proxy 交给 Engine 侧起线程。
///
/// 流程(DESIGN §9.6):
/// 1. `EventLoopBuilder::<UserEvent>::with_user_event().build()`。
/// 2. 建初始 `TrayIcon`(Offline 图标 + 启动 tooltip + 右键 Quit 菜单);
///    `Standard` 权限时 tooltip 追加两态模式提示。
/// 3. `create_proxy()` → `spawn_engine(proxy)`。
/// 4. `run`:`UserEvent(TrayUpdate)` → `set_icon` + `set_tooltip`;
///    菜单 Quit(经 `MenuEvent` 转发的 `UserEvent::MenuQuit`)→ `ControlFlow::Exit`。
pub fn run_tray(
    privilege: Privilege,
    spawn_engine: impl FnOnce(EventLoopProxy<UserEvent>) + Send + 'static,
) -> ! {
    // 1. 带用户事件类型的事件循环。必须在主线程构建。
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // 2. 右键菜单:仅一个 Quit 项。记下其 id 以便事件匹配。
    let menu = Menu::new();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();
    // append 可能返回 Result;忽略错误(单项菜单构建失败不影响主流程,且实际不会失败)。
    let _ = menu.append(&quit_item);

    // 初始 tooltip:启动中;非管理员追加两态模式提示。
    let mut startup_tooltip = String::from("cc monitor: starting…");
    if privilege == Privilege::Standard {
        startup_tooltip.push_str(" (no admin: 2-state mode)");
    }

    // 建托盘:初始 Offline 灰图标 + 启动 tooltip + Quit 菜单。
    // TrayIcon 句柄必须在 event loop 存活期间保持存活,故 move 进 run 闭包。
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(&startup_tooltip)
        .with_icon(icon_for(LinkState::Offline))
        .build()
        .expect("构建托盘图标失败");

    // 3a. 在 event loop 就绪后、run 之前,用 &event_loop(derefs 到 EventLoopWindowTarget)
    //     构造 flyout(tao 窗口 + wry webview)。UI 线程独占,稍后 move 进 run 闭包。
    //     创建失败不致命:仅 log 警告,退化为「只有托盘 + 单行 tooltip」。
    let mut flyout = match platform::new_flyout(&event_loop) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("flyout 创建失败,仅托盘模式:{:#}", e);
            None
        }
    };
    // 记下 flyout 窗口 id,用于在事件循环里匹配「失焦自动收起」。
    let flyout_window_id = flyout.as_ref().map(|f| f.window_id());

    // 3b. 拿 proxy。一份给 Engine 侧线程;一份给菜单事件处理器;一份给托盘左键处理器。
    let proxy = event_loop.create_proxy();

    // 菜单点击经 muda 的全局 handler 投递,统一转成 UserEvent::MenuQuit
    // 送回主线程事件循环(避免在 run 里轮询 MenuEvent::receiver)。
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == quit_id {
            // 事件循环已退出时 send_event 会 Err,忽略即可。
            let _ = menu_proxy.send_event(UserEvent::MenuQuit);
        }
    }));

    // 托盘左键单击经 tray-icon 的全局 handler 投递,转成 UserEvent::TrayClick
    // 送回主线程(与菜单同样的「全局 handler → proxy」模式,DESIGN §18.3)。
    // 只在左键 Up 时触发一次,避免 Down/Up 双触发。
    let click_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let _ = click_proxy.send_event(UserEvent::TrayClick);
        }
    }));

    // event loop 已就绪,交出 proxy 给 Engine 侧起线程。
    spawn_engine(proxy);

    // 缓存最近一次序列化好的 flyout JSON:托盘左键打开时直接用它即时渲染,
    // 不必等下一个评估节拍(DESIGN §18.3)。初值为空数据。
    let mut last_json = sessions_to_json(&[], LinkState::Offline);
    // 缓存最近一次 session 数:用于 flyout 按内容自适应高度(DESIGN §21.2)。
    // 托盘左键打开 / TrayUpdate 可见刷新时据此调窗高。初值 0。
    let mut last_count: usize = 0;

    // 4. 跑事件循环。tao 0.34:run 接收 FnMut(Event, &EventLoopWindowTarget, &mut ControlFlow)。
    //    keep `tray` / `flyout` alive by moving them into the closure。
    event_loop.run(move |event, _target, control_flow| {
        // 平时等待事件即可,不需要忙轮询。
        *control_flow = ControlFlow::Wait;

        match event {
            Event::UserEvent(UserEvent::TrayUpdate { sessions, overall }) => {
                // ① 算并缓存最新 JSON 与 session 数,供下次托盘打开即时渲染 / 自适应高度。
                last_json = sessions_to_json(&sessions, overall);
                last_count = sessions.len();

                // ② 更新图标颜色 + 单行摘要 tooltip。错误仅记日志,不致命。
                if let Err(e) = tray.set_icon(Some(icon_for(overall))) {
                    tracing::warn!("set_icon 失败: {e}");
                }
                if let Err(e) = tray.set_tooltip(Some(render_tooltip(&sessions))) {
                    tracing::warn!("set_tooltip 失败: {e}");
                }

                // ③ flyout 可见时随评估节拍实时刷新,并按最新 session 数自适应窗高(DESIGN §21.2)。
                if let Some(f) = flyout.as_mut() {
                    if f.is_visible() {
                        f.update(&last_json);
                        f.resize_for(last_count);
                    }
                }

                // ④ 前台轮询 light-dismiss(DESIGN §20):借 Engine ~500ms 评估节拍,
                //    周期性检查前台窗口是否已不是 flyout(点了外部)→ 自动收起。
                //    焦点事件不可靠(见下方 Focused 分支注释),故改用此轮询。
                if let Some(f) = flyout.as_mut() {
                    f.poll_autohide();
                }
            }
            Event::UserEvent(UserEvent::TrayClick) => {
                // 托盘左键:幂等显示 flyout(不再 toggle),用缓存 JSON 即时渲染。
                // 即便一次物理点击产生两次 TrayClick,也只是重复显示,消除双触发闪退(DESIGN §19.1)。
                tracing::debug!("托盘左键 → 显示 flyout");
                if let Some(f) = flyout.as_mut() {
                    f.show(&last_json);
                    // 按当前 session 数自适应窗高(DESIGN §21.2):show 已定位,
                    // resize_for 改高后会再 position_bottom_right 重锚,锚点正确。
                    f.resize_for(last_count);
                }
            }
            Event::UserEvent(UserEvent::MenuQuit) => {
                tracing::info!("收到 Quit,退出事件循环");
                *control_flow = ControlFlow::Exit;
            }
            // flyout 窗口焦点变化:仅记录用于观测,**不再据此收起**(DESIGN §20.1)。
            // 真因:show 后 wry 的 WebView2 子窗会瞬间夺走窗口焦点 → tao 立即上报
            // Focused(false),若据此 hide 会「刚弹出就闪退」;且子窗夺焦后宿主已失焦,
            // 后续点外部也不再产生失焦事件,焦点事件无法实现 light-dismiss。
            // 故「点外部关闭」改由 TrayUpdate 节拍里的 poll_autohide(前台轮询)实现。
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Focused(focused),
                ..
            } if Some(window_id) == flyout_window_id => {
                tracing::debug!(focused, "flyout 焦点变化");
            }
            _ => {}
        }
    });
}
