//! 托盘图标与菜单。
//!
//! 行为约定：
//! - 点窗口的关闭按钮 → **隐藏到托盘**，不退出（网关继续在后台跑）
//! - 托盘左键单击 → 显示/聚焦主窗口
//! - 托盘菜单「显示主窗口」/「退出 SnapBot」
//!
//! 为什么关闭按钮是隐藏而不是退出：这是个常驻助手，网关在后台跑着渠道与定时任务。
//! 关掉窗口就顺手把网关也杀了，会让「关个窗口」变成「停掉服务」，不符合预期。
//! 真要退出走托盘菜单，那时才回收网关。

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

/// 用户是否明确要求退出（托盘菜单「退出」）。
///
/// 为什么需要这个标志：Tauri 默认在**最后一个窗口关闭**时触发
/// `ExitRequested` 并真的退出。我们把关窗改成隐藏，但系统在别处仍可能要求退出
/// （例如 Windows 注销）。用这个标志区分「用户要退出」与「只是关了窗口」。
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn is_quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::SeqCst)
}

/// 请求退出应用（托盘菜单与调试钩子共用）。
///
/// 顺序很重要：**先置位标志再 `exit`**。`app.exit()` 会触发
/// `RunEvent::ExitRequested`，而我们在那里会 `prevent_exit()` 拦住它——
/// 只有标志已置位才能放行，否则「退出」会被自己挡住。
pub fn request_quit(app: &AppHandle) {
    QUIT_REQUESTED.store(true, Ordering::SeqCst);
    crate::gateway::log_line("请求退出应用");
    app.exit(0);
}

/// 托盘图标用的 PNG。直接复用打包进来的窗口图标，避免引入第二个图标文件。
const TRAY_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// 显示并聚焦主窗口（托盘点击、菜单项都走这里）。
pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

/// 处理主窗口的关闭请求：隐藏到托盘而不是退出。
///
/// 由 `main.rs` 在建窗之后挂到窗口上（`WebviewWindow::on_window_event`）。
/// 注意 `on_window_event` 是 `Window` / `WebviewWindow` / Builder 上的方法，
/// **不在 `AppHandle` 上**（这是实现时踩到的第一个坑）。
pub fn handle_close_request(window: &tauri::Window, api: &tauri::CloseRequestApi) {
    // 明确退出时不要拦，否则托盘「退出」菜单会被自己挡住
    if is_quit_requested() {
        return;
    }
    api.prevent_close();
    let _ = window.hide();
    crate::gateway::log_line("主窗口已隐藏到托盘（网关继续运行）");
}

/// 创建托盘图标与菜单。
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出 SnapBot", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let icon = tauri::image::Image::from_bytes(TRAY_ICON_PNG)?;

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("SnapBot — nanobot 桌面客户端")
        .menu(&menu)
        // 只响应左键：右键留给菜单，否则右键点一下也会弹窗口，很烦
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => request_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}
