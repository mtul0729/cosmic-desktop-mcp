# cosmic-desktop-mcp

[English](README.md) | **简体中文**

一个小型开源工具，通过 MCP 提供原生 COSMIC 桌面操作：截屏、查看窗口和工作区、移动鼠标、
点击、滚动、输入文字。它直接使用 COSMIC 原生支持的 Wayland 协议，不依赖 `xdotool`、
`scrot` 这类 X11 工具。

适合 UI 测试、桌面排障、面板小程序调试，以及需要人工监督的轻量桌面自动化。

> **平台：** Linux + COSMIC Wayland，已在 COSMIC 1.9.0 上验证。MCP 包提供
> `x86_64-linux` 和 `aarch64-linux` 两个版本。

## 设计

这是一个小而专注的工具，不是通用桌面自动化平台。它不是 shell 替代品，也不是安全边界；
调用方应本来就能访问这台机器，MCP 只是暴露桌面操作的接口。

工具面只保留 COSMIC 桌面状态、坐标和输入相关内容。操作流程、陷阱和普通 shell 配方放在
[操作 skill](skill/cosmic-desktop-ops/SKILL.md)中。工具面、副作用、坐标处理和错误行为
背后的取舍见[适用范围与设计原则](docs/design.zh-CN.md)。

## 能做什么

| 能力 | 说明 |
|---|---|
| 观察 | 截取整个输出或单个窗口；列出窗口、几何位置和工作区归属 |
| 操作 | 移动、单击、双击、滚动、输入文字、发送组合键、切换工作区 |
| 窗口 | 激活、关闭、最小化、最大化、全屏 |
| 剪贴板 | 读写文本，也可以先写入而不立即粘贴 |
| COSMIC 调试 | 列出面板小程序、刷新面板区域、读写 COSMIC 配置 |

完整参数见[工具参考](docs/tool-reference.zh-CN.md)。

## 开始之前

- 一个 COSMIC Wayland 会话。只支持 XWayland 的工具看不到原生 COSMIC 窗口。
- RemoteDesktop portal。COSMIC 自带；第一次执行输入操作时，系统会请求授权。
- 推荐使用启用了 flakes 的 [Nix](https://nixos.org/) 安装。也可以用普通的 Rust 构建。

Nix 包已经包含 `wl-clipboard`，剪贴板和粘贴功能会用到它。

## 快速开始

先运行自检：

```bash
nix run github:mtul0729/cosmic-desktop-mcp -- --doctor
```

自检会截取每个输出并打开输入会话，因此可能弹出系统的 RemoteDesktop 授权对话框。批准一次
后，后续连接会使用保存的恢复令牌。

安装：

```bash
nix profile install github:mtul0729/cosmic-desktop-mcp
cosmic-desktop-mcp --doctor
```

或者只构建本地二进制：

```bash
nix build github:mtul0729/cosmic-desktop-mcp
./result/bin/cosmic-desktop-mcp --doctor
```

### 接入 MCP 客户端

服务通过 stdio 使用 MCP。无参数启动即可：

```bash
cosmic-desktop-mcp
```

Codex 的配置示例：

```toml
[mcp_servers.cosmic-desktop]
command = "cosmic-desktop-mcp"
env_vars = ["WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "XDG_SESSION_ID"]
```

使用通用 JSON 配置的客户端：

```json
{
  "mcpServers": {
    "cosmic-desktop": {
      "command": "cosmic-desktop-mcp"
    }
  }
}
```

推荐显式传递 Wayland 环境变量。服务可以发现运行目录中唯一的 `wayland-*` socket，但如果
同时存在多个 socket，它会拒绝猜测。如果你覆盖了 `XDG_CONFIG_HOME` 或 `XDG_STATE_HOME`，
也要一并传递。修改 MCP 客户端配置后需要重启客户端。

### NixOS

在 flake 中添加入口：

```nix
{
  inputs.cosmic-desktop-mcp.url = "github:mtul0729/cosmic-desktop-mcp";
}
```

然后在 NixOS 模块中安装（假设 `inputs` 已通过 `specialArgs` 传入）：

```nix
{ pkgs, inputs, ... }: {
  environment.systemPackages = [
    inputs.cosmic-desktop-mcp.packages.${pkgs.system}.default
  ];
}
```

不要添加
`inputs.cosmic-desktop-mcp.inputs.nixpkgs.follows = "nixpkgs"`。发布用的 derivation
基于本 flake 自己的 nixpkgs 构建，并已推送到 `mtul` Cachix；跟随你的 nixpkgs 会改变
derivation，导致所有 Rust 依赖重新编译。更多打包细节见 [NIX.md](NIX.md)。

### 用 Cargo 构建

```bash
nix develop
cargo build --release
./target/release/cosmic-desktop-mcp --doctor
```

如果不在开发 shell 中构建，需要另外安装 `wl-clipboard`，并确保 `wl-copy` / `wl-paste`
在 `PATH` 中。

## 第一次使用

建议按下面的顺序完成第一次交互：

1. 调用 `env_info`。它报告输出和输入状态，但不会打开授权对话框。
2. 调用 `screenshot`，打开返回的 PNG。记下响应中的输出索引。
3. 调用 `list_windows`，通过 identifier、app ID 或标题找到目标窗口。
4. 对目标调用 `screenshot_window`，保留返回的 `coordinates` 对象。
5. 从窗口图片上选一个点，调用 `click_window`，并原样传入 `coordinates`。
6. 输入文字前，先调用 `window_action`，参数为 `action: "activate"`；再调用
   `type_text`。
7. 再截一次图，确认操作结果。

桌面上的点使用 `click` / `move` / `scroll`，坐标必须是某次 `screenshot` 的像素坐标，
并传入**相同的输出索引**。窗口内部的点始终使用对应的 `*_window` 工具。

第一次执行输入操作时，系统可能弹出 RemoteDesktop 授权对话框并等待人工批准。如果等待失败，
服务会返回结构化的 `authorization_timeout`、`authorization_denied` 或
`authorization_unavailable` 错误。

## 唯一需要记住的坐标规则

响应中会出现三种坐标空间：

| 坐标空间 | 来源 | 接收方 |
|---|---|---|
| 输出截图像素 | `screenshot` | `click`、`move`、`scroll`，同时传入相同的 `output` |
| 窗口图片像素 | `screenshot_window` | `click_window`、`move_window`、`scroll_window`，同时传入它的 `coordinates` 对象 |
| 逻辑桌面坐标 | COSMIC 合成器 | 仅供内部换算；没有 MCP 工具直接接收 |

**不要把窗口原点加到坐标上再调用 `click`。** 应该调用 `click_window`，并把提供该点的
那次截图返回的完整 `coordinates` 对象原样传回去。混用坐标空间可能悄悄点到另一块屏幕。

`click` 默认使用输出 0，且绝不会根据 `x`/`y` 推断输出。如果指针操作响应中包含
`other_output_match`，说明这组坐标来自另一块屏幕的截图。

## 容易踩的坑

- **点击不会移动键盘焦点。** 发送按键前先激活目标窗口：
  `window_action activate` → `type_text` / `key`。
- **窗口几何位置不等于可见性。** 窗口可能被遮挡，或位于当前未显示的工作区。
  `capture.source` 和 `on_active_workspace` 会说明你拿到的图片属于哪种情况；如果图片内容
  必须和点击落点一致，先激活窗口。
- **`type_text` 默认走剪贴板。** 这是有意设计：合成按键会经过输入法，在中文输入法下可能
  变成候选词。`mode: "keys"` 是需要显式开启的、仅支持 ASCII 的替代方案。粘贴模式会覆盖
  当前剪贴板。
- **锁屏会暂停截图。** logind 可查询时，`screenshot` 会报告 `screen_locked`；否则可能表现为
  截图超时。请让用户先解锁会话。
- **不要 kill 面板小程序进程。** COSMIC 不一定会把它拉起来。使用 `restart_applet`，它会
  刷新整个面板区域，并保留配置备份。

## 工具概览

| 分组 | 工具 |
|---|---|
| 状态 | `env_info`、`prepare_input` |
| 观察 | `screenshot`、`list_windows`、`screenshot_window` |
| 窗口 | `window_action` |
| 指针 | `click`、`move`、`scroll`、`click_window`、`move_window`、`scroll_window` |
| 键盘与剪贴板 | `type_text`、`key`、`clipboard_write`、`clipboard_read` |
| 工作区 | `workspaces` |
| COSMIC 调试 | `applets`、`restart_applet`、`config_read`、`config_write` |

观察类工具不会打开输入会话。`prepare_input` 和实际操作工具会在第一次使用时才打开输入
会话，因此可能弹出授权对话框。

## 排障

| 现象 | 检查项 |
|---|---|
| MCP 服务启动后报告找不到 Wayland socket | 传递 `WAYLAND_DISPLAY`、`XDG_RUNTIME_DIR`、`XDG_SESSION_ID`，然后重启 MCP 客户端 |
| 第一次点击一直等待或超时 | 批准 RemoteDesktop 对话框；查看结构化授权状态 |
| 点击落到了另一块屏幕 | 使用与截图相同的输出索引，或改用 `click_window` |
| 点击没有任何反应 | 查看响应里的 `hit`、`on_active_workspace` 和 `other_output_match` |
| 文字进入了错误的窗口 | 在输入前立即激活目标窗口 |
| `screenshot_window` 返回 `needs_input_session` | 调用 `prepare_input`，批准对话框，再重新截窗口 |
| 截图超时，或画面像登录界面 | 检查会话是否已锁定 |
| 窗口截图内容与直接捕捉不一致 | 查看 `capture.source`；如果图片和点击必须一致，先激活窗口 |

更多操作配方见 [COSMIC 桌面操作 skill](skill/cosmic-desktop-ops/SKILL.md)。

## 安全与隐私

这个服务能观察并控制你的桌面，只应暴露给可信的调用方。RemoteDesktop 恢复令牌保存在
`$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token`（通常还有状态目录回退路径）。它可以
授权输入注入，应视为机密，绝不能提交到仓库。

MCP 只是接口，不是沙箱。即使客户端只能访问这个服务，也不代表桌面控制就是安全或最小权限的。

`config_write` 会修改 COSMIC 配置，`restart_applet` 会刷新面板进程。两者都会返回可用于
回滚的信息，但 Agent 在做破坏性修改前应先征得确认。

## 文档

- [工具参考](docs/tool-reference.zh-CN.md) / [Tool reference](docs/tool-reference.md)
- [适用范围与设计原则](docs/design.zh-CN.md) / [Scope and design principles](docs/design.md)
- [工程笔记](docs/engineering-notes.md) —— 协议研究、实测数据和已知陷阱
- [Nix 打包说明](NIX.md)
- [Agent 操作 skill](skill/cosmic-desktop-ops/SKILL.md)

## 开发

```bash
nix develop
cargo check
cargo test
cargo clippy
```

如果重命名工具或修改参数，请同时更新工具定义、两份工具参考，以及
`skill/cosmic-desktop-ops/SKILL.md`。普通 shell 配方放在 skill 中；只有需要 COSMIC
特有状态或语义的操作才应该新增 MCP 工具。

## 许可证

MIT
