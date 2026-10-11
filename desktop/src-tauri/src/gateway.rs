//! 网关进程的生命周期管理。
//!
//! 设计决策：**优先附着到已在运行的网关，而不是无脑再起一个。**
//! 上游的 `nanobot gateway` 在检测到已有实例时会直接退出（gateway_already_running），
//! 如果我们不先探测，用户手动跑过一次 CLI 之后桌面壳就再也起不来了。

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, GatewayConfig};

/// 一次 TCP 连接尝试的超时。健康检查轮询用，必须短。
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// 启动网关后的最长等待时间。首次冷启动要初始化渠道与 cron，给足余量。
const READY_TIMEOUT: Duration = Duration::from_secs(90);

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
    pub fn python_path(&self) -> &std::path::Path {
        &self.python
    }

    /// 请求停止并等待退出；超时则强杀。
    ///
    /// 为什么要自己杀而不是调 `nanobot gateway stop`：我们持有 Child 句柄，
    /// 直接杀最可靠，也不依赖 CLI 是否可用（冻结打包后尤其重要）。
    pub fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return; // 已退出
        }
        #[cfg(windows)]
        {
            // 网关会拉起子进程（渠道、cron），杀整棵进程树
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

/// 定位 Python 解释器。
///
/// 顺序有意为之：
///   1. `SNAPBOT_PYTHON` —— 开发期与排障用的显式覆盖
///   2. 冻结打包的 sidecar（发布形态，最高优先级，因为它是自包含的）
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

    // 2) 同目录下的冻结后端（Tauri 会把 externalBin 放在可执行文件旁边）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("nanobot-backend.exe"));
            candidates.push(dir.join("backend").join("nanobot-backend.exe"));
        }
    }

    // 3) 开发检出
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from);
    if let Some(root) = workspace {
        candidates.push(root.join("nanobot-0.3.5").join(".venv").join("Scripts").join("python.exe"));
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

/// TCP 探测：端口上有人监听就认为健康端点可用。
///
/// 这里**不解析 /health 的响应体**：桌面壳只关心「网关是否已经能服务请求」，
/// 而是否 ready 由 WebUI 自己判断（它会展示模型未配置等状态）。
fn port_open(host: &str, port: u16) -> bool {
    let addr: SocketAddr = match format!("{host}:{port}").parse() {
        Ok(addr) => addr,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// 读 `run/gateway.json` 里记录的 pid，判断网关是否已在运行。
///
/// 不引第三方 JSON 库：这个文件里我们只需要一个数字字段。
fn recorded_gateway_pid() -> Option<u32> {
    let path = config::nanobot_data_dir().ok()?.join("run").join("gateway.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let key = "\"pid\"";
    let start = raw.find(key)? + key.len();
    let rest = &raw[start..];
    let colon = rest.find(':')? + 1;
    let digits: String = rest[colon..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
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
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains(&format!("\"{pid}\""))
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn pid_alive(pid: u32) -> bool {
    // POSIX: kill -0
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 确保网关可用。返回状态，调用方据其决定退出时是否回收。
pub fn ensure_running(cfg: &GatewayConfig) -> Result<GatewayState, String> {
    // ---- 1) 已经在跑？直接附着 ----
    if port_open(&cfg.health_host, cfg.health_port) {
        if let Some(pid) = recorded_gateway_pid() {
            if pid_alive(pid) {
                log_line(&format!("检测到已在运行的网关 (pid {pid})，附着而不重启"));
            }
        }
        return Ok(GatewayState::Attached);
    }

    // ---- 2) 起一个新网关 ----
    let python = resolve_python()?;
    log_line(&format!("启动网关: {} -m nanobot gateway --foreground --port {}",
        python.display(), cfg.health_port));

    let mut cmd = Command::new(&python);
    cmd.arg("-m")
        .arg("nanobot")
        .arg("gateway")
        .arg("--foreground")
        .arg("--port")
        .arg(cfg.health_port.to_string());

    // 关键：**不要把 bootstrapSecret 放进命令行**。它会出现在进程列表里。
    // 上游 `cli/windows_browser.py` 专门用「匿名管道 + ACL 保护文件」来避免这件事，
    // 我们通过「让 nanobot 自己从 config.json 读」达到同样的效果。
    cmd.current_dir(python.parent().and_then(|p| p.parent()).unwrap_or(&python));

    // 冻结打包后 sys.executable 是我们自己的 exe，上游的
    // `build_gateway_command` 会拼出 `<exe> -m nanobot gateway ...`。
    // 这里显式告诉后端它的真实角色（见 src/main.rs 的 argv 拦截）。
    cmd.env("SNAPBOT_FROZEN_CHILD", "1");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        // 不弹控制台窗口
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("启动网关失败（{}）: {err}", python.display()))?;

    // ---- 3) 等待健康端点就绪 ----
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            // 进程已经退了：把输出读出来给用户看，否则无从排障
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
            return Err(format!(
                "网关在就绪前退出（code {:?}）。输出：\n{}",
                status.code(),
                if detail.trim().is_empty() { "(无输出)".to_string() } else { detail }
            ));
        }

        if port_open(&cfg.health_host, cfg.health_port) {
            log_line("网关健康端点已就绪");
            return Ok(GatewayState::Spawned(GatewayProcess { child, python }));
        }

        if Instant::now() >= deadline {
            let mut process = GatewayProcess { child, python };
            process.shutdown();
            return Err(format!("等待网关就绪超时（{} 秒）", READY_TIMEOUT.as_secs()));
        }

        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 保留 stdout 不被父进程缓冲阻塞。
///
/// 网关会往 stdout/stderr 写日志。如果我们把管道设成 piped 却不读，
/// 子进程写满管道缓冲区（Windows 上约 4KB）后就会**阻塞在写日志上**，
/// 表现为「网关起来了但不干活」。所以起一个线程持续丢进日志。
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
    let stamp = timestamp();
    let line = format!("{stamp} [snapbot] {message}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("desktop.log"))
    {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
}

/// 本地时间戳，避免为一个时间格式化引入 chrono。
///
/// 用 `GetLocalTime` 而不是 `SystemTime`：后者是 UTC，日志里看到的时间会差 8 小时，
/// 排障时会和 nanobot 自己的日志对不上。
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
