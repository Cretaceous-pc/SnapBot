# 分支与版本约定

## 分支模型

```
main                     ← 稳定、可交付。只接受经测试的合并。打 tag 发版。
 └─ feat/tauri-shell     ← 当前开发分支：桌面壳骨架
     └─ feat/<topic>      ← 后续每个独立特性一个分支
fix/<topic>              ← 修 bug
chore/<topic>            ← 仓库维护、CI、文档
```

**规则**

1. **`main` 始终保持可运行**。任何进入 `main` 的提交都必须能 `cargo tauri dev` 跑起来。
2. **一个分支一件事**。特性分支从 `main` 切出，完成后合并回 `main` 并删除。
3. **提交粒度**：一个提交 = 一个可独立说明的改动。不要"一堆 WIP"混在一起。
4. **上游补丁单独成提交**，提交信息以 `upstream:` 开头，便于升级上游时筛出来重放。
5. **绝不提交**：`~/.nanobot/` 里的任何东西（含 API key）、冻结产物、`target/`。

## 提交信息格式

```
<type>: <简短中文描述>

<可选正文：为什么这么改、验证方式、已知限制>
```

`type` 取值：`feat` / `fix` / `chore` / `docs` / `refactor` / `test` / `upstream`

**示例**

```
feat: Tauri 壳骨架，起网关后开窗加载 WebUI

复用上游 _webui_browser_url() 的等价逻辑，从 config.json 直接算出带
bootstrapSecret 的 URL，避免解析 CLI stdout（脆弱）。
验证：cargo tauri dev 后窗口加载 127.0.0.1:8765，能看到会话界面。
```

## 上游版本升级流程

上游不 fork、不 submodule（见 README 说明）。升级时：

1. 把新 zip 放进 `原档/`，跑 `scripts/restore-upstream.ps1 -Force`
2. `patches/` 里的补丁逐个 `git apply`；冲突则手工解决并**更新补丁文件**
3. 前端产物需重建（`bun install && bun run build`），因为 zip 不含 `nanobot/web/dist`
4. 在独立分支（`chore/upgrade-upstream-<version>`）里完成，跑通再合并

> 这条流程要严格执行：上游升级是最容易悄悄破坏桌面壳的环节，
> 因为 `docs/architecture.md` 里记录的那些约束（回环 Host 校验、
> `build_gateway_command` 的自举方式）都可能在升级后变化。

## 版本号

桌面应用版本与上游 nanobot 版本**解耦**：

| | 位置 | 当前 |
|---|---|---|
| 桌面应用 | `desktop/package.json` + `desktop/src-tauri/tauri.conf.json` | 见文件 |
| 上游 nanobot | `nanobot-0.3.5/`（不入库） | 0.3.5 |

发版时在 `main` 上打 tag：`v<桌面版本>`，并在 release notes 里写明
对应的上游 nanobot 版本。
