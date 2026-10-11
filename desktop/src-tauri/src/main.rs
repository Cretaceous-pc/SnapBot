//! SnapBot 桌面壳入口。
//!
//! 流程：读配置 → 确保网关运行 → 开窗显示加载页 → 就绪后导航到 WebUI。
//!
//! 为什么用「加载页 + 轮询」而不是「Rust 侧等好再建窗口」：
//! 窗口先出现能让用户立刻看到进度，而不是双击后几十秒没反应。
//! 冷启动时网关要初始化渠道与 cron，可能花几秒到十几秒。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod gateway;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use tauri::{Manager, RunEvent, Url};

/// 引导状态机
const STATE_STARTING: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_FAILED: u8 = 2;

struct Bootstrap {
    state: AtomicU8,
    /// 就绪后前端要加载的 URL
    url: Mutex<String>,
    /// 失败原因（展示给用户）
    error: Mutex<String>,
    /// 我们起的网关进程；附着模式下为 None
    process: Mutex<Option<gateway::GatewayProcess>>,
}

impl Bootstrap {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_STARTING),
            url: Mutex::new(String::new()),
            error: Mutex::new(String::new()),
            process: Mutex::new(None),
        }
    }

    fn set_ready(&self, url: String) {
        *self.url.lock().unwrap() = url;
        self.state.store(STATE_READY, Ordering::SeqCst);
    }

    fn set_failed(&self, message: String) {
        gateway::log_line(&format!("启动失败: {message}"));
        *self.error.lock().unwrap() = message;
        self.state.store(STATE_FAILED, Ordering::SeqCst);
    }
}

/// 加载页用这个轮询引导进度。返回 `state` / `url` / `error`。
#[tauri::command]
fn bootstrap_status(app: tauri::AppHandle) -> serde_json::Value {
    let boot = app.state::<Bootstrap>();
    let state = boot.state.load(Ordering::SeqCst);
    serde_json::json!({
        "state": state,
        "ready": state == STATE_READY,
        "failed": state == STATE_FAILED,
        "url": boot.url.lock().unwrap().clone(),
        "error": boot.error.lock().unwrap().clone(),
    })
}

/// 用户点「重试」时调用：重新走一遍引导。
#[tauri::command]
fn bootstrap_retry(app: tauri::AppHandle) {
    let boot = app.state::<Bootstrap>();
    boot.state.store(STATE_STARTING, Ordering::SeqCst);
    *boot.error.lock().unwrap() = String::new();
    let handle = app.clone();
    std::thread::spawn(move || run_bootstrap(handle));
}

/// 引导主体。在独立线程里跑，避免阻塞 UI 线程。
fn run_bootstrap(app: tauri::AppHandle) {
    let boot = app.state::<Bootstrap>();

    let cfg = match config::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            boot.set_failed(err);
            return;
        }
    };

    gateway::log_line(&format!("配置: {}", cfg.config_path.display()));
    if !cfg.websocket_enabled {
        boot.set_failed(
            "配置里 websocket 通道未启用，WebUI 无法连接。\n\
             请先运行一次 `nanobot webui --yes` 让它启用本地 WebUI 通道。"
                .to_string(),
        );
        return;
    }

    match gateway::ensure_running(&cfg) {
        Ok(gateway::GatewayState::Attached) => {
            gateway::log_line("已附着到运行中的网关");
        }
        Ok(gateway::GatewayState::Spawned(mut process)) => {
            gateway::log_line(&format!(
                "网关已启动（解释器 {}）",
                process.python_path().display()
            ));
            gateway::drain_output(&mut process);
            *boot.process.lock().unwrap() = Some(process);
        }
        Err(err) => {
            boot.set_failed(err);
            return;
        }
    }

    boot.set_ready(cfg.browser_url());
    gateway::log_line("引导完成，WebView 即将导航到 WebUI");
}

fn main() {
    tauri::Builder::default()
        .manage(Bootstrap::new())
        .invoke_handler(tauri::generate_handler![bootstrap_status, bootstrap_retry])
        .setup(|app| {
            gateway::log_line("=== SnapBot 桌面壳启动 ===");
            let url: Url = "tauri://localhost/".parse()?;
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("SnapBot")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .build()?;
            gateway::log_line("窗口已创建，开始在后台引导网关");

            let handle = app.handle().clone();
            std::thread::spawn(move || run_bootstrap(handle));
            Ok(())
        })        .build(tauri::generate_context!())
        .expect("Tauri 应用初始化失败")
        .run(|app, event| {
            if let RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app.try_state::<Bootstrap>() {
                    if let Some(mut process) = state.process.lock().unwrap().take() {
                        gateway::log_line("停止由本应用启动的网关");
                        process.shutdown();
                    } else {
                        gateway::log_line("网关非本应用启动，退出时保持运行");
                    }
                }
            }
        });
}
