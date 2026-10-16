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
mod tray;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use tauri::{Manager, RunEvent, Url};

/// 引导状态机
const STATE_STARTING: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_FAILED: u8 = 2;

/// 等网关写入 `tokenIssueSecret` 的上限。
///
/// 首次运行实测约 2 秒就能读到（网关初始化写完 config.json + 生成凭据），
/// 15 秒是给慢盘/杀软扫描留的余量。**不能设得更短**：超时后会退到
/// 密码输入页，那正是方案 B 想消掉的体验。
const SECRET_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 轮询间隔。250ms 是为了让"等到了"这件事在用户察觉不到延迟的前提下尽快发生。
const SECRET_WAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

struct Bootstrap {
    state: AtomicU8,
    /// 就绪后前端要加载的 URL
    url: Mutex<String>,
    /// 失败原因（展示给用户）
    error: Mutex<String>,
    /// 非致命提示（例如始终没等到 tokenIssueSecret）——加载页会显示后再进入
    warning: Mutex<String>,
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
            warning: Mutex::new(String::new()),
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
        "warning": boot.warning.lock().unwrap().clone(),
    })
}

/// 用户点「重试」时调用：重新走一遍引导。
#[tauri::command]
fn bootstrap_retry(app: tauri::AppHandle) {
    let boot = app.state::<Bootstrap>();
    boot.state.store(STATE_STARTING, Ordering::SeqCst);
    *boot.error.lock().unwrap() = String::new();
    *boot.warning.lock().unwrap() = String::new();
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

    let was_onboarding = cfg.needs_onboarding;
    if was_onboarding {
        gateway::log_line("检测到首次运行：尚无配置文件，交给网关初始化");
        *boot.onboarding.lock().unwrap() = true;
    } else {
        gateway::log_line(&format!("配置: {}", cfg.config_path.display()));
    }

    // ---- 第二步：确保网关在跑（首次运行会顺带生成配置）----
    let (outcome, plan) = gateway::ensure_running(&cfg);

    match outcome {
        Ok(gateway::GatewayState::Attached) => {
            gateway::log_line("已附着到运行中的网关");
        }
        Ok(gateway::GatewayState::Spawned(mut process)) => {
            gateway::log_line(&format!(
                "网关已启动（解释器 {}）",
                process.backend_path().display()
            ));
            gateway::drain_output(&mut process);
            *boot.process.lock().unwrap() = Some(process);
        }
        Err(err) => {
            boot.set_failed(err);
            return;
        }
    }

    // ---- 第三步：算 WebUI 地址（必要时等 tokenIssueSecret 出现）----
    //
    // `tokenIssueSecret` 是网关初始化时写进 config.json 的：
    //   · 首次运行时它和配置文件一起生成（第一步读到的只是占位值）；
    //   · 配置已存在但 secret 为空时，也可能稍后才补齐。
    // 所以这里**轮询等待**，而不是读一次就认命 —— 只读一次会把用户挡在一个
    // 本可自动填好的密码框前面。这是「方案 B」，分析见 docs/auth-flow-analysis.md。
    //
    // 常规启动（secret 本来就在）不会进入这个分支，零额外耗时。
    if cfg.secret.is_empty() {
        let started = std::time::Instant::now();
        match config::wait_for_secret(SECRET_WAIT_TIMEOUT, SECRET_WAIT_INTERVAL, config::load) {
            Ok(fresh) => {
                cfg = fresh;
                gateway::log_line(&format!(
                    "已等到网关写入 tokenIssueSecret（耗时 {} ms）",
                    started.elapsed().as_millis()
                ));
            }
            Err(outcome) => {
                if let Some(last) = outcome.last {
                    cfg = last;
                }
                let note = format!(
                    "等待 {} 秒（{} 次读取）仍未读到 tokenIssueSecret。\n\
                     将以无凭据方式打开界面：若界面要求输入密码，说明网关要求凭据。\n\
                     请检查 {} 里的 channels.websocket.tokenIssueSecret。",
                    outcome.waited.as_secs(),
                    outcome.attempts,
                    cfg.config_path.display()
                );
                gateway::log_line(&note);
                *boot.warning.lock().unwrap() = note;
            }
        }
    }

    if was_onboarding {
        gateway::log_line(&format!(
            "首次运行完成：WebUI 端口 {}，凭据 {}",
            plan.webui,
            if cfg.secret.is_empty() {
                "未设置（依赖 localhost 判定）"
            } else {
                "已生成"
            }
        ));
    }

    // **用实际端口覆盖配置读到的值**。
    // 端口回退只体现在临时覆盖文件里，不会改写用户的 config.json，
    // 所以 config::load() 拿到的仍是原始端口——不覆盖的话 WebView 会去连
    // 一个根本不是我们网关的端口（实测踩到过：网关在 8766，界面却连 8765）。
    cfg.health_port = plan.health;
    cfg.webui_port = plan.webui;

    // 记下实际端口，供下次启动探测（否则会在回退端口上错过正在运行的网关）
    gateway::remember_ports(&plan);

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

/// 强制 WebUI 默认语言为简体中文。
///
/// 为什么需要注入而不改上游源码：上游 `webui/src/i18n/config.ts` 的
/// `resolveInitialLocale()` **只读 localStorage，完全不看系统语言**，
/// 所以中文用户首次打开必然是英文界面（`defaultLocale` 硬编码为 "en"）。
///
/// 注入点是 `initialization_script`：它在页面自身脚本之前执行，
/// 因此 i18n 模块初始化时读到的就已经是我们写入的值。
///
/// 这里**刻意无条件写入**（不是"仅当未设置"）：需求是默认中文，
/// 无条件写可保证每次启动都回到中文。用户在界面里切换语言后，
/// 那次切换在本次会话内生效，重启后回到中文。
///
/// 存储键必须与上游 `LOCALE_STORAGE_KEY` 一致：`nanobot.locale`。
const FORCE_CHINESE_LOCALE_SCRIPT: &str = r#"
(function () {
  try {
    window.localStorage.setItem('nanobot.locale', 'zh-CN');
  } catch (e) {
    /* localStorage 不可用时交给上游走它自己的默认值 */
  }
})();
"#;

/// 上游 `webui/src/i18n/config.ts` 里定义的语言存储键。
/// 单独抽出来是为了让测试能断言注入脚本与上游约定一致——
/// 这个键一改，语言就会静默退回英文，而且没有任何报错。
#[cfg_attr(not(test), allow(dead_code))]
const UPSTREAM_LOCALE_STORAGE_KEY: &str = "nanobot.locale";

/// 构造注入脚本。抽成函数便于测试。
fn locale_script() -> String {
    FORCE_CHINESE_LOCALE_SCRIPT.to_string()
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

            let handle = app.handle().clone();

            // 托盘建不起来不该让整个应用起不来（例如某些精简版 Windows shell）
            match tray::build(&handle) {
                Ok(()) => gateway::log_line("托盘图标已创建；关闭窗口将隐藏到托盘，退出请用托盘菜单"),
                Err(err) => {
                    gateway::log_line(&format!("托盘图标创建失败（不影响主功能）: {err}"))
                }
            }

            let url: Url = "tauri://localhost/".parse()?;
            let window = tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url))
                .title("SnapBot")
                .inner_size(1280.0, 860.0)
                .min_inner_size(900.0, 600.0)
                .initialization_script(locale_script())
                .build()?;

            // 关窗 = 隐藏到托盘。挂在窗口上（on_window_event 不在 AppHandle 上）
            let inner = window.as_ref().window();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    tray::handle_close_request(&inner, api);
                }
            });

            gateway::log_line("窗口已创建，开始在后台引导网关");

            // 仅调试构建：用于自动化验证「托盘退出」这条路径。
            // 托盘菜单点击无法从外部脚本可靠触发，但那条路径串起了
            // ExitRequested 放行 + Exit 清理网关两个关键行为，值得可测。
            #[cfg(debug_assertions)]
            if let Ok(ms) = std::env::var("SNAPBOT_DEBUG_AUTOQUIT_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    let quit_handle = handle.clone();
                    gateway::log_line(&format!("[调试] {ms} ms 后自动退出"));
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                        tray::request_quit(&quit_handle);
                    });
                }
            }

            std::thread::spawn(move || run_bootstrap(handle));
            Ok(())
        })        .build(tauri::generate_context!())
        .expect("Tauri 应用初始化失败")
        .run(|app, event| match event {
            // 窗口关掉不等于要退出：拦住，让网关继续在后台服务。
            // 注意 Tauri 在**最后一个窗口关闭时**默认会真的退出，所以这一拦是必需的。
            // 托盘「退出」会先置位 QUIT_REQUESTED，这里的判断因而放行。
            RunEvent::ExitRequested { api, .. } => {
                if !tray::is_quit_requested() {
                    api.prevent_exit();
                }
            }
            // 真正退出：进程即将结束，必须清理网关。
            // 原实现在 ExitRequested 里清理，但那个事件现在会被 prevent_exit 拦下，
            // 所以清理必须挪到 Exit —— Exit 在任何退出路径上都会触发一次。
            RunEvent::Exit => {
                if let Some(state) = app.try_state::<Bootstrap>() {
                    if let Some(mut process) = state.process.lock().unwrap().take() {
                        gateway::log_line("停止由本应用启动的网关");
                        process.shutdown();
                    } else {
                        gateway::log_line("网关非本应用启动，退出时保持运行");
                    }
                }
                // 状态文件只在"本应用正跑着"时有意义，退出即清掉，
                // 避免下次启动拿一份过期端口去做探测。
                gateway::forget_ports();
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注入脚本必须写上游真正读取的那个 localStorage 键。
    ///
    /// 这个断言比它看起来重要：如果上游改了 `LOCALE_STORAGE_KEY`
    /// （或我们把它写错），界面会**静默退回英文**——没有异常、没有日志，
    /// 只有肉眼看界面才能发现。这条测试把"静默失败"变成"编译期可见的失败"。
    #[test]
    fn locale_script_writes_the_key_upstream_reads() {
        let script = locale_script();
        let expected = format!("'{UPSTREAM_LOCALE_STORAGE_KEY}'");
        assert!(
            script.contains(&expected),
            "注入脚本必须写入 {UPSTREAM_LOCALE_STORAGE_KEY}，实际脚本：{script}"
        );
    }

    #[test]
    fn locale_script_defaults_to_simplified_chinese() {
        assert!(locale_script().contains("'zh-CN'"));
    }

    /// 必须用 setItem（覆盖写）而不是"仅当不存在时才写"，
    /// 否则用户切到英文后重启仍是英文，不符合"默认中文"的要求。
    #[test]
    fn locale_script_overwrites_rather_than_conditionally_sets() {
        let script = locale_script();
        assert!(script.contains("setItem"));
        assert!(!script.contains("getItem"));
    }

    /// 等 secret 的超时必须"比网关初始化慢"，否则方案 B 会在首次运行上失效：
    /// 超时 → 落到密码框 → 用户被要求手输一个本可自动读取的密码。
    #[test]
    fn secret_wait_timeout_is_generous_enough_for_first_run() {
        assert!(
            SECRET_WAIT_TIMEOUT >= std::time::Duration::from_secs(10),
            "首次运行初始化要数秒，超时不能低于 10 秒（当前 {SECRET_WAIT_TIMEOUT:?}）"
        );
        assert!(
            !SECRET_WAIT_INTERVAL.is_zero() && SECRET_WAIT_INTERVAL <= std::time::Duration::from_millis(500),
            "轮询间隔应在 1..=500ms（当前 {SECRET_WAIT_INTERVAL:?}）"
        );
    }
}
