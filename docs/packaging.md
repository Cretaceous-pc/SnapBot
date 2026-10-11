# 打包与分发

> **状态：设计稿，尚未实施。** 桌面壳的开发期形态（直连 `.venv`）已跑通并验证，
> 本文记录发布形态要做什么、以及哪些地方一定会踩坑。
> 按既定节奏：**先把开发形态跑顺，最后才碰打包。**

---

## 目标形态

```
SnapBot_0.1.0_x64-setup.exe          ← 一个安装程序，用户双击装完即用
  └─ 安装后
     ├─ SnapBot.exe                  ← Tauri 壳（Rust + 系统 WebView2，~10MB）
     ├─ nanobot-backend.exe          ← PyInstaller onedir 冻结的 Python 后端
     └─ backend/                     ← 后端依赖（onedir 形态的 _internal 等）
```

用户不需要装 Python、不需要命令行、不需要浏览器。

---

## 第一部分：把 Python 后端冻成 sidecar

### 1.1 必须先解决的：`sys.executable` 自举陷阱

上游拉起网关子进程的方式（`nanobot/gateway/runtime.py:107`）：

```python
[python_executable, "-m", "nanobot", "gateway", "--foreground", "--port", ...]
#  python_executable 默认 sys.executable
```

冻结后 `sys.executable` 指向 `nanobot-backend.exe`，于是上游会执行：

```
nanobot-backend.exe -m nanobot gateway --foreground --port 18790
```

**`-m` 对冻结的 exe 没有意义，必然失败。** 必须在入口最前面拦截：

```python
# packaging/backend_entry.py
import sys

def main() -> int:
    # 冻结后上游会用 `<exe> -m nanobot ...` 自举，转成正常 CLI 调用
    if sys.argv[1:3] == ["-m", "nanobot"]:
        sys.argv = [sys.argv[0], *sys.argv[3:]]
        from nanobot.cli.entry import main as nanobot_main
        nanobot_main()
        return 0
    # 直接运行（桌面壳拉起我们）
    from nanobot.cli.entry import main as nanobot_main
    nanobot_main()
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
```

> 另有一条更彻底的路：**冻结时不用上游的 CLI 自举**，而是让桌面壳直接把
> `--foreground` 网关跑在同一个进程里（`nanobot.gateway` 的 asyncio 入口）。
> 少一层进程、少一个坑，但会失去上游的租约/看门狗保护。当前选择拦截法。

### 1.2 PyInstaller 必须显式打包的资源

`pyproject.toml` 的 `include` / `artifacts` 声明了这些非 `.py` 文件，PyInstaller
**不会自动收集**，漏了会以各种诡异方式失败：

| 资源 | 漏掉的症状 |
|---|---|
| `nanobot/web/dist/**` | 窗口打开是白屏（前端产物 6.17MB / 438 文件） |
| `nanobot/templates/**/*.md` | 首次运行不生成 `AGENTS.md` / `SOUL.md` / `USER.md` / `HEARTBEAT.md` |
| `nanobot/skills/**/*.md`、`**/*.sh` | 技能系统空掉 |
| `nanobot/channels/*/webui/**` | 部分渠道的设置面板加载失败 |

`.spec` 片段：

```python
datas = [
    ("../nanobot-0.3.5/nanobot/web/dist", "nanobot/web/dist"),
    ("../nanobot-0.3.5/nanobot/templates", "nanobot/templates"),
    ("../nanobot-0.3.5/nanobot/skills", "nanobot/skills"),
]
# 渠道 webui 目录逐个收集
for p in Path("../nanobot-0.3.5/nanobot/channels").glob("*/webui"):
    datas.append((str(p), f"nanobot/channels/{p.parent.name}/webui"))
```

### 1.3 用 onedir，不要 onefile

| | onefile | **onedir（选这个）** |
|---|---|---|
| 杀软误报 | 高（自解压行为） | 低 |
| 启动速度 | 每次解压到 temp，慢 | 直接加载 |
| 分发 | 单文件好看 | 一个目录，但由安装程序封装，用户无感 |

### 1.4 运行时数据绝不能打进包

| 路径 | 必须在用户目录 |
|---|---|
| `~/.nanobot/config.json` | 含 API key |
| `~/.nanobot/workspace/` | 用户工作区 |
| `~/.nanobot/sessions/` | 会话历史 |
| `~/.nanobot/logs/` | 日志 |

打进包的后果：升级覆盖用户数据；装到 `Program Files` 下默认不可写，直接起不来。

---

## 第二部分：Tauri 侧

### 2.1 sidecar 声明

`tauri.conf.json`：

```json
{
  "bundle": {
    "externalBin": ["binaries/nanobot-backend"],
    "resources": ["binaries/backend/**"]
  }
}
```

`externalBin` 要求文件名带 target triple，例如
`binaries/nanobot-backend-x86_64-pc-windows-msvc.exe`。

> ⚠️ **待定**：当前 `gateway.rs::resolve_python()` 是自己找解释器并
> `python -m nanobot gateway`。切到 sidecar 后要改成直接执行
> `nanobot-backend.exe gateway --foreground --port ...`，
> 并把 `resolve_python()` 换成 `resolve_backend()`。
> 两处都要改，不要只改一处。

### 2.2 WebView2 运行时

Win11 自带；Win10 不一定有。Tauri 的选项：

```json
{ "bundle": { "windows": { "webviewInstallMode": { "type": "downloadBootstrapper" } } } }
```

| 模式 | 代价 |
|---|---|
| `downloadBootstrapper` | 安装包小，但安装时需要联网 |
| `embedBootstrapper` | 安装包 +1.8MB，仍需联网 |
| `offlineInstaller` | 安装包 +127MB，离线可装 |
| `skip` | 最小，但用户可能打不开（白屏） |

面向不确定的他人机器，建议 **`offlineInstaller`**：多 127MB 换掉一整类
「装完打开是白屏」的客服问题。

---

## 第三部分：安装与签名

### 3.1 安装程序

Tauri 内置 NSIS 与 MSI(WiX) 两种。选 **NSIS**：不需要额外装 WiX 工具链，
且支持中文界面与自定义安装路径。

```powershell
cd desktop\src-tauri
cargo tauri build
# 产物：target\release\bundle\nsis\SnapBot_0.1.0_x64-setup.exe
```

### 3.2 代码签名（发给别人就绕不过去）

未签名的 exe 会遇到：

- 首次运行 Windows SmartScreen：「Windows 已保护你的电脑 / 未知发布者」
- 部分杀软直接隔离 PyInstaller 产物

| 方案 | 成本 | 效果 |
|---|---|---|
| 不签名 | 0 | 每次都要教用户点「仍要运行」 |
| OV 证书 | 约 $100–300/年 | 消除「未知发布者」，但**新证书仍可能触发 SmartScreen 信誉积累期** |
| EV 证书 | 更贵，需硬件 token/云签名 | 立即获得信誉，SmartScreen 基本不拦 |

> 实话：这一项**和选 Tauri 还是 pywebview 完全无关**。任何未签名的分发形态都会遇到。

### 3.3 首次运行引导

用户装完打开时，`~/.nanobot/config.json` 还不存在。当前实现会直接失败并提示
「还没有配置文件」。发布前需要改成**引导流程**：

1. 检测配置不存在 → 显示内置的设置页（而不是错误）
2. 让用户填 provider 与 API key
3. 写入 `config.json`，再走正常引导

上游的 WebUI 本身就能做这件事（未配置模型时它会展示设置界面），所以更省事的做法是：
**首次运行时用一份最小配置启动网关**（`--yes` 等价的安全默认值），
然后让用户在 WebUI 里配置。这样不用自己写引导 UI。

### 3.4 单实例

用户可能双击两次。需要单实例锁，否则第二个实例会：
探测到端口已监听 → 附着 → 开出第二个窗口，两个窗口共用同一网关。
行为不算错，但观感差。用 Tauri 的单实例插件处理。

---

## 打包前的检查清单

- [ ] `packaging/backend_entry.py` 的 `-m nanobot` 拦截已实现并**单独测过**
- [ ] `.spec` 四类资源（web/dist、templates、skills、channels/*/webui）都能收集到
- [ ] onedir 形态，且确认杀掉 exe 后 Python 子进程不残留
- [ ] 全新用户环境测试：删掉 `~/.nanobot` 后首次运行能走通引导
- [ ] 端口被别的程序占用时（不是网关）不会误判为「已在运行」
- [ ] 安装到含中文/空格的路径下能正常工作
- [ ] WebView2 缺失场景已按选定模式处理
- [ ] 签名与 SmartScreen 表现已实测
