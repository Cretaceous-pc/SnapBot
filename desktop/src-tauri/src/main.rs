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
mod single_instance;

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
    /// 是否处于首次运行引导（前端据此显示不同文案）
    onboarding: Mutex<bool>,
}

impl Bootstrap {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_STARTING),
            url: Mutex::new(String::new()),
            error: Mutex::new(String::new()),
            process: Mutex::new(None),
            onboarding: Mutex::new(false),
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
        "onboarding": *boot.onboarding.lock().unwrap(),
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
///
/// **顺序是刻意的**：先起网关、再读配置。因为首次运行时 `config.json` 还不存在，
/// 是网关的初始化路径把它写出来的。反过来做，首次运行必然失败。
///
/// 首次运行的时序说明（已核对上游 `cli/webui.py`）：`webui --yes --no-open`
/// 在 `--no-open` 且非 dev 时会走到 `_attach_to_background_gateway()`，
/// **跟随网关日志并阻塞**——它不会在初始化后立刻退出。网关本身是它启动的
/// 一个独立后台进程。我们的 WebView 连上后即成为客户端，网关的生命周期
/// 便由客户端租约维持（on-demand 语义：最后一个客户端退出才回收）。
fn run_bootstrap(app: tauri::AppHandle) {
    let boot = app.state::<Bootstrap>();

    // ---- 第一步：判断是不是首次运行（配置文件可能还不存在）----
    let mut cfg = match config::load_or_default() {
        Ok(cfg) => cfg,
        Err(err) => {
            boot.set_failed(err);
            return;
        }
    };

    if cfg.needs_onboarding {
        gateway::log_line("检测到首次运行：尚无配置文件，交给网关初始化");
        *boot.onboarding.lock().unwrap() = true;
    } else {
        gateway::log_line(&format!("配置: {}", cfg.config_path.display()));
    }

    // ---- 第二步：确保网关在跑（首次运行会顺带生成配置）----
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

    // ---- 第三步：现在配置一定存在了，重新读它来算 URL ----
    //
    // 首次运行后**必须重新读**：tokenIssueSecret 是上游在这次初始化里生成的，
    // 第一步拿到的只是占位值（secret 为空），用它开窗会连不上后端。
    if cfg.needs_onboarding {
        match config::load() {
            Ok(fresh) => {
                cfg = fresh;
                gateway::log_line(&format!(
                    "首次运行完成：WebUI 端口 {}，凭据 {}",
                    cfg.webui_port,
                    if cfg.secret.is_empty() {
                        "未设置（依赖 localhost 判定）"
                    } else {
                        "已生成"
                    }
                ));
            }
            Err(err) => {
                boot.set_failed(format!(
                    "网关已启动，但重新读取配置失败：{err}\n\
                     请检查 ~/.nanobot/config.json 是否被正确写入。"
                ));
                return;
            }
        }
    }

    if !cfg.websocket_enabled {
        boot.set_failed(
            "配置里 websocket 通道未启用，WebUI 无法连接。\n\
             请先运行一次 `nanobot webui --yes` 让它启用本地 WebUI 通道。"
                .to_string(),
        );
        return;
    }

    boot.set_ready(cfg.browser_url());
    gateway::log_line("引导完成，WebView 即将导航到 WebUI");
}

fn main() {
    // 单实例：第二个实例直接退出，避免开出两个窗口共用同一网关
    if !single_instance::acquire() {
        gateway::log_line("检测到已有 SnapBot 实例在运行，本实例退出");
        return;
    }

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
