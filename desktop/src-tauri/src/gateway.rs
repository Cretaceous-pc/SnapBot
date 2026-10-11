//! 网关进程的生命周期管理。
//!
//! 两条刻意的设计决策：
//!
//! 1. **优先附着到已在运行的网关**，而不是无脑再起一个。上游的 `nanobot gateway`
//!    检测到已有实例时会直接退出（gateway_already_running），所以如果不先探测，
//!    用户手动跑过一次 CLI 之后，桌面壳就再也起不来了。
//! 2. **首次运行走上游自己的初始化路径**（`webui --yes --no-open`），而不是我们
//!    手搓一份 config.json。手搓等于把上游的 schema 与校验规则复制一遍，早晚漂移。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, GatewayConfig};

// serde_json 用于端口回退时改写配置（见 write_override_config）

/// 单次 HTTP/TCP 探测的超时。健康轮询用，必须短。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// 起网关后的最长等待时间。首次冷启动要初始化渠道与 cron，给足余量。
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// 首次运行时等待上游写出 config.json 的最长时间。
/// 首次会跑 `bun install` + `vite build` 构建前端产物，实测约 90 秒。
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(180);

/// 网关当前状态
pub enum GatewayState {
    /// 我们起的进程，退出时需要回收
    Spawned(GatewayProcess),
    /// 复用已在运行的实例，退出时**不动它**
    Attached,
}

/// 后端形态。决定命令行怎么拼 —— 这是冻结后**最容易出错**的一处。
pub enum BackendRuntime {
    /// 冻结产物：单文件 exe，直接用子命令，不能带 `-m nanobot`
    Frozen(PathBuf),
    /// 开发形态：需要 `python -m nanobot`
    Python(PathBuf),
}

impl BackendRuntime {
    fn program(&self) -> &Path {
        match self {
            BackendRuntime::Frozen(path) | BackendRuntime::Python(path) => path,
        }
    }

    /// 是否为冻结形态（写日志用）。
    fn is_frozen(&self) -> bool {
        matches!(self, BackendRuntime::Frozen(_))
    }

    /// 把 nanobot 的参数翻译成这个形态可用的完整命令行。
    ///
    /// 冻结形态直接 `nanobot-backend.exe gateway --foreground ...`；
    /// 开发形态要 `python.exe -m nanobot gateway --foreground ...`。
    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(self.program());
        if matches!(self, BackendRuntime::Python(_)) {
            cmd.arg("-m").arg("nanobot");
        }
        cmd.args(args);
        cmd
    }
}

pub struct GatewayProcess {
    child: Child,
    /// 记录实际使用的后端路径，排障时第一眼要看的东西
    backend: PathBuf,
}

impl GatewayProcess {
    /// 实际使用的后端路径（写日志/展示用）。
    pub fn backend_path(&self) -> &Path {
        &self.backend
    }

    /// 请求停止并等待退出；超时则强杀。
    ///
    /// 为什么不调 `nanobot gateway stop`：我们持有 Child 句柄，直接杀最可靠，
    /// 也不依赖 CLI 是否可用（冻结打包后尤其重要）。
    /// 用 `/T` 是因为网关会再拉起子进程（渠道、cron），只杀直接子进程会留孤儿占端口。
    pub fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return; // 已退出
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------- 定位解释器

/// 定位后端可执行文件。两种形态：
///
///   1. **冻结形态**：随 Tauri `resources` 打包的 `nanobot-backend.exe`。
///      这是发布形态 —— 用户机器上没有 Python，也不该有。
///   2. **开发形态**：上游检出里的 `.venv\Scripts\python.exe`。
///
/// 顺序有意为之：先找冻结后端（自包含、最快），再回落到开发检出。
/// `SNAPBOT_BACKEND` 可用于显式覆盖，便于排障。
///
/// 接收 `cfg` 是为了让开发形态也能用到「配置所在的数据目录」这一线索；
/// 当前实现按仓库相对路径查找，参数保留以便后续支持便携式布局。
pub fn resolve_backend(_cfg: &GatewayConfig) -> Result<BackendRuntime, String> {
    if let Some(explicit) = std::env::var_os("SNAPBOT_BACKEND") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(BackendRuntime::Frozen(path));
        }
        return Err(format!("SNAPBOT_BACKEND 指向的文件不存在: {}", path.display()));
    }

    // 1) 冻结后端：与主程序同级、或打包后的 resources 目录
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidates = [
                dir.join(BACKEND_EXE),
                dir.join("nanobot-backend").join(BACKEND_EXE),
                dir.join("resources").join("nanobot-backend").join(BACKEND_EXE),
                dir.join("resources").join(BACKEND_EXE),
            ];
            for candidate in candidates {
                if candidate.is_file() {
                    return Ok(BackendRuntime::Frozen(candidate));
                }
            }
        }
    }

    // 2) 开发形态：上游检出的 venv
    let mut dev: Vec<PathBuf> = Vec::new();
    if let Some(root) = workspace_root() {
        dev.push(
            root.join("nanobot-0.3.5")
                .join(".venv")
                .join("Scripts")
                .join("python.exe"),
        );
    }
    for candidate in &dev {
        if candidate.is_file() {
            return Ok(BackendRuntime::Python(candidate.clone()));
        }
    }

    Err(format!(
        "找不到后端可执行文件。\n\
         已尝试冻结形态：随程序打包的 {BACKEND_EXE}\n\
         已尝试开发形态：\n{}\n\
         开发期请运行 scripts/restore-upstream.ps1，或用 SNAPBOT_BACKEND 显式指定。",
        dev.iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// 冻结后端可执行文件名（与 packaging/snapbot-backend.spec 的 name 保持一致）。
const BACKEND_EXE: &str = "nanobot-backend.exe";

fn workspace_root() -> Option<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
}

// ---------------------------------------------------------------- 探测

/// 向 `/health` 发一个最小 HTTP/1.0 GET，返回响应体（失败返回空串）。
///
/// 用 HTTP/1.0 + `Connection: close`：让对端主动结束连接，
/// 于是 `read_to_string` 能自然读到 EOF，不必解析 Content-Length。
fn get_health(host: &str, port: u16) -> String {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(addr) => addr,
        Err(_) => return String::new(),
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));

    let request =
        format!("GET /health HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return String::new();
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
}

/// 确认对面**确实是 nanobot 网关**。
///
/// 为什么不能只做 `TcpStream::connect`：端口上可能蹲着任何别的程序。
/// 只测端口连通会把陌生服务误判成「网关已在运行」，然后「附着」上去，结果是白屏。
/// 这里要求响应里带 `"status"` 字段才认。
fn probe_health(host: &str, port: u16) -> bool {
    get_health(host, port).contains("\"status\"")
}

/// 端口是否空闲（没有任何东西在监听）。
fn port_is_free(host: &str, port: u16) -> bool {
    let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err()
}

/// 端口回退的扫描上限。20 个足够覆盖「同机装了多个 nanobot 实例」的情况。
const PORT_SCAN_LIMIT: u16 = 20;

/// 实际使用的端口组合。
///
/// 调用方**必须**用它覆盖从磁盘读到的端口值：端口回退只写在临时覆盖文件里，
/// 不会改写用户的 `config.json`，所以 `config::load()` 读回的仍是原始端口。
pub struct PortPlan {
    /// 网关健康端点实际使用的端口
    pub health: u16,
    /// WebUI / WebSocket 实际使用的端口
    pub webui: u16,
    /// 是否发生了回退（仅用于日志）
    pub fell_back: bool,
}

/// 挑选可用端口：优先沿用配置里的端口，被占用才往后找。
///
/// 为什么要做这件事：两个端口都可能被别人占——18790 是 nanobot 自己的默认值，
/// 用户很可能之前手动跑过一次 CLI 把它留在后台；8765 更是常见端口。
/// 撞上了就直接起不来，而报错信息对普通用户毫无意义。
pub fn plan_ports(cfg: &GatewayConfig) -> PortPlan {
    for offset in 0..PORT_SCAN_LIMIT {
        let mut health = cfg.health_port.saturating_add(offset);
        if health < 1024 {
            health = 1024 + offset; // 跳过特权端口
        }
        let mut webui = cfg.webui_port.saturating_add(offset);
        if webui < 1024 {
            webui = 1024 + offset;
        }
        // 两者撞到同一个号时让 webui 再挪一格
        let webui_port = if webui == health { webui + 1 } else { webui };

        if port_is_free(&cfg.health_host, health) && port_is_free(&cfg.webui_host, webui_port) {
            let fell_back = health != cfg.health_port || webui_port != cfg.webui_port;
            return PortPlan {
                health,
                webui: webui_port,
                fell_back,
            };
        }
    }

    // 全都被占：退回配置值，让上游自己报错（比我们瞎猜一个端口好）
    PortPlan {
        health: cfg.health_port,
        webui: cfg.webui_port,
        fell_back: false,
    }
}

/// 写一份临时配置，只覆盖 WebUI 端口，交给上游 `--config` 使用。
///
/// 为什么不原地改用户的 config.json：那是用户的数据，我们无权改写；
/// 而且下次启动端口可能又变了，反复改写会把配置搞得面目全非。
/// 覆盖文件只是一个**启动参数**，用完即弃。
fn write_override_config(cfg: &GatewayConfig, webui_port: u16) -> Result<PathBuf, String> {
    let raw = std::fs::read_to_string(&cfg.config_path)
        .map_err(|err| format!("读取配置失败 {}: {err}", cfg.config_path.display()))?;
    let mut doc: serde_json::Value =
        serde_json::from_str(&raw).map_err(|err| format!("解析配置失败: {err}"))?;

    // 只精确改 channels.websocket.port，其余原样保留
    let slot = doc
        .get_mut("channels")
        .and_then(|channels| channels.get_mut("websocket"))
        .and_then(|ws| ws.get_mut("port"))
        .ok_or("配置里找不到 channels.websocket.port，无法覆盖端口")?;
    *slot = serde_json::Value::from(webui_port);

    let target = override_config_path(cfg)?;
    std::fs::write(&target, serde_json::to_string_pretty(&doc).unwrap_or_default())
        .map_err(|err| format!("写覆盖配置失败 {}: {err}", target.display()))?;
    Ok(target)
}

fn override_config_path(cfg: &GatewayConfig) -> Result<PathBuf, String> {
    Ok(cfg
        .config_path
        .parent()
        .ok_or("配置路径没有父目录")?
        .join("config.snapbot-override.json"))
}

/// 删除覆盖配置（尽力而为，失败不影响功能）。
fn remove_override_config(cfg: &GatewayConfig) {
    if let Ok(path) = override_config_path(cfg) {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------- 启动网关

/// 起一个新网关。
///
/// 返回 `(状态, 实际端口计划)`。调用方**必须**用返回的端口计划覆盖从磁盘读到的
/// 端口值——端口回退只写在临时覆盖文件里，不会改写用户的 `config.json`。
pub fn ensure_running(cfg: &GatewayConfig) -> (Result<GatewayState, String>, PortPlan) {
    /// 附着场景下的端口计划：对方用的就是配置里的端口。
    fn configured_plan(cfg: &GatewayConfig) -> PortPlan {
        PortPlan {
            health: cfg.health_port,
            webui: cfg.webui_port,
            fell_back: false,
        }
    }

    let backend = match resolve_backend(cfg) {
        Ok(backend) => backend,
        Err(err) => return (Err(err), configured_plan(cfg)),
    };
    log_line(&format!(
        "后端形态：{}（{}）",
        if backend.is_frozen() { "冻结" } else { "开发 venv" },
        backend.program().display()
    ));

    // ---- 1) 已经在跑？直接附着（要求 /health 真的应了）----
    //
    // 先看本应用上次记录的端口，再看配置端口。
    // 为什么需要这一步：端口回退后，应用**不会**改写用户的 config.json，
    // 于是新实例按配置端口去探测就会**错过**正在运行的网关（它跑在回退端口上），
    // 转而以为没人跑、去起第二个网关。实测踩到过这个现象。
    let recorded = recorded_shell_ports();
    if let Some(ports) = &recorded {
        if (ports.health != cfg.health_port || ports.webui != cfg.webui_port)
            && probe_health(&cfg.health_host, ports.health)
        {
            log_line(&format!(
                "发现本应用上次运行在回退端口上（health={} webui={}），附着而不重启",
                ports.health, ports.webui
            ));
            return (
                Ok(GatewayState::Attached),
                PortPlan {
                    health: ports.health,
                    webui: ports.webui,
                    fell_back: true,
                },
            );
        }
    }

    if probe_health(&cfg.health_host, cfg.health_port) {
        if let Some(pid) = recorded_gateway_pid() {
            if pid_alive(pid) {
                log_line(&format!("检测到已在运行的网关 (pid {pid})，附着而不重启"));
            } else {
                log_line("健康端点有响应，但记录的 pid 已不存在，仍按附着处理");
            }
        }
        return (Ok(GatewayState::Attached), configured_plan(cfg));
    }

    // ---- 2) 决定用哪种方式起 ----
    //
    // 配置缺失时走上游的 WebUI 入口：它会生成 schema 正确的 config.json、
    // 启用本地 WebUI 通道、把 tokenIssueSecret 写进去，然后拉起网关。
    // 注意：`--no-open` 且非 dev 时它会走到 `_attach_to_background_gateway()`
    // 跟随网关日志并**阻塞**，所以这个子进程不会在初始化后立刻退出。
    let first_run = !cfg.config_path.is_file();
    if first_run {
        log_line("未找到配置，走首次运行引导（上游 webui --yes --no-open）");
    }

    // ---- 2b) 端口回退（仅非首次运行）----
    //
    // 首次运行的端口由上游初始化决定，此时还没法覆盖，直接用配置里的值。
    // 非首次运行时先探测：如果端口被别人占（18790 是 nanobot 自己的默认值，
    // 用户很可能之前跑过一次 CLI 留在后台；8765 更是常见端口），
    // 就往后找一组空闲的，并通过临时覆盖配置 + --port 交给上游。
    let (plan, override_cfg) = if first_run {
        (configured_plan(cfg), None)
    } else {
        let plan = plan_ports(cfg);
        if plan.fell_back {
            log_line(&format!(
                "端口被占用，改用 health={} webui={}（配置原值 {}/{}）",
                plan.health, plan.webui, cfg.health_port, cfg.webui_port
            ));
        }
        let override_path = if plan.webui != cfg.webui_port {
            match write_override_config(cfg, plan.webui) {
                Ok(path) => Some(path),
                Err(err) => {
                    log_line(&format!("写端口覆盖配置失败，无法回退端口: {err}"));
                    remove_override_config(cfg);
                    return (Err(err), plan);
                }
            }
        } else {
            None
        };
        (plan, override_path)
    };

    let health_port = plan.health;

    // 组装参数（**不含** `-m nanobot`，那部分由 BackendRuntime::command 决定）
    let mut args: Vec<String> = Vec::new();
    let mode = if first_run {
        args.push("webui".into());
        args.push("--yes".into());
        args.push("--no-open".into());
        "webui --yes --no-open".to_string()
    } else {
        args.push("gateway".into());
        args.push("--foreground".into());
        if let Some(path) = &override_cfg {
            // --config 指向的是**覆盖文件**，它只改了 websocket.port
            args.push("--config".into());
            args.push(path.to_string_lossy().into_owned());
        }
        // 健康端口用 --port 直接覆盖，不必改配置文件
        args.push("--port".into());
        args.push(health_port.to_string());
        format!("gateway --foreground --port {health_port}")
    };

    let mut cmd = backend.command(&args);

    // 关键：**不要把 bootstrapSecret 放进命令行**。它会出现在进程列表里。
    // 上游 cli/windows_browser.py 专门用「匿名管道 + ACL 保护文件」来规避这件事；
    // 我们通过「让 nanobot 自己从 config.json 读」达到同样效果。

    // 发布形态下前端产物已预构建并随包分发，不该在用户机器上重建
    // （既慢又要 bun、要网络）。开发形态不加这个变量，改前端源码后仍会自动重建。
    if !cfg!(debug_assertions) {
        cmd.env("NANOBOT_SKIP_WEBUI_BUILD", "1");
    }

    // 工作目录：冻结形态用 exe 所在目录（onedir 的 _internal 就在旁边）；
    // 开发形态用检出根目录。
    let program = backend.program().to_path_buf();
    match &backend {
        BackendRuntime::Frozen(_) => {
            if let Some(dir) = program.parent() {
                cmd.current_dir(dir);
            }
        }
        BackendRuntime::Python(_) => {
            if let Some(root) = program.parent().and_then(|p| p.parent()) {
                cmd.current_dir(root);
            }
        }
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    log_line(&format!(
        "启动{}：{} {mode}",
        if first_run { "（首次运行）" } else { "" },
        backend.program().display()
    ));

    let backend_path = backend.program().to_path_buf();

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            remove_override_config(cfg);
            return (
                Err(format!("启动网关失败（{}）: {err}", backend_path.display())),
                plan,
            );
        }
    };

    // ---- 3) 首次运行：等上游把 config.json 写出来 ----
    if first_run {
        if let Err(err) = wait_for_config(&mut child, &cfg.config_path) {
            let mut process = GatewayProcess {
                child,
                backend: backend_path,
            };
            process.shutdown();
            remove_override_config(cfg);
            return (Err(err), plan);
        }
    }

    // ---- 4) 轮询健康端点，确认真的能服务请求 ----
    let ready_timeout = if first_run { BOOTSTRAP_TIMEOUT } else { READY_TIMEOUT };
    if let Err(err) = wait_for_health(&mut child, &cfg.health_host, health_port, ready_timeout) {
        let mut process = GatewayProcess {
            child,
            backend: backend_path,
        };
        process.shutdown();
        remove_override_config(cfg);
        return (Err(err), plan);
    }

    // 覆盖配置只在启动时被上游读一次，就绪后即可清掉，不留垃圾文件。
    // （清早于就绪有风险：上游可能还在读；就绪即证明它已经读完了。）
    remove_override_config(cfg);

    log_line(&format!(
        "网关健康端点已就绪（health={health_port}，webui={}）",
        plan.webui
    ));
    (
        Ok(GatewayState::Spawned(GatewayProcess {
            child,
            backend: backend_path,
        })),
        plan,
    )
}

/// 等上游写出配置文件。
fn wait_for_config(child: &mut Child, config_path: &Path) -> Result<(), String> {
    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    loop {
        if config_path.is_file() {
            log_line("首次运行配置已生成");
            return Ok(());
        }
        if let Some(err) = child_exited(child) {
            return Err(format!("首次运行初始化失败：{err}"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "等待生成配置超时（{} 秒）。首次运行需要构建前端产物，\
                 若 bun 或网络不可用会失败——请查看日志里的 [gateway] 输出。",
                BOOTSTRAP_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// 轮询健康端点直到就绪、进程退出或超时。
fn wait_for_health(
    child: &mut Child,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(err) = child_exited(child) {
            return Err(err);
        }
        if probe_health(host, port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("等待网关就绪超时（{} 秒）", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 若子进程已退出，返回带输出说明的错误；否则 None。
///
/// 把子进程的输出读出来是刻意的：否则用户只看到「启动失败」四个字，
/// 而真正的原因（端口占用、缺依赖、配置非法）全在被丢弃的管道里。
fn child_exited(child: &mut Child) -> Option<String> {
    let Ok(Some(status)) = child.try_wait() else {
        return None;
    };
    let mut detail = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut detail);
    }
    if let Some(mut out) = child.stdout.take() {
        let mut buf = String::new();
        let _ = out.read_to_string(&mut buf);
        if !buf.trim().is_empty() {
            detail.push_str("\n--- stdout ---\n");
            detail.push_str(&buf);
        }
    }
    let tail: Vec<&str> = detail.lines().rev().take(30).collect();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    Some(format!(
        "网关在就绪前退出（code {:?}）。输出末尾：\n{}",
        status.code(),
        if tail.trim().is_empty() { "(无输出)" } else { &tail }
    ))
}

// ---------------------------------------------------------------- 状态读取

/// 从 JSON 文本里取一个字符串字段。找不到返回 None。
///
/// 当前引导流程不需要它（我们只读 `pid`），保留是为了让这段极简解析器
/// 有完整的读写对称——接入 `run/gateway.json` 的其它字段时可直接用。
#[cfg_attr(not(test), allow(dead_code))]
fn json_string_field(raw: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 从 JSON 文本里取一个整数字段。找不到返回 None。
///
/// 不引 JSON 库：我们只需要 `run/gateway.json` 里的一个整数。
fn json_u32_field(raw: &str, key: &str) -> Option<u32> {
    let needle = format!("\"{key}\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = raw[start..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 读 `run/gateway.json` 里记录的 pid，判断网关是否已在运行。
fn recorded_gateway_pid() -> Option<u32> {
    let path = config::nanobot_data_dir()
        .ok()?
        .join("run")
        .join("gateway.json");
    let raw = std::fs::read_to_string(path).ok()?;
    json_u32_field(&raw, "pid")
}

/// 本应用上次实际使用的端口。
pub struct ShellPorts {
    pub health: u16,
    pub webui: u16,
}

fn shell_state_path() -> Option<PathBuf> {
    Some(
        config::nanobot_data_dir()
            .ok()?
            .join("run")
            .join("snapbot-desktop.json"),
    )
}

/// 记录本次实际使用的端口。
///
/// 为什么需要：端口回退后我们**不改写**用户的 `config.json`（那是用户的数据），
/// 于是下次启动时无从得知上次跑在哪个端口。把结果记在自己的状态文件里，
/// 下次就能先探测这些端口，正确附着到仍在运行的网关。
pub fn remember_ports(plan: &PortPlan) {
    let Some(path) = shell_state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let payload = serde_json::json!({
        "healthPort": plan.health,
        "webuiPort": plan.webui,
    });
    let _ = std::fs::write(&path, payload.to_string());
}

/// 读取上次记录的端口。文件不存在或字段缺失时返回 None。
fn recorded_shell_ports() -> Option<ShellPorts> {
    let path = shell_state_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    Some(ShellPorts {
        health: json_u32_field(&raw, "healthPort")? as u16,
        webui: json_u32_field(&raw, "webuiPort")? as u16,
    })
}

/// 退出时清掉状态文件；下次启动会重新写入。
pub fn forget_ports() {
    if let Some(path) = shell_state_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// 判断某个 pid 是否还活着。
#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- 输出排空

/// 持续读取子进程输出，避免管道写满导致子进程阻塞。
///
/// 我们以 `piped` 打开 stdout/stderr。如果不读，子进程写满管道缓冲区
/// （Windows 上约 4KB）后会**阻塞在写日志上**，表现为「网关起来了但不干活」。
pub fn drain_output(process: &mut GatewayProcess) {
    if let Some(stdout) = process.child.stdout.take() {
        std::thread::spawn(move || drain(stdout, "gateway"));
    }
    if let Some(stderr) = process.child.stderr.take() {
        std::thread::spawn(move || drain(stderr, "gateway!"));
    }
}

fn drain<R: Read>(mut reader: R, tag: &'static str) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    log_line(&format!("[{tag}] {line}"));
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 日志

/// 日志输出到 `~/.nanobot/logs/desktop.log`。
///
/// 为什么必须写文件而不是 eprintln：发布形态用 `windows_subsystem = "windows"`，
/// 进程没有控制台，stderr 会被直接丢弃，出问题时完全没有线索。
/// 每行独立 open/append：调用频率很低（启动期若干条），换来免状态、免锁、崩溃也不丢。
pub fn log_line(message: &str) {
    eprintln!("[snapbot] {message}");

    let Ok(dir) = config::nanobot_data_dir() else { return };
    let log_dir = dir.join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let line = format!("{} [snapbot] {message}\n", timestamp());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("desktop.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// 本地时间戳，避免为一个时间格式化引入 chrono。
///
/// 用 `GetLocalTime` 而不是 `SystemTime`：后者是 UTC，日志里会差 8 小时，
/// 排障时和 nanobot 自己的日志对不上。
#[cfg(windows)]
fn timestamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    extern "system" {
        fn GetLocalTime(out: *mut SystemTime);
    }
    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.year, st.month, st.day, st.hour, st.minute, st.second, st.milliseconds
    )
}

#[cfg(not(windows))]
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch+{secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个只在测试内使用的配置，端口由调用方指定。
    fn cfg_with_ports(health: u16, webui: u16) -> GatewayConfig {
        GatewayConfig {
            health_port: health,
            webui_port: webui,
            health_host: "127.0.0.1".to_string(),
            webui_host: "127.0.0.1".to_string(),
            ..GatewayConfig::default()
        }
    }

    /// 占住一个端口并返回它。用 0 让内核挑，避免测试之间抢固定端口。
    fn occupy_a_port() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        (listener, port)
    }

    #[test]
    fn plan_ports_keeps_configured_ports_when_free() {
        let (l1, health) = occupy_a_port();
        let (l2, webui) = occupy_a_port();
        // 关掉监听 → 端口空闲
        drop(l1);
        drop(l2);
        let plan = plan_ports(&cfg_with_ports(health, webui));
        // 空闲时应当原样沿用（除非恰好被别的进程抢走，概率极低）
        assert!(!plan.fell_back, "端口空闲时不该回退");
        assert_eq!(plan.health, health);
        assert_eq!(plan.webui, webui);
    }

    /// 核心场景：端口被占用时必须回退，且两个端口都要挪。
    #[test]
    fn plan_ports_falls_back_when_occupied() {
        let (l1, health) = occupy_a_port();
        let (l2, webui) = occupy_a_port();
        // 两个监听都**保持存活**，模拟被别的程序占用
        let plan = plan_ports(&cfg_with_ports(health, webui));
        assert!(plan.fell_back, "端口被占时必须回退");
        assert_ne!(plan.health, health);
        assert_ne!(plan.webui, webui);
        // 回退到的端口应当真的空闲
        assert!(port_is_free("127.0.0.1", plan.health));
        assert!(port_is_free("127.0.0.1", plan.webui));
        drop(l1);
        drop(l2);
    }

    /// 两个端口撞号时，webui 必须被挪开，不能与 health 相同。
    #[test]
    fn plan_ports_never_collides_health_with_webui() {
        let (l, p) = occupy_a_port();
        drop(l);
        // 故意把 health 与 webui 设成同一个号
        let plan = plan_ports(&cfg_with_ports(p, p));
        assert_ne!(plan.health, plan.webui, "health 与 webui 不能同号");
    }

    #[test]
    fn port_is_free_detects_listener() {
        let (listener, port) = occupy_a_port();
        assert!(!port_is_free("127.0.0.1", port), "有监听时应判定为不空闲");
        // 关掉监听后应恢复为空闲
        drop(listener);
        assert!(port_is_free("127.0.0.1", port), "监听关闭后应判定为空闲");
    }

    #[test]
    fn parses_pid_from_gateway_state() {
        let raw = r#"{"pid": 36348, "port": 18790, "launch_mode": "foreground"}"#;
        assert_eq!(json_u32_field(raw, "pid"), Some(36348));
        assert_eq!(json_u32_field(raw, "missing"), None);
    }

    #[test]
    fn tolerates_whitespace_in_state_json() {
        let raw = "{\n  \"pid\" :  40484\n}";
        assert_eq!(json_u32_field(raw, "pid"), Some(40484));
    }

    #[test]
    fn parses_string_field() {
        let raw = r#"{"gatewayId": "abc-123"}"#;
        assert_eq!(
            json_string_field(raw, "gatewayId").as_deref(),
            Some("abc-123")
        );
        assert_eq!(json_string_field(raw, "nope"), None);
    }

    #[test]
    fn string_field_rejects_non_string() {
        let raw = r#"{"pid": 12}"#;
        assert_eq!(json_string_field(raw, "pid"), None);
    }
}
