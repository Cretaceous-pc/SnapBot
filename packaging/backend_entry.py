"""冻结后端的入口点。

这是 PyInstaller 的入口脚本。它解决一个 **上游自举陷阱**：

    上游 `nanobot/gateway/runtime.py::build_gateway_command()` 这样拉起网关子进程：

        [sys.executable, "-m", "nanobot", "gateway", "--foreground", "--port", ...]

    冻结之后 `sys.executable` 指向**本 exe**，于是上游会执行：

        nanobot-backend.exe -m nanobot gateway --foreground --port 18790

    `-m` 对冻结的 exe 毫无意义，Python 解释器不存在了，必然失败。

所以这里在入口最前面把这种 argv 形态**翻译**成正常的 CLI 调用：
剥掉 `-m nanobot` 三个参数，把剩下的原样交给 nanobot 的 CLI main。
效果等价于上游直接调用 `nanobot gateway --foreground ...`。

桌面壳自己也是通过这个入口启动网关的（`nanobot-backend.exe gateway --foreground`），
那条路径不需要翻译，直接透传。

它还修掉第二个坑：**windowed 冻结体的 stdio 为 None**。

    spec 里 `console=False` 让 exe 变成 GUI 子系统（PE Subsystem=2），
    于是**没有控制台**（双击、或被 shell 以 UseShellExecute 拉起）时，
    PyInstaller 会把 `sys.stdout` / `sys.stderr` / `sys.stdin` 置为 **None**。

    上游 `nanobot/cli/entry.py::_configure_windows_console()` 第一句就是：

        if sys.platform != "win32" or sys.stdout.encoding == "utf-8":

    在没有控制台时 `sys.stdout` 是 None，取 `.encoding` 直接抛

        AttributeError: 'NoneType' object has no attribute 'encoding'

    进程当场死掉、**连 traceback 都看不见**（GUI 子系统没有 stderr 可写）。

好在这类流可以简单兜底：用一个暴露 `encoding` 属性的内存流占位。
上游只读 `.encoding`，并在 `suppress(Exception)` 里调 `.reconfigure()`，
所以占位流满足这两点即可。

注意：**不能直接给 `io.StringIO()` 实例赋 `.encoding`** ——
C 实现里该属性只读，会抛
`AttributeError: attribute 'encoding' of '_io._TextIOBase' objects is not writable`。
必须在**子类的类层面**声明。
"""

from __future__ import annotations

import io
import sys


class _NullTextStream(io.StringIO):
    """内存占位流：可读写，且在类层面暴露上游需要的 ``encoding``。"""

    encoding = "utf-8"
    errors = "replace"


def _ensure_std_streams() -> None:
    """给 windowed 冻结体补上占位 stdio，避免 None 引发的 AttributeError。

    只在流确实为 None 时替换；有真实控制台时原样保留，不影响正常输出。
    """
    for name in ("stdout", "stderr", "stdin"):
        if getattr(sys, name, None) is None:
            setattr(sys, name, _NullTextStream())


def main() -> int:
    _ensure_std_streams()

    # 把冻结后的自举调用翻译成正常 CLI 调用。
    # sys.argv 形如 ['<exe>', '-m', 'nanobot', <真实参数...>]
    if len(sys.argv) >= 3 and sys.argv[1] == "-m" and sys.argv[2] == "nanobot":
        sys.argv = [sys.argv[0], *sys.argv[3:]]

    from nanobot.cli.entry import main as nanobot_main

    nanobot_main()
    # nanobot 的 CLI 通过 typer 自行处理退出码；正常返回即视为成功。
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
