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

**3. bootstrapSecret 绝不进命令行**

上游 `cli/windows_browser.py` 专门用「匿名管道 + ACL 保护的重定向文件」来避免凭据出现在
进程参数里（进程列表、崩溃转储、日志都可能泄露）。我们的做法等价：**让 nanobot 自己从
`config.json` 读**，我们只在内存里拼 URL，通过 `location.replace()` 交给 WebView。

**4. 必须排空子进程的 stdout/stderr**

我们以 `piped` 打开网关的输出。如果不读，子进程写满管道缓冲区（Windows 约 4KB）后
会**阻塞在写日志上**，表现为「网关起来了但不干活」。所以 `drain_output()` 起线程持续读取。

**5. 退出时用 `taskkill /T` 杀整棵树**

网关会再拉起子进程（渠道、cron 任务）。只杀直接子进程会留下孤儿占住端口。

## 已知限制

| 限制 | 说明 |
|---|---|
| 未签名 | 分发时 Windows SmartScreen 会提示「未知发布者」。见 [`../docs/packaging.md`](../docs/packaging.md) |
| 端口写死读配置 | 目前直接用 `config.json` 里的端口；若该端口被**别的程序**占用，会误判为「网关已在运行」。待加入进程校验 |
| 日志仅 stderr | 开发期够用；发布形态应写入 `~/.nanobot/logs/desktop.log` |
| 无托盘图标 | 关闭窗口即退出（并回收网关）。后续可加最小化到托盘 |
