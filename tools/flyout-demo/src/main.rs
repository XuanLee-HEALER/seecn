// flyout-demo: validate a macOS status-bar flyout. Throwaway experiment.
// Checks: undecorated+transparent webview popup; anchoring under the tray icon
// via TrayIconEvent::Click.rect; Accessory activation policy (no Dock icon);
// light-dismiss on focus loss.
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::WebViewBuilder;

enum UserEv {
    Click { x: f64, y: f64, w: f64, h: f64 },
    AutoShow,
}

const HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8"><style>
html,body{margin:0;background:transparent;font-family:-apple-system,sans-serif}
.card{margin:8px;padding:14px 16px;border-radius:14px;background:rgba(28,28,30,.92);
color:#fff;box-shadow:0 8px 30px rgba(0,0,0,.45)}
.dot{display:inline-block;width:9px;height:9px;border-radius:50%;background:#22C55E;margin-right:7px}
h1{font-size:13px;margin:0 0 8px;font-weight:600}
.row{font-size:12px;opacity:.85;margin:3px 0}
</style></head><body><div class="card">
<h1><span class="dot"></span>cc monitor &mdash; demo</h1>
<div class="row">pid 86519 &middot; active &middot; &darr; 39 KB/s</div>
<div class="row">pid 61066 &middot; idle</div>
</div>
<script>window.onload=function(){var c=document.querySelector('.card').getBoundingClientRect();var s=getComputedStyle(document.querySelector('.card'));window.ipc.postMessage('card '+Math.round(c.width)+'x'+Math.round(c.height)+' radius='+s.borderTopLeftRadius+' bg='+s.backgroundColor);};</script>
</body></html>"#;

fn make_icon() -> Icon {
    let s = 16u32;
    let mut rgba = vec![0u8; (s * s * 4) as usize];
    let c = (s as f32 - 1.0) / 2.0;
    let r = s as f32 / 2.0 - 1.0;
    for y in 0..s {
        for x in 0..s {
            let d = (((x as f32 - c).powi(2)) + ((y as f32 - c).powi(2))).sqrt();
            let a = if d <= r { 255u8 } else { 0u8 };
            let i = ((y * s + x) * 4) as usize;
            rgba[i] = 0x22;
            rgba[i + 1] = 0xC5;
            rgba[i + 2] = 0x5E;
            rgba[i + 3] = a;
        }
    }
    Icon::from_rgba(rgba, s, s).unwrap()
}

fn main() {
    let mut event_loop = EventLoopBuilder::<UserEv>::with_user_event().build();
    // Status-bar app: no Dock icon, no menu-bar takeover.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let menu = Menu::new();
    let _ = menu.append(&MenuItem::new("Quit", true, None));
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("flyout demo")
        .with_icon(make_icon())
        .build()
        .expect("tray build");

    let click_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            rect,
            ..
        } = e
        {
            let _ = click_proxy.send_event(UserEv::Click {
                x: rect.position.x,
                y: rect.position.y,
                w: rect.size.width as f64,
                h: rect.size.height as f64,
            });
        }
    }));

    let window = WindowBuilder::new()
        .with_decorations(false)
        .with_resizable(false)
        .with_visible(false)
        .with_always_on_top(true)
        .with_transparent(true)
        .with_inner_size(LogicalSize::new(280.0, 110.0))
        .build(&event_loop)
        .expect("window build");
    let _webview = WebViewBuilder::new()
        .with_transparent(true)
        .with_ipc_handler(|req| println!("[demo][webview] rendered: {}", req.body()))
        .with_html(HTML)
        .build(&window)
        .expect("webview build");
    let win_id = window.id();

    // Auto-show at top-right after 1.5s, for screenshot validation without a click.
    let auto = event_loop.create_proxy();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = auto.send_event(UserEv::AutoShow);
    });

    event_loop.run(move |event, _t, cf| {
        *cf = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEv::Click { x, y, w, h }) => {
                // Anchor under the icon: center x on the icon, top at icon bottom.
                let ws = window.outer_size();
                let px = (x + w / 2.0 - ws.width as f64 / 2.0) as i32;
                let py = (y + h + 2.0) as i32;
                window.set_outer_position(PhysicalPosition::new(px, py));
                window.set_visible(true);
                window.set_focus();
                println!("[demo] Click rect pos=({x:.0},{y:.0}) size=({w}x{h}) -> flyout at ({px},{py})");
            }
            Event::UserEvent(UserEv::AutoShow) => {
                if let Some(m) = window.primary_monitor() {
                    let ms = m.size();
                    let mo = m.position();
                    let sc = m.scale_factor();
                    let ws = window.outer_size();
                    let px = mo.x + ms.width as i32 - ws.width as i32 - 16;
                    let py = mo.y + 36;
                    window.set_outer_position(PhysicalPosition::new(px, py));
                    window.set_visible(true);
                    window.set_focus();
                    println!(
                        "[demo] AUTO-show scale={sc} pos_phys=({px},{py}) size_phys=({},{})",
                        ws.width, ws.height
                    );
                } else {
                    window.set_visible(true);
                    window.set_focus();
                    println!("[demo] AUTO-show (no monitor info)");
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Focused(f),
                ..
            } if window_id == win_id => {
                println!("[demo] Focused({f})");
                if !f {
                    window.set_visible(false);
                    println!("[demo] -> hidden on focus loss (light-dismiss)");
                }
            }
            _ => {}
        }
    });
}
