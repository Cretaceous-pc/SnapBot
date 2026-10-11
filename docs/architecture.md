# 架构与关键约束

本文件记录**已通过阅读上游源码核实**的事实，以及由其推出的设计约束。
每条都标注了来源位置，便于上游升级后重新核对。

---

## 1. 上游服务端形态

网关**不是** FastAPI / uvicorn，而是 nanobot 自己实现的异步 HTTP + WebSocket 服务。

| 事实 | 来源 |
|---|---|
| 前端是 Vite 构建的**静态 SPA**，当前 438 文件 / **6.17 MB** | `nanobot/web/dist/` 实测 |
| 静态产物由**网关自己**服务 | `nanobot/webui/ws_http.py` → `self.static_dist_path` |
| 运行时**不需要** Node / Bun / Vite | 静态产物已构建完成；bun 仅用于重建前端 |

**推论**：桌面壳只需一个 WebView 指向网关地址，**前端代码一行都不用改**。
语言包、设置面板、会话历史、流式输出全部照旧。

---

## 2. 硬约束：WebView 必须加载 `http://127.0.0.1:<port>/`

bootstrap 凭据（`/webui/bootstrap`）与 Full Access 权限都由这个判定保护：

```python
# nanobot/webui/http_utils.py:297
def is_local_browser_request(connection, headers) -> bool:
    if not is_localhost(connection):          # TCP 对端必须是回环
        return False
    host = case_insensitive_header(headers, "Host")
    if not is_loopback_host(host):            # Host 必须是 127.0.0.1 / localhost
        return False
    return _all_forwarded_values_are_loopback(headers)
```

判定**只看**三样：TCP 对端、`Host` 头、转发头（`X-Forwarded-*` / `Forwarded` / `X-Real-IP`）。
**不校验** `Origin`，也不校验 `Sec-Fetch-Site`。

| 加载方式 | 结果 |
|---|---|
| `http://127.0.0.1:<port>/` | ✅ 对端是回环、Host 正确，全部通过 |
| `file://.../index.html` | ❌ Host 不是回环 → bootstrap 401/403 → **界面开着但连不上后端** |
| 自定义协议 `app://` | ❌ 同上 |

> ⚠️ 这是本项目最容易踩的坑：把打包进安装目录的 `index.html` 直接用 WebView 打开"看起来能跑"，
> 实际连不上。**dist 必须继续由 Python 服务端吐出来。**

---

## 3. 拿 URL 的正确姿势：复用上游现成函数

```python
# nanobot/cli/webui_support.py:314
def webui_bootstrap_secret(config) -> str:
    ws_cfg = _webui_config_dict(config)
    return str(ws_cfg.get("tokenIssueSecret") or ws_cfg.get("token") or "").strip()

# nanobot/cli/webui_support.py:320
def _webui_browser_url(config) -> str:
    host = _host_for_local_browser(str(ws_cfg.get("host") or "127.0.0.1"))
    port = int(ws_cfg.get("port") or 8765)
    base_url = f"http://{host}:{port}"
    secret = webui_bootstrap_secret(config)
    if not secret:
        return base_url
    return f"{base_url}/#/?bootstrapSecret={quote(secret, safe='')}"
```

**不要自己拼这个 URL。** bootstrapSecret 通过 URL fragment 传递，且
`nanobot/cli/windows_browser.py` 专门用「ACL 保护的私有 HTML 重定向文件 + 匿名管道」
来避免把凭据放进进程参数（那样会在进程列表/日志里泄露）。我们复用同样的路径。

---

## 4. 进程模型：必须复用上游的租约逻辑

```
nanobot/cli/tui_launcher.py   注入 NANOBOT_TUI_* 环境变量（TUI 才需要）
nanobot/gateway/runtime.py    GatewayRuntime / GatewayClientLease
    start_on_demand()         本地客户端租约，最后一个客户端退出才回收
    start_background()        持久化（--background）
```

`build_gateway_command()` 拉起子进程的方式：

```python
# nanobot/gateway/runtime.py:107
[python_executable, "-m", "nanobot", "gateway", "--foreground", "--port", str(port), ...]
#  python_executable 默认 sys.executable
```

**两条设计决策**：

1. **复用 `GatewayClientLease`，不要自己 `Popen`**。租约语义（on_demand 回收、看门狗、
   最后一个客户端退出才停）已经写好且有测试覆盖。自己起进程很容易做出"桌面应用关了、
   网关还在后台占端口"的孤儿进程。
2. **冻结打包后 `sys.executable` 会指向我们的 exe**，`<exe> -m nanobot gateway ...` 必然崩。
   必须在 exe 入口最先拦截这个精确 argv 前缀并转发给 `nanobot` 的 CLI main。
   详见 [`packaging.md`](packaging.md)。

---

## 5. 上游已预留的桌面集成点

`nanobot/cli/desktop_tui.py` 表明作者已经设计了「把客户端附着到已运行的桌面实例」协议：

- `NANOBOT_TUI_DESKTOP_RESOLVER` / `NANOBOT_TUI_DESKTOP_TARGET`（含 `instanceId`、`gatewayId`）
- `--desktop-protocol` 能力探测，退出码 `90` 表示「切换到 Desktop」
- 解析器要求 stdout 是管道（凭据只允许经管道传给客户端）

**设计决策**：跟随这个模式，不要另起一套。将来上游更新时不会和我们的壳打架。

---

## 6. 运行时数据必须留在用户目录

| 路径 | 内容 |
|---|---|
| `~/.nanobot/config.json` | 配置（**含 API key**） |
| `~/.nanobot/workspace/` | 工作区、`AGENTS.md` / `SOUL.md` / `USER.md` / `HEARTBEAT.md` |
| `~/.nanobot/sessions/` | 会话历史 |
| `~/.nanobot/logs/` | 日志 |
| `~/.nanobot/run/` | 网关状态、`gateway.json` |
| `~/.nanobot/llm_usage.sqlite3` | 用量统计 |

放进安装目录的后果：升级覆盖用户数据；且 `Program Files` 默认不可写，直接起不来。

---

## 7. 待确认 / 未查明

| 项 | 状态 |
|---|---|
| `~/.nanobot/config.json` 里 API key 的注入点 | **未查明**。已排除：`DEEPSEEK_API_KEY` 环境变量（User/Machine 级均未设）、`~/.nanobot` 下独立凭据文件（无 keyring/secrets.json）、仓库 `.env`。删除 config.json 重建后 key 仍在（指纹一致）。 |
| 上游是否计划官方桌面版 | 未知。`desktop_tui.py` 的存在暗示有过桌面化设计，但当前仓库无桌面壳代码。 |
| WebView2 在目标用户机器上的覆盖率 | Win11 自带；Win10 需确认。打包时用 Tauri 的 WebView2 bootstrapper 选项。 |
