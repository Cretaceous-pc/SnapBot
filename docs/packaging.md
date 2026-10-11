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

`desktop/src-tauri/installer-hooks.nsh` 挂两个宏：

- **`NSIS_HOOK_PREUNINSTALL`**
  - `StrCpy $DeleteAppDataCheckboxState 1` —— 默认按「已勾选删除数据」处理
  - 停掉后端进程树（`taskkill /T`）；否则文件被占用会删不干净
  - 顺带清理开发形态遗留的 `nanobot gateway --background` 进程
- **`NSIS_HOOK_POSTUNINSTALL`**
  - 逐项 `RmDir /r` 删 `$PROFILE\.nanobot` 下的子目录，最后整体再删一次
  - **必须判断 `$UpdateMode <> 1`**：升级覆盖安装时绝不能删用户数据

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

- [ ] `packaging/backend_entry.py` 的 `-m nanobot` 拦截已实现并**单独测过**
- [ ] `.spec` 四类资源（web/dist、templates、skills、channels/*/webui）都能收集到
- [ ] onedir 形态，且确认杀掉 exe 后 Python 子进程不残留
- [x] 端口被别的程序占用时（不是网关）不会误判为「已在运行」
      → 已改为校验 `/health` 响应体
- [x] 全新用户环境测试：删掉 `~/.nanobot` 后首次运行能走通引导
      → 已实测（1.6s 生成配置、4.3s 就绪）
- [x] 应用界面默认简体中文 → 已用截图实测确认
- [ ] **卸载残留验证**：装完整包 → 用一次（产生 config.json / sessions / logs）
      → 卸载 → 确认 `%USERPROFILE%\.nanobot` 整个消失
- [ ] **升级不误删数据验证**：装 v1 → 用一次 → 覆盖安装 v2
      → 确认 `$UpdateMode = 1` 分支生效、用户数据仍在
- [ ] 卸载时若网关正在运行，确认进程被停掉且数据仍能删净
- [ ] 安装程序界面为中文（中文系统下）
- [ ] 安装到含中文/空格的路径下能正常工作
- [ ] WebView2 缺失场景已按选定模式处理
- [ ] 签名与 SmartScreen 表现已实测

> 前两条（卸载残留、升级不误删）是**必须实测**的：它们都只在打包产物上才能验证，
> 而 hooks 一旦写错（比如漏了 `$UpdateMode` 判断），后果是用户升级一次就丢光数据。
