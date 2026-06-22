//! macOS flyout 窗口 glue:tao 无边框窗口 + wry WebView(WKWebView),对应 windows/flyout.rs。
//!
//! 无边框 / always-on-top / 透明 / 初始隐藏的小窗,内挂 wry WebView 加载 `FLYOUT_HTML`。
//! 窗口与 webview 常驻(显隐而非销毁),全程 UI 线程独占(非 Send)。
//!
//! 与 Windows 的三处差异(都用 tao 内建 API,在本模块内解决,不碰复用层):
//! 1. 定位:**右上角**(菜单栏下方),对称 Windows 的右下角——各自系统状态区所在的屏幕角落。
//! 2. light-dismiss:用 `Window::is_focused()` 查询(WKWebView 是 NSView、不夺窗口焦点,故焦点
//!    可靠);Windows 因 WebView2 子 HWND 夺焦才需 `GetForegroundWindow` 轮询。
//! 3. activation policy:`new` 里设 `Accessory`(不占 Dock、不抢菜单栏),状态栏 app 标准。

use crate::flyout::{FlyoutView, FLYOUT_HTML};
use crate::model::TrayAnchor;
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use tao::event_loop::EventLoopWindowTarget;
use tao::platform::macos::{ActivationPolicy, EventLoopWindowTargetExtMacOS};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

/// flyout 初始尺寸(逻辑像素;宽 300,起步高 220,后续高度由 resize_for 自适应)。
const FLYOUT_W: f64 = 300.0;
const FLYOUT_H: f64 = 220.0;
/// 自适应高度参数(逻辑像素,与 assets/flyout.html 对齐,可 e2e 微调,同 Windows)。
const HEADER_H: f64 = 46.0;
const ROW_H: f64 = 50.0;
const MAX_ROWS: usize = 3;
/// 屏幕右缘留边(逻辑像素)。
const MARGIN: f64 = 12.0;
/// 菜单栏高度的保守预留(逻辑像素):primary_monitor 只给整屏、不含菜单栏工作区,
/// 故从屏幕顶部下移这个量,确保浮层落在菜单栏之下。精确值待 e2e 微调。
const MENUBAR_RESERVE: f64 = 30.0;
/// 显示后的 light-dismiss 宽限期:刚 show 时焦点未稳,这段时间内 poll_autohide 跳过,
/// 避免「刚弹出就被判失焦而立即收起」。
const DISMISS_GRACE: Duration = Duration::from_millis(600);

/// macOS flyout:持有 tao 窗口 + wry webview,维护可见性。
///
/// **字段顺序即 drop 顺序**:`webview` 在 `window` 之前声明,保证 webview 先于宿主窗口析构。
pub struct MacFlyout {
    webview: WebView,
    window: Window,
    visible: bool,
    /// 最近一次 show 的时刻,poll_autohide 据此跳过 DISMISS_GRACE 宽限期。
    shown_at: Option<Instant>,
    /// 最近一次托盘点击的图标锚点(定位到图标下方);None 时回退右上角。
    anchor: Option<TrayAnchor>,
}

impl MacFlyout {
    /// 在 event loop target 上构造 flyout(初始隐藏)。同时把 app 设为 Accessory(状态栏 app)。
    pub fn new<T: 'static>(target: &EventLoopWindowTarget<T>) -> anyhow::Result<Self> {
        // 状态栏 app:不占 Dock、不抢菜单栏。在 flyout 模块内设,不碰复用层 main/tray。
        target.set_activation_policy_at_runtime(ActivationPolicy::Accessory);

        let window = WindowBuilder::new()
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_always_on_top(true)
            .with_transparent(true)
            .with_inner_size(LogicalSize::new(FLYOUT_W, FLYOUT_H))
            .build(target)
            .map_err(|e| anyhow::anyhow!("创建 flyout 窗口失败: {e}"))?;

        let webview = WebViewBuilder::new()
            .with_transparent(true)
            .with_html(FLYOUT_HTML)
            .build(&window)
            .map_err(|e| anyhow::anyhow!("创建 flyout webview 失败: {e}"))?;

        Ok(Self {
            webview,
            window,
            visible: false,
            shown_at: None,
            anchor: None,
        })
    }

    /// flyout 窗口 id,供事件循环匹配 WindowEvent。
    pub fn window_id(&self) -> WindowId {
        self.window.id()
    }

    /// 定位窗口:有图标锚点 → 锚到图标正下方(x 居中、clamp 进屏);否则回退右上角
    /// (对称 Windows 右下角)。窗口尺寸用 `outer_size()` 实时取值,resize_for 改高后重锚仍正确。
    fn position(&self) {
        let Some(monitor) = self.window.primary_monitor() else {
            return;
        };
        let scale = monitor.scale_factor();
        let size: PhysicalSize<u32> = monitor.size();
        let origin: PhysicalPosition<i32> = monitor.position();
        let win: PhysicalSize<u32> = self.window.outer_size();
        let margin = (MARGIN * scale) as i32;

        let (x, y) = match self.anchor {
            // 图标正下方:x 让 flyout 中心对齐图标中心、clamp 到屏内;y 落图标底部下方一点。
            Some(a) => {
                let cx = a.x + a.width / 2.0;
                let want_x = (cx - win.width as f64 / 2.0) as i32;
                let min_x = origin.x + margin;
                let max_x = origin.x + size.width as i32 - win.width as i32 - margin;
                let x = want_x.clamp(min_x, max_x);
                let y = (a.y + a.height) as i32 + (2.0 * scale) as i32;
                (x, y)
            }
            // 无锚点:右上角(菜单栏之下)。
            None => {
                let menubar = (MENUBAR_RESERVE * scale) as i32;
                let x = origin.x + size.width as i32 - win.width as i32 - margin;
                let y = origin.y + menubar;
                (x, y)
            }
        };
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }

    /// 内部显示:定位 → 可见 → 抢焦点 → 用最新 json 渲染。幂等(供 trait show 复用)。
    fn show_inner(&mut self, json: &str) {
        self.position();
        self.window.set_visible(true);
        self.window.set_focus();
        self.visible = true;
        self.shown_at = Some(Instant::now());
        self.render(json);
    }

    /// 调 webview 重绘:`window.seecnRender(<json>)`。
    fn render(&self, json: &str) {
        let script = format!("window.seecnRender({json})");
        if let Err(e) = self.webview.evaluate_script(&script) {
            tracing::warn!("flyout evaluate_script 失败: {e}");
        }
    }
}

impl FlyoutView for MacFlyout {
    fn show(&mut self, json: &str) {
        tracing::debug!("flyout show");
        self.show_inner(json);
    }

    fn hide(&mut self) {
        tracing::debug!("flyout hide");
        self.window.set_visible(false);
        self.visible = false;
    }

    fn update(&mut self, json: &str) {
        if self.visible {
            self.render(json);
        }
    }

    fn resize_for(&mut self, session_count: usize) {
        let rows = session_count.clamp(1, MAX_ROWS);
        let h = HEADER_H + rows as f64 * ROW_H;
        self.window.set_inner_size(LogicalSize::new(FLYOUT_W, h));
        self.position();
    }

    fn is_visible(&self) -> bool {
        self.visible
    }

    /// light-dismiss:可见且过了宽限期后,若 flyout 已失焦(用户点了外部)→ 收起。
    ///
    /// macOS 上 WKWebView 是嵌入的 NSView、不会夺走宿主窗口焦点,故 `is_focused()`
    /// 可靠反映「flyout 是否仍是用户焦点」,无需 Windows 那种前台窗口轮询。
    fn poll_autohide(&mut self) {
        if !self.visible {
            return;
        }
        if let Some(shown_at) = self.shown_at {
            if shown_at.elapsed() < DISMISS_GRACE {
                return;
            }
        }
        if !self.window.is_focused() {
            self.hide();
        }
    }

    fn set_anchor(&mut self, anchor: TrayAnchor) {
        self.anchor = Some(anchor);
    }
}
