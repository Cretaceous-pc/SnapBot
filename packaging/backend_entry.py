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
"""

from __future__ import annotations

import sys


def main() -> int:
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
