//! 读取 nanobot 的 config.json，算出桌面壳需要的一切。
//!
//! 为什么不解析 `nanobot webui` 的 stdout：那个输出格式没有稳定性承诺，
//! 而且 bootstrapSecret 会经过管道——我们从配置里直接读，等价且更稳。
//! 对应上游实现见 `nanobot/cli/webui_support.py::_webui_browser_url`。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// nanobot 数据目录：`~/.nanobot`（上游 get_data_dir 的约定）
pub fn nanobot_data_dir() -> Result<PathBuf, String> {
    if let Some(override_dir) = env::var_os("SNAPBOT_DATA_DIR") {
        return Ok(PathBuf::from(override_dir));
    }
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .ok_or("找不到 USERPROFILE / HOME，无法定位 nanobot 数据目录")?;
    Ok(Path::new(&home).join(".nanobot"))
}

pub fn config_path() -> Result<PathBuf, String> {
    Ok(nanobot_data_dir()?.join("config.json"))
}

// ---- 只声明我们真正需要的字段，其余由 serde 忽略 -------------------------

#[derive(Debug, Deserialize)]
struct Root {
    #[serde(default)]
    channels: Channels,
    #[serde(default)]
    gateway: Gateway,
}

#[derive(Debug, Default, Deserialize)]
struct Channels {
    #[serde(default)]
    websocket: WebSocket,
}

#[derive(Debug, Default, Deserialize)]
struct WebSocket {
    #[serde(default)]
    enabled: bool,
    host: Option<String>,
    port: Option<u16>,
    #[serde(rename = "tokenIssueSecret")]
    token_issue_secret: Option<String>,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Gateway {
    host: Option<String>,
    port: Option<u16>,
}

impl Default for Gateway {
    fn default() -> Self {
        // 与上游 schema 默认值保持一致
        Self { host: None, port: Some(18790) }
    }
}

/// 桌面壳启动网关所需的最小配置快照。
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// WebUI / WebSocket 监听端口（前端从这里加载）
    pub webui_port: u16,
    /// WebUI 绑定主机。非回环时必须显式提供 tokenIssueSecret。
    pub webui_host: String,
    /// 健康检查端口
    pub health_port: u16,
    pub health_host: String,
    /// bootstrap 凭据；空字符串表示上游会走「仅限 localhost」判定
    pub secret: String,
    pub websocket_enabled: bool,
    pub config_path: PathBuf,
}

impl GatewayConfig {
    /// 前端要加载的 URL。等价于上游 `_webui_browser_url()`。
    ///
    /// bootstrapSecret 走 URL fragment：fragment 不会发给服务端、
    /// 不进 Referer，是上游刻意的设计（见 `cli/windows_browser.py`）。
    pub fn browser_url(&self) -> String {
        let base = format!("http://{}:{}", host_for_url(&self.webui_host), self.webui_port);
        if self.secret.is_empty() {
            return base;
        }
        format!("{base}/#/?bootstrapSecret={}", percent_encode(&self.secret))
    }

    /// 健康检查 URL。目前探测走 TCP 连接（见 `gateway::port_open`），
    /// 保留此方法供后续需要解析 `/health` 响应体时使用。
    #[allow(dead_code)]
    pub fn health_url(&self) -> String {
        format!("http://{}:{}/health", host_for_url(&self.health_host), self.health_port)
    }
}

/// 上游 `_host_for_local_browser` 的等价物：通配绑定要换成回环地址才能访问。
fn host_for_url(host: &str) -> String {
    match host.trim() {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}

/// 上游 `quote(secret, safe='')` 的等价物：除 `A-Za-z0-9-._~` 外全部百分号编码。
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// 读取并解析配置。文件不存在不算错误——上游首次运行会自动生成。
pub fn load() -> Result<GatewayConfig, String> {
    let path = config_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "还没有配置文件：{}\n请先用 nanobot 初始化（或让桌面壳首次运行时引导创建）。",
                path.display()
            ));
        }
        Err(err) => return Err(format!("读取配置失败 {}: {err}", path.display())),
    };

    let root: Root = serde_json::from_str(&raw)
        .map_err(|err| format!("解析配置失败 {}: {err}", path.display()))?;

    let ws = root.channels.websocket;
    let secret = ws
        .token_issue_secret
        .or(ws.token)
        .unwrap_or_default()
        .trim()
        .to_string();

    Ok(GatewayConfig {
        webui_port: ws.port.unwrap_or(8765),
        webui_host: ws.host.unwrap_or_else(|| "127.0.0.1".to_string()),
        health_port: root.gateway.port.unwrap_or(18790),
        health_host: root.gateway.host.unwrap_or_else(|| "127.0.0.1".to_string()),
        secret,
        websocket_enabled: ws.enabled,
        config_path: path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_without_secret_is_bare_base() {
        let cfg = GatewayConfig {
            webui_port: 8765,
            webui_host: "127.0.0.1".into(),
            health_port: 18790,
            health_host: "127.0.0.1".into(),
            secret: String::new(),
            websocket_enabled: true,
            config_path: PathBuf::from("x"),
        };
        assert_eq!(cfg.browser_url(), "http://127.0.0.1:8765");
    }

    #[test]
    fn url_encodes_secret() {
        let cfg = GatewayConfig {
            webui_port: 8765,
            webui_host: "127.0.0.1".into(),
            health_port: 18790,
            health_host: "127.0.0.1".into(),
            secret: "a b+c/d?e".into(),
            websocket_enabled: true,
            config_path: PathBuf::from("x"),
        };
        assert_eq!(
            cfg.browser_url(),
            "http://127.0.0.1:8765/#/?bootstrapSecret=a%20b%2Bc%2Fd%3Fe"
        );
    }

    #[test]
    fn wildcard_bind_maps_to_loopback() {
        assert_eq!(host_for_url("0.0.0.0"), "127.0.0.1");
        assert_eq!(host_for_url("::"), "127.0.0.1");
        assert_eq!(host_for_url("localhost"), "localhost");
    }
}
