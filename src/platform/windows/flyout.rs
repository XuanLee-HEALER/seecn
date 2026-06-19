//! Windows flyout 窗口 glue:tao 无边框窗口 + wry WebView(WebView2)。
//!
//! DESIGN §18.1 / §18.3:undecorated / always-on-top / 不进任务栏 / 不可缩放 / 背景透明的
//! 小窗,内挂 wry WebView 加载 `FLYOUT_HTML`。窗口与 webview 常驻(显隐而非销毁),
//! 全程 UI 线程独占(非 Send)。
//!
//! 实测依赖:tao 0.34.8 + wry 0.53.5(二者都用 raw-window-handle 0.6;tao `Window`
//! 实现 `HasWindowHandle`,可直接传给 `WebViewBuilder::build`)。

use crate::flyout::{FlyoutView, FLYOUT_HTML};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Window, WindowBuilder, WindowId};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use wry::{WebView, WebViewBuilder};

/// flyout 窗口尺寸(逻辑像素;HTML 卡片宽 300,起步高 220,Windows 下 webview 随窗口自适应)。
/// `FLYOUT_W`/`FLYOUT_H` 仅用于 `new()` 的初始 `with_inner_size`;后续高度由 `resize_for`
/// 按 session 数自适应(DESIGN §21.2),定位改用窗口实际 `outer_size`,不再依赖固定常量。
const FLYOUT_W: u32 = 300;
const FLYOUT_H: u32 = 220;
/// 自适应高度参数(逻辑像素,与 assets/flyout.html 实测对齐,可 e2e 微调,DESIGN §21.2)。
/// header(标题 + 整体药丸 + 计数)的逻辑高度。
const HEADER_H: f64 = 46.0;
/// 每个 session 行的逻辑高度。
const ROW_H: f64 = 50.0;
/// 行数封顶:≤3 个 session 时窗口正好容纳、不出滚动条;>3 则窗口封顶在 3 行高,
/// 列表内部滚动(flyout.html 的 `ul#list{overflow-y:auto}`)。
const MAX_ROWS: usize = 3;
/// 屏幕右下角留边距(像素)。
const MARGIN: i32 = 12;
/// 任务栏高度的保守预留(像素):MonitorHandle 只给整屏分辨率,拿不到工作区,
/// 故从屏幕底部上抬这个量,确保浮层落在任务栏之上。精确定位留待 e2e 调。
const TASKBAR_RESERVE: i32 = 48;
/// 显示后的 light-dismiss 宽限期(DESIGN §20.2):刚 `show` 时前台窗口尚未稳定
/// (webview 子窗夺焦、前台切换有延迟),这段时间内 `poll_autohide` 跳过,
/// 避免「刚弹出就被前台轮询误判为点外部」而立即收起。
const DISMISS_GRACE: Duration = Duration::from_millis(600);

/// Windows flyout:持有 tao 窗口 + wry webview,维护可见性标志。
///
/// **字段顺序即 drop 顺序**:`webview` 在 `window` 之前声明,保证 webview 先于宿主窗口析构
/// (wry 推荐 webview 不晚于其宿主窗口存活)。
pub struct WinFlyout {
    webview: WebView,
    window: Window,
    visible: bool,
    /// 顶层窗口句柄(构造时从 tao 窗口的 raw-window-handle 取出),以 `isize` 存储,
    /// 与 `GetForegroundWindow()` 的返回值(转 `isize`)比较实现前台轮询(DESIGN §20.2)。
    hwnd: isize,
    /// 最近一次 `show` 的时刻;`poll_autohide` 用它判断是否过了 `DISMISS_GRACE` 宽限期。
    /// `None` 表示从未显示过(或已隐藏后清空语义上不需要,保留最后一次即可)。
    shown_at: Option<Instant>,
}

impl WinFlyout {
    /// 在传入的 event loop target 上构造 flyout 窗口与 webview(初始隐藏)。
    ///
    /// 窗口属性(DESIGN §18.1):无边框、不可缩放、初始不可见、置顶、背景透明、不进任务栏。
    /// webview 用 `with_html(FLYOUT_HTML)` 加载内嵌模板。
    pub fn new<T: 'static>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_always_on_top(true)
            .with_transparent(true)
            .with_skip_taskbar(true)
            .with_inner_size(PhysicalSize::new(FLYOUT_W, FLYOUT_H))
            .build(target)?;

        // 取顶层 HWND(DESIGN §20.2):tao `Window` 实现 rwh 0.6 的 `HasWindowHandle`,
        // `window_handle()?.as_raw()` 得到 `RawWindowHandle`,Windows 下是
        // `RawWindowHandle::Win32(Win32WindowHandle{ hwnd: NonZeroIsize, .. })`。
        // 取不到(非 Win32 或出错)视为构造失败返回 Err(上层 new_flyout 已对失败降级)。
        let hwnd = match window.window_handle() {
            Ok(handle) => match handle.as_raw() {
                RawWindowHandle::Win32(win32) => win32.hwnd.get(),
                other => {
                    anyhow::bail!("flyout 窗口不是 Win32 句柄,无法取 HWND:{other:?}");
                }
            },
            Err(e) => {
                anyhow::bail!("flyout 取 window_handle 失败:{e}");
            }
        };

        // tao `Window` 实现 raw-window-handle 0.6 的 `HasWindowHandle`,
        // 直接传给 wry 的 build;build 的借用在返回后即结束(WebView 无生命周期参数,
        // 内部已捕获句柄),故 window 可与 webview 同存一个结构体。
        let webview = WebViewBuilder::new()
            .with_transparent(true)
            .with_html(FLYOUT_HTML)
            .build(&window)?;

        Ok(Self {
            webview,
            window,
            visible: false,
            hwnd,
            shown_at: None,
        })
    }

    /// flyout 窗口的 id,供事件循环匹配 `WindowEvent`(失焦自动收起)。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 把窗口定位到主显示器右下角(任务栏之上,留边距)。
    ///
    /// MonitorHandle 只提供整屏分辨率与左上角坐标,不含工作区,故用
    /// `TASKBAR_RESERVE` 保守上抬;精确位置留待 e2e 调(DESIGN §18.3)。
    ///
    /// 窗口尺寸用 `outer_size()`(物理像素)实时取值,而非固定 `FLYOUT_W/FLYOUT_H`,
    /// 这样 `resize_for` 改高之后锚点仍正确(DESIGN §21.2)。注意 `outer_size` 已是
    /// 物理像素,直接和同为物理像素的 monitor.size()/position() 计算,不再乘 scale。
    fn position_bottom_right(&self) {
        let Some(monitor) = self.window.primary_monitor() else {
            // 拿不到主显示器(极少见)就不强行定位,交给系统默认位置。
            return;
        };
        let scale = monitor.scale_factor();
        let size: PhysicalSize<u32> = monitor.size();
        let origin: PhysicalPosition<i32> = monitor.position();

        // 窗口当前实际外尺寸(物理像素)。set_inner_size 后这里会反映最新高度。
        let win: PhysicalSize<u32> = self.window.outer_size();
        let win_w = win.width as i32;
        let win_h = win.height as i32;
        // 边距/任务栏预留是逻辑像素,× scale 折成物理像素再算。
        let margin = (MARGIN as f64 * scale) as i32;
        let taskbar = (TASKBAR_RESERVE as f64 * scale) as i32;

        let x = origin.x + size.width as i32 - win_w - margin;
        let y = origin.y + size.height as i32 - win_h - taskbar;
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }

    /// 内部显示逻辑:定位 → 可见 → 抢焦点 → 立即用最新 json 渲染。
    /// 幂等:不论当前是否可见都完整执行,供 trait `show` 复用(DESIGN §19.1)。
    fn show_inner(&mut self, json: &str) {
        self.position_bottom_right();
        self.window.set_visible(true);
        self.window.set_focus();
        self.visible = true;
        // 记录显示时刻:poll_autohide 据此跳过 DISMISS_GRACE 宽限期(DESIGN §20.2)。
        self.shown_at = Some(Instant::now());
        self.render(json);
    }

    /// 调 webview 重绘:`window.seecnRender(<json>)`(json 已是完整 JSON 对象串)。
    fn render(&self, json: &str) {
        let script = format!("window.seecnRender({json})");
        if let Err(e) = self.webview.evaluate_script(&script) {
            tracing::warn!("flyout evaluate_script 失败: {e}");
        }
    }
}

impl FlyoutView for WinFlyout {
    /// 左键托盘:幂等显示并渲染(DESIGN §19.1)。
    /// 不论当前是否可见都执行 position→set_visible(true)→set_focus→render,
    /// 即便一次物理点击产生两次 TrayClick,也只是重复显示,消除「双触发又显又隐」的闪退。
    fn show(&mut self, json: &str) {
        tracing::debug!("flyout show");
        self.show_inner(json);
    }

    /// 隐藏(保留窗口与 webview,不销毁)。
    fn hide(&mut self) {
        tracing::debug!("flyout hide");
        self.window.set_visible(false);
        self.visible = false;
    }

    /// 推送最新数据:仅在可见时重绘(隐藏时刷新无意义,省一次 IPC)。
    fn update(&mut self, json: &str) {
        if self.visible {
            self.render(json);
        }
    }

    /// 按 session 数自适应窗口高度(DESIGN §21.2)。
    ///
    /// 行数 clamp 到 [1, MAX_ROWS],逻辑高度 = HEADER_H + 行数 * ROW_H;
    /// `set_inner_size(LogicalSize)` 由 tao 按 DPI 折算物理像素,随后 `position_bottom_right`
    /// 用更新后的 `outer_size` 重锚右下角。session 少则窗口矮、不留空白,多则封顶滚动。
    fn resize_for(&mut self, session_count: usize) {
        // 等价于 max(1).min(MAX_ROWS);clippy::manual_clamp 要求改用 clamp
        // (MAX_ROWS=8 > 1,不会 panic)。
        let rows = session_count.clamp(1, MAX_ROWS);
        let h = HEADER_H + rows as f64 * ROW_H;
        // LogicalSize:tao 自动按 DPI 折算成物理像素;宽度沿用 FLYOUT_W。
        self.window
            .set_inner_size(LogicalSize::new(FLYOUT_W as f64, h));
        // 高度变了,重锚右下角(position_bottom_right 用 outer_size 取最新尺寸)。
        self.position_bottom_right();
    }

    fn is_visible(&self) -> bool {
        self.visible
    }

    /// 前台轮询 light-dismiss(DESIGN §20.2):
    /// 1. 未显示 → 直接返回;
    /// 2. 距 `show` 不足 `DISMISS_GRACE` → 返回(刚弹出,前台未稳,跳过一轮);
    /// 3. 否则取 `GetForegroundWindow()`,若 != 本 flyout 顶层窗口 → 说明前台已切到
    ///    别的 app/桌面/任务栏(用户点了外部)→ `hide()`。
    ///
    /// 焦点事件不可靠(webview 子窗会瞬间夺走窗口焦点 → tao `Focused(false)`),
    /// 而顶层前台窗口仍是 flyout,故改用前台窗口比较实现「点外部关闭」。
    fn poll_autohide(&mut self) {
        if !self.visible {
            return;
        }
        // 刚弹出的宽限期内不收起,避免前台尚未稳定时被误判。
        if let Some(shown_at) = self.shown_at {
            if shown_at.elapsed() < DISMISS_GRACE {
                return;
            }
        }
        // SAFETY:GetForegroundWindow 无参、不取所有权、不会失败(返回值可能为 NULL,
        // 此时也只是与本窗口句柄不等而触发 hide,语义正确)。返回的 HWND 仅用于数值比较。
        let foreground: HWND = unsafe { GetForegroundWindow() };
        if foreground as isize != self.hwnd {
            self.hide();
        }
    }
}
