; ============================================================================
;  SnapBot NSIS 安装/卸载钩子
;
;  由 tauri.conf.json 的 bundle.windows.nsis.installerHooks 引入。
;  可用宏：PREINSTALL / POSTINSTALL / PREUNINSTALL / POSTUNINSTALL
;
;  这些宏**在卸载器的执行上下文中运行**：其中调用的函数必须加 `un.` 前缀，
;  Var 却是共享的。所以下面自定义的函数都命名为 un.xxx。
; ============================================================================

; ----------------------------------------------------------------------------
;  为什么需要这个文件：Tauri 默认的「删除应用程序数据」删不干净
;
;  Tauri 模板里的删除动作是：
;      RmDir /r "$APPDATA\${BUNDLEID}"          → %APPDATA%\com.snapbot.desktop
;      RmDir /r "$LOCALAPPDATA\${BUNDLEID}"     → %LOCALAPPDATA%\com.snapbot.desktop
;
;  但 nanobot 把**全部用户数据**放在 `~/.nanobot`（Python 的 Path.home()：
;  Windows 上即 %USERPROFILE%），根本不在上面两个路径里：
;      %USERPROFILE%\.nanobot\config.json      ← 含 API key，必须删
;      %USERPROFILE%\.nanobot\workspace\       ← 用户工作区与记忆
;      %USERPROFILE%\.nanobot\sessions\        ← 会话历史
;      %USERPROFILE%\.nanobot\logs\            ← 日志（含桌面壳日志）
;      %USERPROFILE%\.nanobot\run\             ← 网关状态
;      %USERPROFILE%\.nanobot\llm_usage.sqlite3
;
;  结果：用户卸载后，凭据与全部聊天记录仍然留在磁盘上，且目录名以点开头
;  （隐藏），用户很难自己发现。这正是要修的问题。
; ----------------------------------------------------------------------------

; ----------------------------------------------------------------------------
;  卸载前：停掉进程 + 把「删除应用程序数据」默认置为勾选
;
;  关于默认勾选：需求是「卸载不留残留」，所以默认应为干净卸载；同时保留
;  取消勾选的余地——这里删的包含会话历史与 API key，属不可恢复操作。
;
;  关于置位时机（重要）：复选框由模板的 `un.ConfirmPagePre` 创建，其勾选状态
;  在离开确认页时由 `un.ConfirmLeave` 读进 `$DeleteAppDataCheckboxState`。
;  我们无法在 PREUNINSTALL 里回头去点那个复选框，但**可以直接写结果变量**——
;  它在页面创建时就已声明（Var），此时写入即等价于"用户勾选了"。
;  这比替换整个 NSIS 模板去挂 MUI_PAGE_CUSTOMFUNCTION_PRE 可靠得多。
; ----------------------------------------------------------------------------
!macro NSIS_HOOK_PREUNINSTALL
  ; 默认按「用户勾选了删除数据」处理：需求是卸载不留残留。
  ; 模板在页面创建时已声明该 Var；这里在读取之前写入即可。
  StrCpy $DeleteAppDataCheckboxState 1

  ; 卸载前先确保后端已停：进程活着会锁住目录，导致删除不完整。
  Call un.SnapBotStopBackend

  ; 清理开发形态留下的 `nanobot gateway --background` 实例。
  Call un.SnapBotStopBackgroundGateway
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; 注意：必须判断 $UpdateMode —— 升级时不能删用户数据，
  ; 否则每次覆盖安装都会抹掉配置与会话历史。
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    Call un.SnapBotPurgeDataDir
  ${EndIf}
!macroend

; ----------------------------------------------------------------------------
;  停止后端（桌面壳启动的 sidecar 进程树）
;
;  为什么不只 taskkill 主程序：网关会再拉起子进程（渠道、cron），
;  只杀直接子进程会留下孤儿继续占用 %USERPROFILE%\.nanobot 里的文件。
;  /T 杀整棵树。
; ----------------------------------------------------------------------------
Function un.SnapBotStopBackend
  ; 冻结形态的后端 sidecar 与 Tauri 主程序同名不同名皆有可能，
  ; 这里按"主程序 + 后端可执行名"依次尝试，失败不报错。
  nsExec::ExecToLog 'taskkill /IM "${MAINBINARYNAME}.exe" /T /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM "nanobot-backend.exe" /T /F'
  Pop $0
  ; 给文件句柄一点释放时间，否则紧接着的 RmDir 可能只删掉一部分
  Sleep 800
FunctionEnd

; ----------------------------------------------------------------------------
;  停止「开发形态」遗留的后台网关
;
;  开发形态下网关由 `nanobot-0.3.5\.venv\Scripts\python.exe -m nanobot gateway`
;  启动。不处理的话，它们会持有 config.json / llm_usage.sqlite3，
;  导致删不干净。用 WMIC 按命令行匹配，比按进程名精确。
; ----------------------------------------------------------------------------
Function un.SnapBotStopBackgroundGateway
  nsExec::ExecToLog 'wmic process where "name='python.exe' and commandline like '%nanobot%gateway%'" call terminate'
  Pop $0
  Sleep 500
FunctionEnd

; ----------------------------------------------------------------------------
;  删除 nanobot 数据目录
;
;  用 $PROFILE（当前用户主目录）而不是硬编码路径：安装程序可能以其他用户
;  身份运行，$PROFILE 始终指向"当前用户"。
;
;  逐项删除而不是直接 RmDir /r "$PROFILE\.nanobot"：万一有文件被占用导致
;  整目录删除失败，逐项删至少能保住大部分清理效果，而不是全军覆没。
; ----------------------------------------------------------------------------
Function un.SnapBotPurgeDataDir
  ; 设置到当前用户上下文，确保 $PROFILE 解析正确
  SetShellVarContext current

  Delete "$PROFILE\.nanobot\config.json"
  Delete "$PROFILE\.nanobot\llm_usage.sqlite3"
  Delete "$PROFILE\.nanobot\llm_usage.sqlite3-shm"
  Delete "$PROFILE\.nanobot\llm_usage.sqlite3-wal"

  RmDir /r "$PROFILE\.nanobot\workspace"
  RmDir /r "$PROFILE\.nanobot\sessions"
  RmDir /r "$PROFILE\.nanobot\logs"
  RmDir /r "$PROFILE\.nanobot\run"
  RmDir /r "$PROFILE\.nanobot\media"
  RmDir /r "$PROFILE\.nanobot\cron"
  RmDir /r "$PROFILE\.nanobot\history"
  RmDir /r "$PROFILE\.nanobot\webui"
  RmDir /r "$PROFILE\.nanobot\cli-apps"
  RmDir /r "$PROFILE\.nanobot\venv"
  RmDir /r "$PROFILE\.nanobot\bin"

  ; 最后整体删一次，捕捉上面没列到的新增子目录
  RmDir /r "$PROFILE\.nanobot"

  ; 开发形态的检出目录不归安装程序管，这里**不动**它：
  ; 那是用户的源码，删掉属于越权。只清理本产品自己创建的数据。
FunctionEnd
