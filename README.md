# cosmic-desktop-mcp

一个让 agent 能看见并操作 COSMIC 桌面的 MCP server：截图、列窗口、鼠标点击、键盘输入。

跑在 Wayland 上，用 COSMIC 真正实现的协议，不依赖 X11，也不经过 xdg-desktop-portal。

## 为什么要自己写

公开的 Linux 桌面 MCP 都是 X11 出身（xdotool + scrot），或者绑 GNOME Shell 扩展。COSMIC 上都不成立：

| 常见做法 | 在 COSMIC 上 |
|---|---|
| `xdotool` / `scrot` | 只对 XWayland 应用有效，原生窗口点不到 |
| `grim` / `wayshot`（wlr-screencopy） | COSMIC 不实现这个协议，直接失败 |
| `cosmic-screenshot` 非交互模式 | 卡死（portal 不响应，30s 超时） |
| `gnome-desktop-mcp` | 需要 GNOME Shell 45-49 扩展，无关 |
| `ydotool` | 相对指针 + 指针加速，长距离跳跃会偏 |

实测 `cosmic-comp 1.9.0` 暴露了 62 个 wayland 全局接口，够用的有三条：

- **截图** — `ext_image_copy_capture_manager_v1`（staging），不需要对话框
- **列窗口** — `ext_foreign_toplevel_list_v1`（staging），和 `lswt` 看到的是同一份数据
- **键鼠** — 没有 `zwp_virtual_pointer_manager_v1`，只能走 `/dev/uinput`

## 现状（2026-10-17 实测）

| 能力 | 状态 |
|---|---|
| `list_windows` | ✅ 通，输出和 `lswt -j` 一致 |
| 键鼠（`click` / `move` / `scroll` / `type_text` / `key`） | ✅ `/dev/uinput` 设备创建成功 |
| `screenshot` | ✅ 通，颜色/方向正确 |

**一个重要行为**：锁屏时（cosmic-greeter 挂着）COSMIC 会挂起 capture——`create_session`
一切正常，但 frame 的 `capture()` 永远不回事件，直到解锁。这是合理的（锁屏不该被拍），
但 MCP 调用会等到 5 秒超时。排查这个时曾误判成「shm 路径没实现」，实际是屏幕锁着。

## 工具

| 工具 | 做什么 | 走什么 |
|---|---|---|
| `screenshot` | 截一个输出，存 PNG，返回路径 | ext-image-copy-capture + wl_shm |
| `list_windows` | 列出窗口的 identifier / title / app_id | ext-foreign-toplevel-list |
| `click` | 按截图坐标点击（先移动、等悬停、再按下） | /dev/uinput |
| `move` | 移动指针悬停 | /dev/uinput |
| `scroll` | 滚轮 | /dev/uinput |
| `type_text` | 打 ASCII 文本 | /dev/uinput |
| `key` | 按组合键，如 `ctrl+s` | /dev/uinput |

坐标一律是**截图像素的坐标**，截图不缩放，所以不需要换算。

## 已知限制

1. **拿不到窗口状态**。`ext-foreign-toplevel-list` 协议本身不带 focused / minimized / geometry，只有 `wlr-foreign-toplevel-management` 带，而 COSMIC 没实现它。所以 `list_windows` 只能告诉你「有哪些窗口」，不能告诉你「焦点在哪」——那个要靠截图看。
2. **`type_text` 只支持 ASCII**。uinput 发的是按键，不是字符，中文需要输入法。中文内容请走剪贴板。
3. **坐标只对主输出（index 0）**。`/dev/uinput` 的绝对坐标范围按主输出尺寸建，多屏时第二块屏的坐标会错。截图本身可以指定 `output`。
4. **HiDPI 下的坐标映射未验证**。这台机器 `scale=2`，`wl_output` 的 mode 报的是物理像素（1920x1080），uinput 的绝对范围也按物理像素建；compositor 把绝对指针映射到逻辑坐标时是乘还是除 scale，需要实测一次 `click` 才能确认。`scale=1` 的机器没有这个问题。
4. **`/dev/uinput` 需要写权限**。NixOS 上加一条 udev 规则，或者把用户放进 `uinput` 组。

## 安装

```bash
nix build github:mtul0729/cosmic-desktop-mcp
```

产物是 `result/bin/cosmic-desktop-mcp`。

接进 WorkBuddy（`~/.workbuddy/mcp.json`）：

```json
{
  "mcpServers": {
    "cosmic-desktop": {
      "command": "/path/to/cosmic-desktop-mcp"
    }
  }
}
```

写进 NixOS 配置的话：

```nix
{
  inputs.cosmic-desktop-mcp.url = "github:mtul0729/cosmic-desktop-mcp";
  inputs.cosmic-desktop-mcp.inputs.nixpkgs.follows = "nixpkgs";
  # ...
  environment.systemPackages = [ inputs.cosmic-desktop-mcp.packages.${pkgs.system}.default ];
}
```

## 自检

不要一上来就接 MCP，先跑：

```bash
cosmic-desktop-mcp --doctor
```

它依次验证四件事，任何一步失败都会直接指出是哪一步。上面这台机器的真实输出：

```
wayland: Ok("wayland-1")
output #0: 1920x1080 scale=2
output #1: 2560x1440 scale=2
toplevels: 4
  WorkBuddy [yKlXztLTFkE6KoyiT4A5RQgQ9OxjLidx] WorkBuddy
  google-chrome [bDnZaXHkKVBXExpsn0VF15TVqL9BPpON] Build sticky-notes - Google Chrome
  kitty [62RB3VJhOwe73CtJINYPQ6RbMUyyy2od] hx .github/workflows ~/S/p/sticky-notes
  org.b3log.siyuan [CpFiIdqTYDPeKlAFr5a028BXqgenCFzi] 20260919134006 - SiYuan - 思源笔记 v3.8.7
capture #0: 1920x1080 ok (8294400 bytes rgba)
capture #1: 2560x1440 ok (14745600 bytes rgba)
uinput: ok (1920x1080)
```

## 开发

```bash
nix develop          # cargo / rustc / rustfmt / clippy
cargo check
cargo build
```

打包细节见 [NIX.md](NIX.md)。

## 协议参考

- `ext-image-copy-capture` — https://wayland.app/protocols/ext-image-copy-capture-v1
- `ext-foreign-toplevel-list` — https://wayland.app/protocols/ext-foreign-toplevel-list-v1
- `linux/uinput.h` — ioctl 号是稳定 ABI，所以这里手写而不是引 bindings crate

## License

MIT
