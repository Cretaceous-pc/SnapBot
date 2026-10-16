# 打包与分发

> **状态：已实施并实测通过。** 发布形态（Tauri 壳 + 冻结 Python 后端 + NSIS 安装程序）
> 已构建：`SnapBot_0.1.0_x64-setup.exe`，**259.3 MB**（WebView2 离线安装模式），
> 未签名。静默安装、升级不丢数据、卸载清理、无控制台启动、已删功能未进包
> 等实测见文末「实测记录」。
> 本文记录发布形态的取舍，以及**一定会踩的坑**（含踩坑时的症状与验收方法）。

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

> **状态：已实施并实测通过。** 见 [`../packaging/`](../packaging/)：
> `backend_entry.py`、`snapbot-backend.spec`、`build-backend.ps1`。
>
> 实测结果：产物 **120.7 MB**，冻结 exe 注册 **23 个工具**（与开发形态逐一相同）、
> 启用 websocket 渠道、`/health` 返回 `websocket: running`；
> 桌面壳指向冻结后端时 WebView 正常连上并渲染中文界面。

```powershell
# 构建（含冒烟测试：CLI 启动 + 起网关 + /health 就绪）
.\packaging\build-backend.ps1 -Clean
```

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

**`-m` 对冻结的 exe 没有意义，必然失败。** 已在 `packaging/backend_entry.py`
入口最前面拦截这个精确 argv 前缀并转发给 nanobot 的 CLI main。实测有效。

### 1.2 最大的坑：`pkgutil` 枚举失效导致渠道与工具**静默消失**

上游用 `pkgutil.iter_modules(package.__path__)` 动态枚举包：

- `nanobot/channels/registry.py:25` —— 枚举渠道
- `nanobot/agent/tools/loader.py:43` —— 枚举工具

冻结后 `__path__` 指向 PYZ 归档内部，而 `iter_modules` 需要扫描**真实文件系统目录**，
于是什么都枚举不到。症状极具误导性：

- 进程正常启动、`/health` 返回 `ready: true` —— 看起来一切正常
- 但日志里只有一行 WARNING：`No channels enabled`
- WebUI 打不开（websocket 渠道没起来），工具全部消失
- **没有任何异常或 traceback**

**关键区分**：`hiddenimports` / `collect_submodules` 只保证模块「**可导入**」，
**不解决「可枚举」**。这是最容易误判的地方——以为收了子模块就够了。

正确修法是在 spec 的 `Analysis` 里用 `module_collection_mode` 把包从归档里
拿出来、作为散装文件落地：

```python
module_collection_mode={
    "nanobot.channels": "py+pyz",
    "nanobot.agent.tools": "py+pyz",
},
```

合法取值：`'pyz'`(默认) / `'pyc'` / `'py'` / `'pyz+py'`。选 `'py+pyz'` 是两头都要：
归档里留一份（导入快），磁盘上留一份（可枚举）。

**验证方法**：对比冻结形态与开发形态的**工具注册数与渠道列表**，
而不是只看「进程起来了 / health 返回 ok」——后者在渠道为 0 时同样成立。

### 1.3 其余必须显式打包的资源

| 资源 | 漏掉的症状 |
|---|---|
| `nanobot/web/dist/**` | 白屏（前端产物 6.17MB / 438 文件） |
| `nanobot/templates/**/*.md` | 首次运行不生成 `AGENTS.md` / `SOUL.md` / `USER.md` |
| `nanobot/skills/**/*.md` | 技能系统空掉 |
| `nanobot/channels/*/webui/**` | 部分渠道设置面板加载失败 |

> 属性访问式 `datas` **必须用 `(src, dest)` 元组**。写成列表会被当成字符串序列，
> 生成出逐字符的垃圾目录。

同时过滤掉 `collect_submodules` 收进来的测试包（`.tests` / `test_`），
否则产物里会塞进每个渠道的整套测试。

### 1.4 用 onedir，不要 onefile

| | onefile | **onedir（已选）** |
|---|---|---|
| 杀软误报 | 高（自解压行为） | 低 |
| 启动速度 | 每次解压到 temp，慢 | 直接加载 |
| 分发 | 单文件好看 | 一个目录，由安装程序封装，用户无感 |

### 1.5 构建脚本的两个 PowerShell 坑

1. **脚本必须纯 ASCII**。Windows PowerShell 5.1 在没有 BOM 时按 ANSI/GBK
   读取 `.ps1`，中文会把字符串字面量甚至**行继续符反引号**弄坏，
   报出指向错误行号的诡异解析错误（本次实测：`无法将参数绑定到参数"Path"，因为该参数是空值`）。
2. **PyInstaller 把进度写到 stderr**，在 `$ErrorActionPreference='Stop'` 下
   会被包装成 NativeCommandError 并**终止脚本**，表现为「构建莫名跑到一半退出」。
   脚本里临时放宽该设置、自行接管两个输出流、只看退出码。

### 1.6 运行时数据绝不能打进包

| 路径 | 必须在用户目录 |
|---|---|
| `~/.nanobot/config.json` | 含 API key |
| `~/.nanobot/workspace/` | 用户工作区 |
| `~/.nanobot/sessions/` | 会话历史 |
| `~/.nanobot/logs/` | 日志 |

打进包的后果：升级覆盖用户数据；装到 `Program Files` 下默认不可写，直接起不来。

> 另注：**上游没有 `NANOBOT_DATA_DIR` 之类的环境变量**。数据目录由
> `set_config_path()` 决定，等于 config 文件的**父目录**
> （`nanobot/config/paths.py`）。做隔离测试要用 `--config` 指向临时目录，
> 不要指望环境变量。

### 1.7 windowed 冻结体的 stdio 是 None（会导致直接双击运行就崩）

`snapbot-backend.spec` 里 `console=False`（桌面应用不要黑窗），
这让 exe 变成 **GUI 子系统**（PE `Subsystem=2`）。代价是：
**没有控制台时**（双击、或被 shell 以 `UseShellExecute=true` 拉起），
PyInstaller 把 `sys.stdout` / `sys.stderr` / `sys.stdin` 置为 **None**。

上游 `nanobot/cli/entry.py::_configure_windows_console()` 第一句是：

```python
if sys.platform != "win32" or sys.stdout.encoding == "utf-8":
```

`sys.stdout` 为 None 时取 `.encoding` 直接抛：

```
AttributeError: 'NoneType' object has no attribute 'encoding'
```

而且因为是 GUI 子系统，**没有 stderr 可写，traceback 连打都打不出来** ——
表现为「双击毫无反应 / 一闪而过」。

症状对比（实测）：

| 启动方式 | 修复前 | 修复后 |
|---|---|---|
| 控制台里运行（stdout 重定向到文件/管道） | ✅ 正常，打印 `nanobot v0.3.5` | ✅ 正常 |
| **双击 / `UseShellExecute`（无控制台）** | ❌ 立即死亡，退出码为空 | ✅ 退出码 0 |

**修法（已实施，在 `packaging/backend_entry.py`）**：入口处先补齐占位 stdio。

```python
class _NullTextStream(io.StringIO):
    encoding = "utf-8"      # 必须在类层面声明！
    errors = "replace"

def _ensure_std_streams() -> None:
    for name in ("stdout", "stderr", "stdin"):
        if getattr(sys, name, None) is None:
            setattr(sys, name, _NullTextStream())
```

**踩坑记录**：最初写成先 `io.StringIO()` 再实例赋值 `.encoding`，
运行时报
`AttributeError: attribute 'encoding' of '_io._TextIOBase' objects is not writable`
—— C 实现里该属性只读，**必须用子类在类层面声明**。

只替换为 None 的流：有真实控制台时保持原样，不影响正常输出。

---

## 第二部分：Tauri 侧

### 2.1 冻结产物的挂载方式

用 `bundle.resources` 而不是 `externalBin`：

```json
{
  "bundle": {
    "resources": {
      "../../packaging/dist/nanobot-backend": "nanobot-backend"
    }
  }
}
```

**为什么不选 `externalBin`**：Tauri 的 sidecar 机制预期单个可执行文件，
而 PyInstaller 的 onedir 是一个含 `_internal/` 的目录树。
`resources` 按目录整体搬运，语义正好。
（代价：失去 Tauri 的 sidecar 自动命名/校验，但我们本来就要自己管进程。）

运行时由 `gateway::resolve_backend()` 依次查找：
显式覆盖 `SNAPBOT_BACKEND` → 冻结产物（程序同级 / `resources/nanobot-backend/`）
→ 开发形态的 `.venv`。

### 2.2 命令行形态差异（容易出错）

| 形态 | 命令行 |
|---|---|
| 冻结 | `nanobot-backend.exe gateway --foreground --port 18790` |
| 开发 | `python.exe -m nanobot gateway --foreground --port 18790` |

这个差异集中在 `BackendRuntime::command()` 一处，调用方只传 nanobot 自己的参数。

### 2.3 WebView2 运行时

Win11 自带；Win10 不一定有。Tauri 的选项：

```json
{ "bundle": { "windows": { "webviewInstallMode": { "type": "downloadBootstrapper" } } } }
```

| 模式 | 代价 |
|---|---|
| `downloadBootstrapper` | 安装包小，但安装时需要联网 |
| `embedBootstrapper` | 安装包 +1.8MB，仍需联网 |
| **`offlineInstaller`（已选）** | 安装包 +127MB，离线可装 |
| `skip` | 最小，但用户可能打不开（白屏） |

面向不确定的他人机器，已改用 **`offlineInstaller`**：多 127MB 换掉一整类
「装完打开是白屏」的客服问题。代价是安装包从 52MB 涨到 **259MB**
（安装后占用仍是 130.6MB，不受影响）。

### 2.3.1 必须在打包前清掉 Tauri 的 staging 副本

**这是一个会静默产出「僵尸文件」的坑。** `cargo tauri build` 把
`bundle.resources` **复制进** `target/release/nanobot-backend`，
但**不会**删除源里已不存在的文件。Vite 产出的是内容哈希文件名，
所以 WebUI 每次重建都会生成**新文件名**，旧文件全部残留：

```
症状：staging/assets 文件数 > 源 dist 文件数（实测 473 vs 422）
      index.html 指向新 chunk，但旧 chunk 仍躺在磁盘上
后果：安装包把新旧两套都打进去。旧 chunk 里可能仍含**已删除的功能代码**
```

实测：某次打包后旧 `SettingsView-Ca2xn6SF.js` 仍被收进安装包，
里面就是已删除的自动任务界面。因为 `index.html` 指向新 chunk，**运行起来
完全正常**，只有拆开安装包才能发现——所以这个坑不会自己暴露。

**修法**：`build-backend.ps1` 在构建前强制删除 staging 副本
（`target/release/nanobot-backend`），让下一次 `tauri build` 从干净状态复制。

**验收方法**（不要只看「装完能用」）：解压/检查生成的
`target/release/nsis/x64/installer.nsi`，它含有完整打包清单：

```powershell
# 打包的 asset 行数应 = 源文件数 x 2
(Select-String -Path installer.nsi -Pattern 'web\\dist\\assets\\' -AllMatches).Count
# 逐个核对源文件名是否都在清单里，且没有多余的旧文件名
```


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

### 3.3 首次运行引导 —— 已实现

原问题：用户装完打开时 `~/.nanobot/config.json` 还不存在，直接报错。

**实现方式（已在代码里）**：不自己手搓 `config.json`（那等于复制上游 schema
与校验规则，早晚漂移），而是调用上游 `nanobot webui --yes --no-open`
走官方初始化路径。它会生成 schema 正确的配置、启用本地 WebUI 通道、
把 `tokenIssueSecret` 写进去，然后拉起网关。

两个必须遵守的顺序约束（写反都会失败）：

1. **先起网关、再读配置** —— 首次运行时配置文件是**网关写出来的**。
2. **初始化后重新读一次配置** —— `tokenIssueSecret` 是那次才生成的，
   用占位默认值开窗会连不上后端。

实测：1.6s 生成配置、4.3s 网关就绪。首次运行生成的配置里**没有任何 provider
key**，所以引导文案提示「随后请在 Settings → Models 里配置模型」是必要且准确的。

### 3.4 单实例 —— 已实现

第二个实例会探测到端口被监听（那正是第一个实例起的网关）→ 判定「附着」→
开出第二个窗口，两个窗口共用同一网关。

**用命名互斥体（`CreateMutexW`）而不是 Tauri 单实例插件、更不是锁文件**：
锁文件的经典故障是「崩溃后永远起不来」（残留陈旧锁）；
命名互斥体由内核在进程退出时自动释放，崩溃也不会残留。

---

## 第四部分：卸载残留与界面语言

### 4.1 Tauri 默认的「删除应用程序数据」删不干净 nanobot

Tauri 的 NSIS 模板**已经内置**「删除应用程序数据」复选框，但它删的是：

```
RmDir /r "$APPDATA\${BUNDLEID}"        → %APPDATA%\com.snapbot.desktop
RmDir /r "$LOCALAPPDATA\${BUNDLEID}"   → %LOCALAPPDATA%\com.snapbot.desktop
```

而 nanobot 把**全部用户数据**放在 `~/.nanobot`（Python `Path.home()`，
Windows 上即 `%USERPROFILE%`），**根本不在上面两个路径里**：

| 路径 | 内容 |
|---|---|
| `%USERPROFILE%\.nanobot\config.json` | **含 API key** |
| `%USERPROFILE%\.nanobot\workspace\` | 工作区与记忆 |
| `%USERPROFILE%\.nanobot\sessions\` | 会话历史 |
| `%USERPROFILE%\.nanobot\logs\` | 日志（含桌面壳日志） |
| `%USERPROFILE%\.nanobot\run\` | 网关状态 |
| `%USERPROFILE%\.nanobot\llm_usage.sqlite3` | 用量统计 |

而且 `.nanobot` 以点开头、资源管理器里默认隐藏，用户很难自己发现。
**这就是「卸载留残留」的真正来源。**

### 4.2 解决办法：installer hooks

```json
{
  "bundle": {
    "windows": {
      "nsis": {
        "languages": ["SimpChinese", "English"],
        "displayLanguageSelector": false,
        "installerHooks": "installer-hooks.nsh",
        "installMode": "currentUser"
      }
    }
  }
}
```

`desktop/src-tauri/installer-hooks.nsh` 挂三个宏：

- **`NSIS_HOOK_POSTINSTALL`**（新增）
  - 补一个开始菜单的「**SnapBot 卸载**」快捷方式，指向 `$INSTDIR\uninstall.exe`
  - **为什么需要**：Tauri 模板**只**把卸载程序注册进
    `HKCU\...\Uninstall\SnapBot`，也就是说卸载入口只在
    「设置 → 应用」或安装目录里。开始菜单**只有启动快捷方式**，
    实测用户会找不到卸载入口。
- **`NSIS_HOOK_PREUNINSTALL`**
  - `StrCpy $DeleteAppDataCheckboxState 1` —— 默认按「已勾选删除数据」处理
  - 停掉后端进程树（`taskkill /T`）；否则文件被占用会删不干净
  - 顺带清理开发形态遗留的 `nanobot gateway --background` 进程
- **`NSIS_HOOK_POSTUNINSTALL`**
  - 显式删除上面补的「SnapBot 卸载」快捷方式
  - 逐项 `RmDir /r` 删 `$PROFILE\.nanobot` 下的子目录，最后整体再删一次
  - **必须判断 `$UpdateMode <> 1`**：升级覆盖安装时绝不能删用户数据

> ⚠️ **测试卸载前务必备份 `~/.nanobot`。** 卸载程序的「删除应用程序数据」
> **默认勾选**（这是刻意的设计：卸载不留残留），意味着
> `uninstall.exe /S` 会**连同 `config.json`（含 API key）和会话历史一起删掉**，
> 且不进回收站。做卸载实测前先复制一份。

三个关键实现细节：

1. **宏在卸载器上下文里执行**，其中调用的自定义函数必须命名为 `un.xxx`。
   写错会得到 NSIS 编译错误或在运行时找不到函数。
2. **无法回头去点那个复选框**：它由模板的 `un.ConfirmPagePre` 创建，
   勾选状态在 `un.ConfirmLeave` 读入 `$DeleteAppDataCheckboxState`。
   但该变量在页面创建时已声明（`Var`），在 PREUNINSTALL 里直接写它
   即等价于「用户勾选了」。这比替换整个 NSIS 模板去挂
   `MUI_PAGE_CUSTOMFUNCTION_PRE` 可靠得多。
3. **逐项删除而不是只 `RmDir /r` 整个目录**：万一有文件被占用导致整体删除
   失败，逐项删至少能保住大部分清理效果，而不是全军覆没。

### 4.3 明确不删的东西

- **开发形态的源码检出**（如 `D:\Desktop\SnapBot\nanobot-0.3.5`）**不删**。
  那是用户的源码，删掉属越权。安装程序只清理本产品自己创建的数据。
- 安装目录由 Tauri 模板自己处理，hooks 不重复插手。

### 4.4 安装程序界面语言

`"languages": ["SimpChinese", "English"]` + `displayLanguageSelector: false`：
按系统语言自动选，中文系统即中文界面，不弹语言选择框。

中文语言包（`SimpChinese.nsh`，随 `tauri-bundler` 分发）**已包含**自定义文案的
翻译，包括那个复选框的 `deleteAppData` → **"删除应用程序数据"**，
所以不需要 `customLanguageFiles` 补翻译。

### 4.5 应用界面语言（注意：这不是安装程序语言）

上游 `webui/src/i18n/config.ts` 的 `resolveInitialLocale()` **只读 localStorage，
完全不看系统语言**：

```ts
export const defaultLocale: SupportedLocale = "en";
export function resolveInitialLocale() {
  return readStoredLocale() ?? defaultLocale;   // ← 没有 navigator.language 兜底
}
```

所以中文用户首次打开必然是英文界面——**即使 `zh-CN` 语言包已经打包在 `dist` 里**。

**解决办法：注入初始化脚本，不 fork 上游。** Tauri 的
`WebviewWindowBuilder::initialization_script()` 在页面自身脚本之前执行，
因此 i18n 模块初始化时，localStorage 里已经有我们写入的值：

```rust
const FORCE_CHINESE_LOCALE_SCRIPT: &str = r#"
(function () {
  try { window.localStorage.setItem('nanobot.locale', 'zh-CN'); } catch (e) {}
})();
"#;
```

三个要点：

1. **无条件写入**（不是「仅当未设置」）——需求是默认中文。用户切换语言后
   在本次会话内生效，重启回到中文。
2. **存储键必须与上游 `LOCALE_STORAGE_KEY` 一致**（`nanobot.locale`）。
   写错会**静默退回英文**：无异常、无日志。所以有一条单元测试锁住这个约定
   （`locale_script_writes_the_key_upstream_reads`）。
3. 脚本会注入 WebView 里的**每一个页面**（含加载页），加载页不受影响。

已用截图实测确认界面为中文。

---

## 打包前的检查清单

- [x] `packaging/backend_entry.py` 的 `-m nanobot` 拦截已实现并**单独测过**
- [x] `.spec` 四类资源（web/dist、templates、skills、channels/*/webui）都能收集到
- [x] onedir 形态，且确认杀掉 exe 后 Python 子进程不残留
- [x] 端口被别的程序占用时（不是网关）不会误判为「已在运行」
      → 已改为校验 `/health` 响应体
- [x] 全新用户环境测试：删掉 `~/.nanobot` 后首次运行能走通引导
      → 已实测（1.6s 生成配置、4.3s 就绪）
- [x] 应用界面默认简体中文 → 已用截图实测确认
- [x] **卸载残留验证**：装完整包 → 用一次 → 卸载 → `%USERPROFILE%\.nanobot` 整个消失
- [x] **升级不误删数据验证**：装 → 用一次 → 覆盖安装 → 用户数据仍在
- [x] 卸载时若网关正在运行，进程被停掉且数据仍能删净
- [x] 安装程序界面为中文（`languages: [SimpChinese, English]`）
- [x] 安装到用户目录（`%LOCALAPPDATA%`，per-user，无需管理员）
- [x] 安装到含中文/空格的路径下能正常工作 → 2026-10-16 实测通过（见下）
- [x] WebView2 缺失场景已按选定模式处理（已改为 `offlineInstaller`，离线可装）
- [x] **无控制台双击启动不崩**（见 §1.7，windowed stdio 为 None）
- [x] 开始菜单有卸载入口（见 §4.2 `NSIS_HOOK_POSTINSTALL`）
- [ ] 签名与 SmartScreen 表现已实测

### 实测记录（2026-10-16）：安装到含中文与空格的路径

被测对象仍是 `SnapBot_0.1.0_x64-setup.exe`（259.3 MB，
SHA256 `DEE5116E236170E8A6E5D2CA7C08948CABCD6E1637A5E6DEA0090BD444849845`）。
安装到 **`D:\安装 测试\SnapBot 空格`**（同时含中文与空格），全程未设任何 `SNAPBOT_*` 环境变量。

**方法（两处必须踩准，否则测的不是这件事）**

1. **`/D=` 必须最后一个参数、且不能加引号**。NSIS 的规定是「路径里即使有空格也不要加引号，
   加引号会把引号本身当成路径的一部分」。所以不能用 PowerShell 的
   `& $exe /S /D=$dir`（它会给含空格的参数补引号），要用：
   ```powershell
   $psi = New-Object System.Diagnostics.ProcessStartInfo
   $psi.FileName  = $exe
   $psi.Arguments = '/S /D=' + $dir      # 原样传递，不引号
   $psi.UseShellExecute = $false
   [System.Diagnostics.Process]::Start($psi).WaitForExit()
   ```
2. **测试脚本正文里不要出现中文**。本机 `powershell.exe` 是 5.1，读无 BOM 的 `.ps1`
   时按 GBK 解码，中文字面量会把字符串引号吃掉（历史上已被这个坑咬过两次：
   `.cmd` 与 `.ps1`）。这里的路径用码点拼出来：
   ```powershell
   $dir = 'D:\' + [char]0x5B89 + [char]0x88C5 + ' ' + [char]0x6D4B + [char]0x8BD5 + '\SnapBot ' + [char]0x7A7A + [char]0x683C
   #           安                装            空格            测                试                空格   空                格
   ```

| 验证项 | 结果 |
|---|---|
| 静默安装 | ✅ 退出码 0 |
| 安装目录落地 | ✅ `D:\安装 测试\SnapBot 空格`（壳 10.0 MB + `uninstall.exe` + `nanobot-backend\`） |
| 注册表 | ✅ `SnapBot 0.1.0`，`InstallLocation = "D:\安装 测试\SnapBot 空格"` |
| 开始菜单 | ✅ `SnapBot.lnk` + `SnapBot 卸载.lnk` |
| **真正跑起来** | ✅ 壳与后端进程路径都在中文目录下：<br>`"D:\安装 测试\SnapBot 空格\nanobot-backend\nanobot-backend.exe" -m nanobot gateway --foreground --port 18790` |
| 网关就绪 | ✅ `/health` → `{"status":"ok","ready":true,"websocket":"running"}` |
| WebUI | ✅ `http://127.0.0.1:8765/` HTTP 200，7498 字节 |
| 工具注册 | ✅ 日志含 23 个工具与 `Channels enabled: websocket` |
| 首次引导 | ✅ `~/.nanobot` 正常生成，`tokenIssueSecret` 已写入（无 provider key，符合预期） |
| **卸载清理** | ✅ 安装目录、注册表项、开始菜单、`~/.nanobot`、进程、端口**全部干净** |

**两点说明**

- 安装器只删除自己的 `$INSTDIR`，**不会删上级目录**：测试后 `D:\安装 测试\`
  会作为空目录留下，需要手工清理。这不是残留问题（里面没有任何文件）。
- 结论：非 ASCII 路径**不是**风险点 —— 壳、冻结后端、Python 的
  `sys.executable`、WebView2 用户数据目录在这一路径下都工作正常。

### 实测记录（2026-10-12，删除自动任务 + 注销之后）

安装包 `SnapBot_0.1.0_x64-setup.exe`，**259.3 MB**，**未签名**。
（体积上涨来自 WebView2 `offlineInstaller`；安装后占用仍是 130.6 MB。）

| 验证项 | 方法 | 结果 |
|---|---|---|
| 冻结后端 | `build-backend.ps1 -Clean` | ✅ 120.9 MB，两个冒烟测试通过 |
| 工具与渠道 | 冻结网关启动日志 | ✅ `Registered 23 tools`（含 `cron`）、`Channels enabled: websocket` |
| 已删功能未进包 | 检查 staging + `installer.nsi` 清单 | ✅ 0 处残留，422/422 asset 全部匹配 |
| 安装 | `setup.exe /S` | ✅ 退出码 0，注册为 `SnapBot 0.1.0`，per-user |
| **升级不丢数据** | 安装前后统计 `~/.nanobot` | ✅ 51 → 51 文件，`config.json` 完好 |
| **独立运行** | 启动安装后的 exe，探测 `/health` | ✅ 2s 内 `ready: true, websocket: running` |
| 进程来源 | 检查 `nanobot-backend` 路径 | ✅ 取自安装目录，非开发目录 |
| **运行时功能核对** | 抓取线上 `index-C4M0V8oW.js` 并正则比对 | ✅ 删除项 0 命中；保留项（`create_automation`、locale 注入、bootstrapSecret）均在 |
| **无控制台启动**（stdin/stdout 为 None） | `Process.Start` + `CreateNoWindow`，无重定向 | ✅ 退出码 0（修复前：立即死亡、退出码为空） |
| 有控制台启动 | stdout 重定向到文件 | ✅ 正常打印 `nanobot v0.3.5` |
| `-m nanobot` 自举翻译 | 同上，无控制台 | ✅ 退出码 0 |
| **开始菜单卸载入口** | 安装后检查 `$SMPROGRAMS` | ✅ `SnapBot 卸载.lnk` 已创建且指向 `uninstall.exe` |
| 卸载清理 | `uninstall.exe /S` | ✅ 安装目录 + `~/.nanobot` 全部删除（**默认勾选删数据**） |

**教训**：
1. 验证「已删功能没进包」不能只看应用能跑起来。由于 `index.html`
   始终指向最新的 hash chunk，陈旧 chunk 即使被打进安装包也不会影响运行，
   必须直接核对 `installer.nsi` 的打包清单。
2. **测卸载前必须先备份 `~/.nanobot`**。这次实测卸载顺带删掉了真实的
   `config.json`（含 API key）与会话历史 —— 卸载器默认勾选「删除应用程序数据」，
   这是设计如此，不是 bug。**但测试流程必须自己先备份。**
3. windowed 冻结体的启动路径**必须单独测无控制台场景**。
   在控制台里跑一切正常，双击却直接崩，且 GUI 子系统没有 stderr、
   连 traceback 都看不到。见 §1.7。



### 实测记录（2026-10-11）

安装包 `SnapBot_0.1.0_x64-setup.exe`，**52.29 MB**，**未签名**。
安装后 `%LOCALAPPDATA%\SnapBot` 共 **130.4 MB**（9.54 MB 壳 + 120.7 MB 冻结后端）。

| 验证项 | 方法 | 结果 |
|---|---|---|
| 安装 | `setup.exe /S` | 退出码 0，注册为 `SnapBot 0.1.0`，per-user 无需管理员 |
| **独立运行** | 清空全部 `SNAPBOT_*` 环境变量后启动 | 后端取自**安装目录**（非开发目录），`/health` 返回 `websocket: running` |
| **升级不丢数据** | 放哨兵文件 → 再次 `/S` 安装 | 39 → 39 文件，哨兵与 `config.json` 均在 ✓ |
| **卸载无残留** | `uninstall.exe /S` | 数据目录、安装目录、注册表项全部清除 ✓ |

三项实测通过后，已从备份完整还原用户数据（51 文件，DeepSeek key 与 websocket 配置完好）。

### 安装程序与 NSIS 工具链（国内网络的坑）

构建时 Tauri 需要从 GitHub 取两样东西，**走代理时 TLS 常失败**：

1. `nsis-3.11.zip`（约 2 MB）→ `%LOCALAPPDATA%\tauri\NSIS`
2. `nsis_tauri_utils.dll`（约 34 KB）→ 同上目录的 `Plugins\x86-unicode\additional\`

Tauri 会自动下载，失败时报 `failed to bundle project: io: unexpected end of file`
或 `timeout: global`。可手动放置：

```
%LOCALAPPDATA%\tauri\NSIS\
├─ makensis.exe                                    ← 来自 nsis-3.11.zip
├─ Bin\makensis.exe
└─ Plugins\x86-unicode\additional\
   └─ nsis_tauri_utils.dll                         ← 需单独下载
```

**只解压 `nsis-3.11.zip` 是不够的**：Tauri 会校验
`Plugins/x86-unicode/additional/nsis_tauri_utils.dll`，缺了会报
`NSIS directory is missing some files. Recreating it.` 并尝试重新下载。
该 DLL 必须匹配 Tauri 期望的 SHA1（见 `tauri-bundler/src/bundle/windows/nsis/mod.rs`
的 `NSIS_TAURI_UTILS_SHA1`），v0.5.3 为 `75197FEE3C6A814FE035788D1C34EAD39349B860`。

实测可用的镜像前缀：`https://ghproxy.net/<原始 GitHub URL>`。

### 写 NSIS hooks 的语法坑

**NSIS 字符串字面量用单引号定界，内部不能再出现单引号。** 最初用 wmic 按命令行
匹配开发形态网关：

```nsis
; ✗ 报 "unterminated string parsing"（'python.exe' 截断了字符串字面量）
nsExec::ExecToLog 'wmic process where "name='python.exe' and commandline like '%nanobot%gateway%'" call terminate'
```

改成 `taskkill /IM python.exe /T /F` + PowerShell 命令行过滤解决。

### 未签名的影响（实测）

`Get-AuthenticodeSignature` 返回 `NotSigned`。用户首次运行会遇到 Windows
SmartScreen「未知发布者」提示。**这与壳选型无关**，任何未签名的分发形态都一样。
