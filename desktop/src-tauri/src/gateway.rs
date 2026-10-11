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

pub struct GatewayProcess {
    child: Child,
    /// 记录实际使用的解释器，排障时第一眼要看的东西
    python: PathBuf,
}

impl GatewayProcess {
    /// 实际使用的 Python 解释器路径（写日志/展示用）。
    pub fn python_path(&self) -> &Path {
        &self.python
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

/// 定位 Python 解释器。
///
/// 顺序有意为之：
///   1. `SNAPBOT_PYTHON` —— 开发期与排障用的显式覆盖
///   2. 冻结打包的 sidecar（发布形态，自包含，最优先）
///   3. 上游开发检出里的 `.venv`
pub fn resolve_python() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("SNAPBOT_PYTHON") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("SNAPBOT_PYTHON 指向的文件不存在: {}", path.display()));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("nanobot-backend.exe"));
            candidates.push(dir.join("backend").join("nanobot-backend.exe"));
        }
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from);
    if let Some(root) = workspace {
        candidates.push(
            root.join("nanobot-0.3.5")
                .join(".venv")
                .join("Scripts")
                .join("python.exe"),
        );
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    Err(format!(
        "找不到 Python 解释器。已尝试：\n{}\n\
         开发期请设置 SNAPBOT_PYTHON 指向 nanobot 的 venv python.exe。",
        candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

// ---------------------------------------------------------------- 探测

/// 向 `/health` 发一个最小 HTTP/1.0 GET，确认对面**确实是 nanobot 网关**。
///
/// 为什么不能只做 `TcpStream::connect`：端口上可能蹲着任何别的程序。
/// 只测端口连通会把陌生服务误判成「网关已在运行」，然后「附着」上去，结果是白屏。
/// 这里要求响应里带 `"status"` 字段才认。
fn probe_health(host: &str, port: u16) -> bool {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));

    let request =
        format!("GET /health HTTP/1.0\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    // HTTP/1.0 + Connection: close 会让对端主动结束，读不动就当作不可用。
    let _ = stream.read_to_string(&mut response);
    response.contains("\"status\"")
}

// ---------------------------------------------------------------- 启动网关

/// 起一个新网关。返回状态供调用方决定退出时是否回收。
pub fn ensure_running(cfg: &GatewayConfig) -> Result<GatewayState, String> {
    let python = resolve_python()?;

    // ---- 1) 已经在跑？直接附着（要求 /health 真的应了）----
    if probe_health(&cfg.health_host, cfg.health_port) {
        if let Some(pid) = recorded_gateway_pid() {
            if pid_alive(pid) {
                log_line(&format!("检测到已在运行的网关 (pid {pid})，附着而不重启"));
            } else {
                log_line("健康端点有响应，但记录的 pid 已不存在，仍按附着处理");
            }
        }
        return Ok(GatewayState::Attached);
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

    let mut cmd = Command::new(&python);
    cmd.arg("-m").arg("nanobot");
    let mode = if first_run {
        cmd.arg("webui").arg("--yes").arg("--no-open");
        "webui --yes --no-open".to_string()
    } else {
        cmd.arg("gateway")
            .arg("--foreground")
            .arg("--port")
            .arg(cfg.health_port.to_string());
        format!("gateway --foreground --port {}", cfg.health_port)
    };

    // 关键：**不要把 bootstrapSecret 放进命令行**。它会出现在进程列表里。
    // 上游 cli/windows_browser.py 专门用「匿名管道 + ACL 保护文件」来规避这件事；
    // 我们通过「让 nanobot 自己从 config.json 读」达到同样效果。

    // 发布形态下前端产物已预构建并随包分发，不该在用户机器上重建
    // （既慢又要 bun、要网络）。开发形态不加这个变量，改前端源码后仍会自动重建。
    if !cfg!(debug_assertions) {
        cmd.env("NANOBOT_SKIP_WEBUI_BUILD", "1");
    }

    if let Some(root) = python.parent().and_then(|p| p.parent()) {
        cmd.current_dir(root);
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
        "启动{}({}): {} -m nanobot {mode}",
        if first_run { "（首次运行）" } else { "" },
        python.display(),
        if first_run { "" } else { "模式 " }
    ));

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("启动网关失败（{}）: {err}", python.display()))?;

    // ---- 3) 首次运行：等上游把 config.json 写出来 ----
    if first_run {
        if let Err(err) = wait_for_config(&mut child, &cfg.config_path) {
            let mut process = GatewayProcess { child, python };
            process.shutdown();
            return Err(err);
        }
    }

    // ---- 4) 轮询健康端点，确认真的能服务请求 ----
    let ready_timeout = if first_run { BOOTSTRAP_TIMEOUT } else { READY_TIMEOUT };
    if let Err(err) = wait_for_health(&mut child, cfg, ready_timeout) {
        let mut process = GatewayProcess { child, python };
        process.shutdown();
        return Err(err);
    }

    log_line("网关健康端点已就绪");
    Ok(GatewayState::Spawned(GatewayProcess { child, python }))
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
    cfg: &GatewayConfig,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(err) = child_exited(child) {
            return Err(err);
        }
        if probe_health(&cfg.health_host, cfg.health_port) {
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
