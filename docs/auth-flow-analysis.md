# 认证流程分析：为什么「注销」是个伪功能，以及如何简化

> 本文是「删除浏览器端注销」这一改动的配套分析。结论：**注销已删除，auth 界面保留作为兜底**；
> 进一步简化需要改 Tauri 侧时序，建议作为独立一轮。

## 一、当前（注销删除前）的认证链路

```
config.json (tokenIssueSecret)
      │
      │ ① Tauri 启动时读取
      ▼
desktop/src-tauri/src/config.rs::browser_url()
      │  拼成  http://127.0.0.1:<port>/#/?bootstrapSecret=<secret>
      │  （走 URL fragment：不发往服务端、不进 Referer）
      ▼
WebView 加载该 URL
      │
      │ ② webui/src/lib/bootstrap.ts::consumeUrlBootstrapSecret()
      │    从 hash 里取出 secret，并立刻从 URL 中抹掉
      ▼
bootstrapWithSecret(secret)
      │
      │ ③ GET /webui/bootstrap   Header: X-Nanobot-Auth: <secret>
      ▼
网关返回 { token, ws_path, api_token, ... }
      │
      │ ④ saveSecret(secret) → localStorage["nanobot-webui.bootstrap-secret"]
      ▼
状态机 → status: "ready"，WebSocket 连上
```

关键实现点：

| 环节 | 位置 | 行为 |
|---|---|---|
| 注入 | `desktop/src-tauri/src/config.rs:108-114` | secret 为空时**不拼** fragment |
| 提取 | `webui/src/lib/bootstrap.ts:42-63` | 取出后 `history.replaceState` 抹掉 |
| 缓存 | `webui/src/lib/bootstrap.ts:25-31` | 存 localStorage，刷新页面免重输 |
| 校验 | `webui/src/lib/bootstrap.ts:69-94` | 401/403 → `BootstrapAuthRequiredError` |
| 兜底 | `App.tsx:954` | 捕获该错误 → `status: "auth"` 显示密码框 |

## 二、注销做了什么（以及为什么它是伪功能）

删除前的实现只有三行（`App.tsx:1034-1040`）：

```ts
const handleLogout = () => {
  if (state.status === "ready") state.client.close();  // 关掉前端 WebSocket
  clearSavedSecret();                                   // 删 localStorage 里的 secret
  setState({ status: "auth" });                         // 回到密码输入页
};
```

它**没有**任何服务端动作：没有吊销 token、没有改 `tokenIssueSecret`、没有撤销会话。
而真正的密码始终以明文躺在 `~/.nanobot/config.json` 的 `channels.websocket.tokenIssueSecret`
里 —— 用户随时可以打开文件抄回来。

所以「注销」的净效果是：

- ✅ **没**提升任何安全性（凭据未被吊销，且可从配置文件读回）
- ❌ 迫使桌面端用户手工重新输入一个本可自动读取的密码
- ❌ 语义误导：叫「注销」会让人以为凭据已失效

对**本地桌面应用**来说这是纯粹的负担。对「浏览器访问远程网关」的场景，注销也只是
「这个浏览器忘记密码」，而不是「服务端拒绝此浏览器」——把这两件事都叫「注销」本身就是
命名问题，也印证了你的直觉是对的。

## 三、auth 界面何时才真正可达

打包桌面端中，`auth` 界面**只在这三种情况下出现**（都是异常路径，不是正常步骤）：

1. **首次运行、配置尚未生成**  
   Tauri 先起网关、网关初始化写出 `config.json`，随后 Tauri **重新读一次**配置
   （`main.rs:138` 附近有注释说明这个顺序是刻意的）。但若网关初始化失败或超时，
   secret 读不到，注入的就是无 secret 的 URL。
2. **配置存在但 `tokenIssueSecret` 为空**  
   `browser_url()` 在 `secret.is_empty()` 时直接返回不带 fragment 的 base URL。
   若此时网关的 `websocket_requires_token` 仍为真，前端就会收到 401 → auth 界面。
3. **用户手改了密码，或清了 localStorage 且 URL 未带 secret**

正常启动路径下，用户**永远看不到**登录页 —— 这正是我建议保留它作兜底、但去掉主动
「注销」按钮的原因：它不再是「登出/登入」的一环，而只是配置读取失败时的补救入口。

## 四、简化方案

### 方案 A（已完成，本轮的改动）
**删除注销，保留 auth 界面作兜底。**

- 改动量：小
- 收益：去掉伪功能与语义误导；auth 界面回归「异常兜底」单一职责
- 风险：无（`clearSavedSecret` 函数保留，auth 兜底路径不受影响）

### 方案 B（已实施，见第七节）：让 Tauri 在配置缺失时自愈，而非让用户手输
**目标**：把上面的情况 1、2 也变成自动完成，使 auth 界面只在真正异常时出现。

现状的断点在于：`run_bootstrap` 只读一次配置。若首次初始化没能在读取前写出
`tokenIssueSecret`，secret 就丢了。

建议做法：

1. 在 `gateway::ensure_running` 返回后，**轮询等待** `tokenIssueSecret` 出现（带超时，
   例如 15s，间隔 250ms），而不是一次性读取；
2. 拿到 secret 后用 `webview.eval()` 或重新 `load_url()` 注入
   `#/?bootstrapSecret=...`，复用现有 `bootstrapWithSecret` 路径，前端零改动；
3. 超时仍拿不到，才落到 auth 界面并给出明确文案（「未能自动读取网关密钥，请检查
   config.json 的 channels.websocket.tokenIssueSecret」）。

- 改动面：`desktop/src-tauri/src/main.rs`（时序）+ `config.rs`（轮询辅助）
- 收益：打包端基本告别手输密码
- 风险：中等 —— 涉及启动时序与超时；需要真实安装包上验证首次运行与「配置被删后重启」
  两条路径。**已按此建议独立一轮实施并配合 NSIS 安装包实测，见第七节。**

### 方案 C（不推荐）：彻底删除 auth 界面
只有在「桌面端是唯一入口、且 secret 注入 100% 可靠」时才成立。
但配置损坏、端口占用回退、用户手工编辑配置等场景都可能导致注入失败，
届时用户会面对一个白屏且无从下手的应用。**保留兜底更稳妥。**

## 五、本轮实际改动清单

| 文件 | 改动 |
|---|---|
| `webui/src/App.tsx` | 删 `handleLogout`、`onLogout` 传递链、`clearSavedSecret` import |
| `webui/src/components/settings/SettingsSidebar.tsx` | 删注销按钮、`onLogout` prop、`LogOut` 图标 |
| `webui/src/components/settings/SettingsView.tsx` | 删 `onLogout` prop 与透传 |
| `webui/src/components/settings/SettingsPage.tsx` | 删 `onLogout` prop 与 `requestExit(onLogout)` 接线 |
| `webui/src/i18n/locales/*/common.json`（10 语言） | 删 `app.account.*` |

**明确保留**：`lib/bootstrap.ts` 的 `clearSavedSecret`（auth 兜底仍可能调用）、
auth 状态机、`SecretPrompt` 界面，以及**供应商 OAuth 注销**
（`settings.oauth.signOut`、`logoutProviderOAuth`、`nanobot provider logout`）——
那是真正删除已存储凭据的功能，与本改动无关。

## 六、验证结果

| 检查项 | 命令 | 结果 |
|---|---|---|
| WebUI 构建 | `cd webui && bun run build` | ✅ exit 0 |
| WebUI 类型检查 | `bunx tsc -p tsconfig.build.json` | ✅ exit 0，无错误 |
| WebUI 测试 | `bun run test` | ✅ 92/92 文件，1429/1429 通过 |
| Python lint | `uvx ruff check nanobot/` | ✅ All checks passed |
| Python 测试 | `pytest tests/webui nanobot/channels/websocket/tests` | ✅ 759 passed |
| Rust 检查 | `cd desktop/src-tauri && cargo check` | ✅ exit 0 |
| 遗留引用 | grep 非测试前端源码 + Python 源码 | ✅ 零残留 |

### ⚠️ 一个预先存在的 flake（与本改动无关）

`webui/src/tests/thread-shell.test.tsx:3564` 有一处时序敏感的断言：

```ts
await waitFor(() => expect(historyCalls).toBe(2));
```

它在无关本改动的情况下**自身就会间歇失败**。证据：

1. 该文件与 `原档/nanobot-0.3.5.zip` 里的上游原版**SHA256 完全相同**
   （`9c582c96…`），即从未被本改动碰过；
2. 单独连跑 3 次：失败 / 通过 / 失败 —— 典型 flake；
3. 本文件与 `message-bubble.test.tsx` 一起单独跑：132/132 全通过。

因此全量跑偶尔出现 `1 failed | 1428 passed` 属正常，**不要**把它当成回归去「修」。
如需消除，应加大该 `waitFor` 的 timeout——但那属于独立的上游测试稳定性问题。

---

## 七、方案 B 实施与验证记录（2026-10-16）

### 改了什么

| 文件 | 改动 |
|---|---|
| `desktop/src-tauri/src/config.rs` | 新增 `wait_for_secret(timeout, interval, load)`：轮询直到 `tokenIssueSecret` 非空。`load` 作为参数注入，因此可用脚本化的假读取做单元测试。超时返回 `SecretWaitOutcome { last, attempts, waited }` —— **把最后一次读到的配置交出来**，否则超时会让引导在无关的地方失败 |
| `desktop/src-tauri/src/main.rs` | 第三步由「首次运行时读一次配置」改为「secret 为空时轮询等待」；新增 `SECRET_WAIT_TIMEOUT = 15s` / `SECRET_WAIT_INTERVAL = 250ms`；`Bootstrap` 新增 `warning` 字段并经 `bootstrap_status` 暴露；`bootstrap_retry` 清空 warning |
| `desktop/dist/index.html`、`loading.html` | 新增非致命提示块：就绪但带 warning 时，先讲清楚再进入（10 秒倒计时 + 「立即进入」按钮），而不是让用户一脸茫然地看到密码框 |

**常规启动零开销**：secret 本来就在配置里时，`cfg.secret.is_empty()` 为假，
整个轮询分支不进入，启动时序与改动前完全一致。

### 改前 / 改后

| 场景 | 改前 | 改后 |
|---|---|---|
| 常规启动（secret 已在） | 直接进 WebUI | 直接进 WebUI（不变） |
| 首次运行、配置尚未生成 | 读一次配置；读早了就丢掉 secret → 密码框 | 轮询等到写入为止（实测 0ms 命中，因为网关就绪时已写好） |
| 配置存在但 secret 为空 | 立即以无凭据 URL 开窗 | 等 15 秒；超时则**先给出明确文案**再进入 |
| 配置一直读不到（全失败） | 引导失败 | 超时后用原配置（含端口）继续启动，不额外失败 |

### 验证（真实安装包，非开发形态）

安装包重新构建：`SnapBot_0.1.0_x64-setup.exe`，259.3 MiB，
SHA256 `8E86A9B0DB2B55B6811FF766AABCE3404750D63D5211EC7BE586A10309D7C1A4`，
静默安装后占用仍 130.6 MB。

| 验证项 | 方法 | 结果 |
|---|---|---|
| Rust 单元测试 | `cargo test` | ✅ 22/22 通过（含 5 条新增：立即命中 / 等到第三次出现 / 持续读错超时 / 超时交出 last / 超时常量下界） |
| **首次运行自愈** | 删掉 `~/.nanobot` 后启动安装版 | ✅ 日志 `已等到网关写入 tokenIssueSecret` → `首次运行完成：凭据 已生成`；`/health` 6.4s 就绪、WebUI HTTP 200；截图确认**直接进入 WebUI，无密码框** |
| **secret 为空** | 把 `tokenIssueSecret` 改成空串后启动 | ✅ 日志 `等待 15 秒（61 次读取）仍未读到 tokenIssueSecret`；截图确认提示页文案与「立即进入（10）」倒计时 |
| 兜底落点 | 上一条继续观察 | ✅ 空 secret 时上游按 localhost 判定放行，仍进入 WebUI（未出现密码框） |
| 卸载清理 | `uninstall.exe /S` | ✅ 安装目录 / 注册表 / 开始菜单 / `~/.nanobot` 全部清除 |

> 结论：打包端正常情况下**不会**再看到密码框；只有配置真的读不到时才会出现，
> 而且出现前一定有中文说明。

### 踩坑（与功能无关，但值得记）

1. **截图截错窗口**：`SetForegroundWindow` 在终端占用前台时会被系统拒绝，
   第一次截到的是浏览器。改用 `PrintWindow(hwnd, ..., PW_RENDERFULLCONTENT=0x2)`
   直接取窗口像素，与 z-order 无关。
2. **截图被放大裁切**：PowerShell 进程默认 DPI-unaware，`GetWindowRect` 返回的是
   *虚拟化*后的逻辑尺寸，`PrintWindow` 画出来就变成放大的局部。
   先调 `SetProcessDPIAware()` 才拿到 1:1 的图（实测 1293×896 → 2586×1791）。
3. **测试脚本里的中文匹配全部失效**：脚本正文是 ASCII-only，但我在里面写了中文
   日志关键字，PS 5.1 按 GBK 解码后正则永远匹配不到——表现为「日志里明明有，
   脚本却说没等到」。改成用码点拼关键字（`[char]0x4ECD…`）。
   同一个坑本项目已经踩过三次（`.cmd`、`.ps1`、以及子代理改中文测试文件）。

