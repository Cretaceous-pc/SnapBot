# SnapBot

把 [nanobot](https://github.com/HKUDS/nanobot) 包装成 **Windows 桌面应用**：自带窗口（系统 WebView2），
用户双击即用，**不需要浏览器、不需要装 Python、不需要命令行**。

面向两种使用者：作者自己日常使用，以及**分发给他人安装**。

---

## 这个仓库跟踪什么

只跟踪**我们自己写的代码**。上游 nanobot 检出、第三方二进制、原档压缩包一律不入库
——`nanobot-0.3.5/` 有 **527 MB / 47406 个文件**，入库会毁掉仓库。

| 目录 | 是否入库 | 说明 |
|---|---|---|
| `desktop/` | ✅ | Tauri 桌面壳（Rust + 前端） |
| `packaging/` | ✅ | 把 Python 后端冻结成 sidecar 的脚本与 spec |
| `patches/` | ✅ | 对上游 nanobot 的补丁（**不 fork 上游**）→ [`patches/README.md`](patches/README.md) |
| `scripts/` | ✅ | 开发期辅助脚本（恢复上游等） |
| `docs/` | ✅ | 架构与设计决策记录 |
| `nanobot-0.3.5/` | ❌ | 上游源码检出，用 `scripts/restore-upstream.ps1` 恢复 |
| `nanobot-tui-win32-x64.exe/` | ❌ | 上游发布的 TUI 二进制（123 MB） |
| `原档/` | ❌ | 上游压缩包原档（48 MB），恢复上游的来源 |

## 为什么不用 git submodule 管上游

上游是**解压出来的源码包**，不是 clone（目录里没有 `.git`，已核实）。没有可指向的远程，
加上 47406 个文件，submodule/subtree 都只会带来痛苦。改用：

- 上游按 zip 原档保存（`原档/`），脚本一键恢复
- 我们对上游的**全部改动以 patch 形式入库**（`patches/`），可审查、可重放、升级上游时不会丢

## 架构

```
Nanobot.exe  (Tauri 壳, 系统 WebView2)
   │
   ├─ 1. 拉起 sidecar: nanobot-backend.exe   (PyInstaller 冻结的 Python 后端)
   ├─ 2. 轮询 /health 直到 ready
   ├─ 3. 复用 nanobot 的 _webui_browser_url() 取带 bootstrapSecret 的 URL
   ├─ 4. WebView 加载 http://127.0.0.1:<port>/   ← 前端一行都不用改
   └─ 5. 退出时停掉网关
```

关键约束（违反会白屏或连不上）：**WebView 必须加载 `http://127.0.0.1:<port>/`**，
不能用 `file://` 或自定义协议。原因见 [`docs/architecture.md`](docs/architecture.md)。

## 状态

- [x] 仓库初始化、分支策略确立 → [`docs/git-conventions.md`](docs/git-conventions.md)
- [x] Tauri 壳骨架：起网关 → 等就绪 → 开窗 → 退出回收
      → [`desktop/README.md`](desktop/README.md)
- [x] 开发期直连现有 `.venv`（**三条路径已实测**：附着已有网关 / 自行拉起并回收 / 首次运行引导）
- [x] 首次运行引导：无配置时走上游初始化路径（实测 1.6s 生成配置、4.3s 网关就绪）
- [x] 单实例锁（命名互斥体）、加载页进度与失败重试、日志落地 `~/.nanobot/logs/desktop.log`
- [x] 界面默认简体中文（注入 `nanobot.locale`，已截图实测）
- [x] 卸载清除 `%USERPROFILE%\.nanobot`（**Tauri 默认删不到这里**，见 packaging.md §4）
- [x] 安装程序中文（`nsis.languages = SimpChinese`）
- [x] 托盘图标：关窗隐藏到托盘，托盘菜单退出并回收网关（已实测）
- [x] 端口被占自动回退 + 记住实际端口（已实测）
- [x] 冻结 Python 后端为可分发目录（**120.7 MB**，已实测）
      → [`packaging/`](packaging/)
- [x] 功能减法：删除「自动任务」UI 与后端路由（**保留 `cron` 引擎与工具**）、
      删除浏览器端「注销」（保留供应商 OAuth 注销）
      → [`docs/auth-flow-analysis.md`](docs/auth-flow-analysis.md)
- [x] **NSIS 安装程序构建并实测**（`SnapBot_0.1.0_x64-setup.exe`，**259.3 MB**，
      WebView2 离线安装模式）→ [`docs/packaging.md`](docs/packaging.md)
      - **独立运行**：清空全部 `SNAPBOT_*` 环境变量后，后端取自安装目录
      - **升级不误删用户数据**（哨兵文件验证，51 → 51 文件）
      - **卸载无残留**：`~/.nanobot` + 安装目录 + 注册表项全部清除
      - **无控制台双击启动不崩**（windowed 冻结体 stdio 为 `None` 的坑）
      - **已删功能未进包**：核对 `installer.nsi` 清单，422/422 asset 匹配
- [x] WebView2 分发策略：`offlineInstaller`（安装包 +127 MB，免掉「装完白屏」）
- [x] 开始菜单补卸载入口（Tauri 模板只注册注册表项，用户找不到卸载）
- [x] **安装路径含中文/空格的兼容性**：实测装到 `D:\安装 测试\SnapBot 空格`，
      壳与冻结后端均从该路径启动，`/health` 就绪、WebUI HTTP 200、卸载清理干净
- [ ] 代码签名（当前 `NotSigned`，用户首启会遇到 SmartScreen「未知发布者」）

> 安装包与构建产物均不入库（`packaging/dist/`、`target/` 已在 `.gitignore`）。
> 重新构建：`.\packaging\build-backend.ps1` → `cd desktop\src-tauri; cargo tauri build`

详细计划与决策记录见 [`docs/`](docs/)。
