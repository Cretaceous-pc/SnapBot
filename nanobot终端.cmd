@echo off
rem ============================================================
rem  nanobot terminal TUI launcher
rem
rem  Do NOT run nanobot-tui-win32-x64.exe directly. It is only the
rem  renderer process: the nanobot Python launcher must inject the
rem  NANOBOT_TUI_* environment variables into it. Started by itself it
rem  throws "NANOBOT_TUI_BOOTSTRAP_URL or NANOBOT_TUI_WS_URL is
rem  required" and the window closes instantly.
rem
rem  This window stays open after exit so errors remain readable.
rem ============================================================
setlocal
set "ROOT=D:\Desktop\SnapBot\nanobot-0.3.5"

if not exist "%ROOT%\pyproject.toml" goto :noroot
if not exist "%ROOT%\.venv\Scripts\nanobot.exe" goto :novenv

cd /d "%ROOT%"
echo Starting nanobot terminal TUI from "%ROOT%" ...
"%ROOT%\.venv\Scripts\nanobot.exe" %*
set "RC=%ERRORLEVEL%"
echo.
echo [exit code %RC%]
goto :hold

:noroot
echo [ERROR] checkout not found: %ROOT%
goto :hold

:novenv
echo [ERROR] missing venv: %ROOT%\.venv\Scripts\nanobot.exe
echo         create it first:  uv venv --python 3.13 .venv
echo         then:             uv pip install -e .

:hold
echo.
echo Press any key to close this window.
pause >nul
endlocal
