# 对上游 nanobot 0.3.5 的改动（补丁序列）

上游**不 fork、不 submodule**（理由见[根 README](../README.md)）：`nanobot-0.3.5/`
是解压出来的源码包，527 MB / 47406 个文件，入不了库。所以我们对上游的**全部改动**
以补丁形式放在这里 —— 可审查、可重放、升级上游时不丢。

这些补丁不是"顺手改的"，它们正是**交付出去的那个 SnapBot 与上游 nanobot 的差异**：
安装包里没有「自动任务」界面、没有浏览器端「注销」，就是靠这四个补丁。

## 规模

```
57 files changed, 7271 insertions(+), 14715 deletions(-)
```

## 补丁清单

| 补丁 | 范围 | 内容 |
|---|---|---|
| `0001-backend-remove-automations-and-logout.patch` | `nanobot/**` | 删除 `/api/webui/automations` 全部 5 个 action 路由、`/api/sessions/{key}/automations`、`automation_results.py`（整文件）；`session_automations.py` 由 344 行裁剪到 58 行；删除浏览器端注销的服务端支撑 |
| `0002-frontend-remove-automations-and-logout.patch` | `webui/**` | 删除自动任务 UI（侧栏入口、设置页 4 个组件、会话信息弹窗面板、删除确认提示、`Ctrl+Shift+3` 快捷键、聊天入口）与注销全链路（`handleLogout`/`onLogout`）；10 个语言包清理；前端测试同步 |
| `0003-tests-follow-automation-removal.patch` | `tests/**` | 后端测试跟随删除，并新增 `test_cron_run_identity.py` |
| `0004-docs-drop-automations.patch` | `docs/**` | 上游文档里提到自动任务的部分同步 |

**明确保留**（删 UI 不等于删能力）：`nanobot/cron/` 引擎、`nanobot/agent/tools/cron.py`
工具、`nanobot/triggers/`。定时任务在界面上看不到了，但 agent 仍能管理它们。

**顺带修掉的一个真问题**：会话删除原来会被 `blocked_by_automations` 拦截，要求客户端
带 `delete_automations=1` 二次确认。UI 一删，这个拦截就再没有客户端能响应 ——
会话删除会**永久卡死**。补丁改成直接删，并级联清理绑定的 cron 任务与本地触发器。

## 怎么应用

```powershell
# 1) 先恢复原版上游（会重建 nanobot-0.3.5/）
.\scripts\restore-upstream.ps1 -Force

# 2) 依次应用补丁 —— 注意必须带 -c core.autocrlf=false（原因见下）
cd nanobot-0.3.5
Get-ChildItem ..\patches\*.patch | Sort-Object Name | ForEach-Object {
    git -c core.autocrlf=false apply -p1 $_.FullName
}

# 3) 重建前端产物（补丁**不含** nanobot/web/dist）
cd webui
bun install
bun run build
```

> ⚠️ **`-c core.autocrlf=false` 不能省。** Git for Windows 的系统级配置
> （`C:\Program Files\Git\etc\gitconfig`）默认 `core.autocrlf=true`，不加这个参数时
> `git apply` 会把补丁内容整体写成 CRLF —— 实测 43 个文件被改写，
> 与验收基线不再逐字节一致。功能上未必立刻出问题，但从此没人能判断
> "这棵树到底是不是补丁描述的那棵树"。

## 故意不覆盖的东西

| 路径 | 原因 |
|---|---|
| `nanobot/web/dist/**` | 前端构建产物，内容是哈希文件名，必须现 build（上面的第 3 步） |
| `.venv/`、`uv.lock` | 本机生成的依赖产物，不属于源码改动 |

## 怎么验证补丁是准的（可复现）

把原档解到临时目录 → 依次应用四个补丁 → 与工作区**逐文件比对 SHA256**：

```powershell
# 期望输出
patched files : 1460
current files : 1460
missing in patched : 0
extra in patched   : 0
content differs    : 0
```

2026-10-16 实测即为上述结果（`原档\nanobot-0.3.5.zip` 解包 1471 个文件，
排除 `.venv`/`node_modules`/`__pycache__`/`nanobot/web/dist`/`uv.lock` 后比对 1460 个）。
