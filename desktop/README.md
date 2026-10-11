# SnapBot 桌面壳

基于 Tauri v2 的 Windows 外壳：**系统 WebView2 + 冻结的 Python 后端**，用户双击即用。

## 开发运行

前置：Rust 工具链、WebView2 运行时（Win11 自带）、以及一个可用的 nanobot 检出。

```powershell
# 后端：上游检出 + venv（若还没恢复）
.\scripts\restore-upstream.ps1

# 桌面壳：指向那个 venv 的 python
$env:SNAPBOT_PYTHON = 'D:\Desktop\SnapBot\nanobot-0.3.5\.venv\Scripts\python.exe'
cd desktop\src-tauri
cargo run
```

`cargo run` 会：读 `~/.nanobot/config.json` → 算出带 `bootstrapSecret` 的 URL →
检查健康端口是否已有人监听（有就**附着**，不重启）→ 否则拉起
`python -m nanobot gateway --foreground` → 轮询就绪 → 窗口导航到 WebUI。

## 目录结构

```
desktop/
├─ dist/                    # 加载页（frontendDist）：启动进度 + 失败重试
│  ├─ index.html
│  └─ loading.html
└─ src-tauri/
   ├─ src/
   │  ├─ main.rs            # 窗口、引导状态机、退出时回收网关
   │  ├─ config.rs          # 读 config.json，等价于上游 _webui_browser_url()
   │  └─ gateway.rs         # Python 定位、附着或拉起、就绪探测、进程回收
   ├─ capabilities/         # 权限声明（仅 core:default）
   ├─ icons/                # 应用图标
   ├─ Cargo.toml
   └─ tauri.conf.json
```

## 几个刻意的设计决策

**1. 加载页 + 轮询，而不是「Rust 等好再建窗口」**

窗口先出现，用户立刻看到进度。冷启动时网关要初始化渠道与 cron，可能几秒到十几秒；
若等就绪再建窗口，用户双击后会以为程序没反应。

**2. 优先附着到运行中的网关**

上游 `nanobot gateway` 检测到已有实例会直接退出。如果桌面壳不先探测健康端口就无脑起进程，
那么用户手动跑过一次 CLI 之后，桌面壳就再也起不来了。所以先探 `gateway.port`，
通则直接附着，并且**退出时不动它**（谁起的谁负责回收）。

**3. 探测必须校验响应体，不能只看端口通不通**

最初只用 `TcpStream::connect` 判断「网关是否在跑」。这有个真实缺陷：端口上可能蹲着
**别的程序**，于是会被误判成网关已在运行 → 「附着」上去 → 白屏。
现在改为向 `/health` 发一个最小 HTTP/1.0 GET，并要求响应里带 `"status"` 字段。

**4. 首次运行走上游自己的初始化路径**

配置不存在时不自己手搓 `config.json`——那等于把上游的 schema 与校验规则复制一份，
早晚漂移。改为调用上游 `nanobot webui --yes --no-open`：它会生成 schema 正确的配置、
启用本地 WebUI 通道、把 `tokenIssueSecret` 写进去，然后拉起网关。

**顺序很关键**：先起网关、再读配置。因为首次运行时配置文件是**网关写出来的**，
反过来做必然失败。首次运行结束后还要**重新读一次**配置——`tokenIssueSecret`
是那次初始化才生成的，用占位值开窗会连不上后端。

**5. bootstrapSecret 绝不进命令行**

上游 `cli/windows_browser.py` 专门用「匿名管道 + ACL 保护的重定向文件」来避免凭据出现在
进程参数里（进程列表、崩溃转储、日志都可能泄露）。我们的做法等价：**让 nanobot 自己从
`config.json` 读**，我们只在内存里拼 URL，通过 `location.replace()` 交给 WebView。

**6. 必须排空子进程的 stdout/stderr**

我们以 `piped` 打开网关的输出。如果不读，子进程写满管道缓冲区（Windows 约 4KB）后
会**阻塞在写日志上**，表现为「网关起来了但不干活」。所以 `drain_output()` 起线程持续读取。

**7. 退出时用 `taskkill /T` 杀整棵树**

网关会再拉起子进程（渠道、cron 任务）。只杀直接子进程会留下孤儿占住端口。

**8. 单实例用命名互斥体，不用锁文件**

第二个实例探测到端口被监听（那正是第一个实例起的网关）就会「附着」并开出第二个窗口，
两个窗口共用同一网关，观感很差。用 `CreateMutexW` 而不是锁文件：进程崩溃时内核自动释放，
不会留下需要手工清理的陈旧锁——锁文件的经典故障正是「崩溃后永远起不来」。

**9. 发布形态禁止重建前端**

前端产物（`nanobot/web/dist`，6.17MB）随包分发。发布形态设
`NANOBOT_SKIP_WEBUI_BUILD=1`，避免在用户机器上重新构建——那既慢，又要求用户有 bun 和网络。
开发形态不加这个变量，改前端源码后仍会自动重建。

## 已知限制

| 限制 | 说明 |
|---|---|
| 未签名 | 分发时 Windows SmartScreen 会提示「未知发布者」。见 [`../docs/packaging.md`](../docs/packaging.md) |
| 端口被别的程序占用 | 现在能正确识别（`/health` 不响应 → 判定为「没有网关」）；随后起网关若端口真被占，会带着网关的退出码与输出报错。尚未做「自动换端口」 |
| 无托盘图标 | 关闭窗口即退出（并回收由本应用启动的网关） |
| 首次运行需联网构建 | 开发形态下首次运行要跑上游初始化并构建前端（约 90 秒）。发布形态因预置 `dist/` 不需要这步 |

