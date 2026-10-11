# -*- mode: python ; coding: utf-8 -*-
"""PyInstaller 规格文件：把 nanobot 后端冻结成可分发目录。

构建（建议用 packaging/build-backend.ps1，它会带上必需的参数）：

    pyinstaller packaging/snapbot-backend.spec \
        --distpath packaging/dist --workpath packaging/build --noconfirm

为什么不写 .spec 里的 Analysis(['backend_entry.py']) 相对路径：
PyInstaller 以 CWD 解析相对路径，而 CWD 会随调用方式变化。
所以下面用 SPECPATH 构造绝对路径。

三个必须显式处理的点（漏了会以各种诡异方式失败）：
  1. collect_submodules —— 上游用 pkgutil.iter_modules 发现内置工具与渠道，
     冻结后**枚举不到**，工具会静默消失。
  2. datas —— web/dist / templates / skills / channels/*/webui 都是非 .py 资源，
     PyInstaller 不会自动收集。漏 web/dist → 界面白屏；漏 templates → 不生成工作区文件。
  3. 属性访问式 datas —— 必须用 (src, dest) **元组**，写成列表会被当成字符串序列，
     生成出逐字符的垃圾目录。
"""

import os
from pathlib import Path

from PyInstaller.utils.hooks import collect_submodules

# SPECPATH 由 PyInstaller 注入，指向本文件所在目录（packaging/）
SPEC_DIR = Path(SPECPATH).resolve()
REPO_ROOT = SPEC_DIR.parent
UPSTREAM = REPO_ROOT / "nanobot-0.3.5"
PACKAGE = UPSTREAM / "nanobot"

if not (UPSTREAM / "pyproject.toml").is_file():
    raise SystemExit(
        f"找不到上游检出：{UPSTREAM}\n"
        "请先运行 scripts/restore-upstream.ps1"
    )

# ---- 1) 动态发现的子模块 ----------------------------------------------------
# pkgutil.iter_modules 在冻结环境里枚举不到东西，必须显式收集。
# 但要**过滤掉测试模块**：collect_submodules 会把每个渠道的 tests/ 也收进来，
# 白白增大产物并拖慢启动。
def _real_modules(package: str) -> list[str]:
    return [
        name
        for name in collect_submodules(package)
        if ".tests" not in name and ".test_" not in name
    ]


hiddenimports = (
    _real_modules("nanobot.agent.tools")
    + _real_modules("nanobot.channels")
    + _real_modules("nanobot.providers")
)

# ---- 2) 非 .py 资源 --------------------------------------------------------
datas = [
    (str(PACKAGE / "web" / "dist"), "nanobot/web/dist"),
    (str(PACKAGE / "templates"), "nanobot/templates"),
    (str(PACKAGE / "skills"), "nanobot/skills"),
]

# 各渠道自带的 webui 静态资源
channels_dir = PACKAGE / "channels"
if channels_dir.is_dir():
    for channel in sorted(channels_dir.iterdir()):
        webui = channel / "webui"
        if webui.is_dir():
            datas.append((str(webui), f"nanobot/channels/{channel.name}/webui"))

# ---- 3) 明确排除 ------------------------------------------------------------
excludes = [
    # 开发/测试期依赖，不该进分发产物
    "pytest",
    "pytest_asyncio",
    "pytest_cov",
    "pytest_xdist",
    "basedpyright",
    "ruff",
    # 各渠道的测试包（hiddenimports 已过滤，这里再兜一层）
    "nanobot.channels.qq.tests",
    "nanobot.channels.weixin.tests",
    "nanobot.channels.wecom.tests",
    "nanobot.channels.feishu.tests",
    "nanobot.channels.websocket.tests",
    "nanobot.channels.telegram.tests",
    "nanobot.channels.discord.tests",
    "nanobot.channels.slack.tests",
    "nanobot.channels.signal.tests",
    "nanobot.channels.matrix.tests",
    "nanobot.channels.mattermost.tests",
    "nanobot.channels.msteams.tests",
    "nanobot.channels.dingtalk.tests",
    "nanobot.channels.napcat.tests",
    "nanobot.channels.whatsapp.tests",
    "nanobot.channels.email.tests",
    # 本机可能装了但后端用不到的 GUI 栈
    "tkinter",
    "PyQt5",
    "PySide2",
    "PySide6",
    "matplotlib",
]

a = Analysis(  # noqa: F821  (PyInstaller 注入这些名字)
    [str(SPEC_DIR / "backend_entry.py")],
    pathex=[str(UPSTREAM)],
    binaries=[],
    datas=datas,
    hiddenimports=hiddenimports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=excludes,
    noarchive=False,
    # ---- 关键：让这两个包以**真实文件**形式落在磁盘上 --------------------
    #
    # 上游用 `pkgutil.iter_modules(package.__path__)` 枚举渠道与工具
    # （nanobot/channels/registry.py:25、nanobot/agent/tools/loader.py:43）。
    # 冻结后 `__path__` 指向 PYZ 归档内部，iter_modules 需要扫描真实目录，
    # 因此**什么都枚举不到** —— 表现为启动日志里的 "No channels enabled"，
    # 渠道与工具静默消失，且没有任何报错。
    #
    # hiddenimports 只保证模块"可导入"，不解决"可枚举"。
    # 必须用 module_collection_mode 把它们从归档里拿出来、作为散装文件落地。
    # 用 'py+pyz'：既在归档里留一份（导入快），又在磁盘上留一份（可枚举）。
    module_collection_mode={
        "nanobot.channels": "py+pyz",
        "nanobot.agent.tools": "py+pyz",
    },
)

pyz = PYZ(a.pure)  # noqa: F821

exe = EXE(  # noqa: F821
    pyz,
    a.scripts,
    [],
    exclude_binaries=True,
    name="nanobot-backend",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,          # UPX 压缩会显著提高杀软误报率
    console=False,      # 桌面应用不需要控制台窗口
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)

coll = COLLECT(  # noqa: F821
    exe,
    a.binaries,
    a.datas,
    strip=False,
    upx=False,
    upx_exclude=[],
    name="nanobot-backend",
)
