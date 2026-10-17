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
- **键鼠** — 两条路，默认 uinput：
  - `/dev/uinput`：无授权流程、无依赖，但绝对坐标按物理像素建，HiDPI 映射要自己扛
  - **EIS（libei）**：走 portal 的 RemoteDesktop v2，坐标是**逻辑像素**且横跨全部输出，
    HiDPI 问题不存在；代价是首次要过一次授权框（restore token 可持久化，之后静默）。
    已实测可用，见下面「EIS 后端」

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
| `type_text` | **一律走剪贴板 + ctrl+v**（默认）；`mode:"keys"` 才发 keycode | uinput + wl-clipboard |
| `key` | 按组合键，如 `ctrl+s` | /dev/uinput |
| `clipboard_write` | 写剪贴板（不粘贴） | wl-copy（来自 wl-clipboard） |
| `clipboard_read` | 读剪贴板 | wl-paste |

坐标一律是**截图像素的坐标**，截图不缩放，所以不需要换算。

## EIS 后端（实测通过）

`--eis-probe` / `--eis-click X Y` 走 RemoteDesktop portal 拿一个 libei 连接，合成器报上来的设备：

```
eis: device type=Virtual name=Some("virtual keyboard")
eis:   interfaces pointer_absolute=false button=false keyboard=true
eis: device type=Virtual name=Some("virtual absolute pointer")
eis:   region x=384 y=1152 1280x720  scale=1.5  mapping_id=eDP-1
eis:   region x=0   y=0    2048x1152 scale=1.25 mapping_id=DP-2
eis:   interfaces pointer_absolute=true button=true keyboard=false
```

关键点：

- region 是**逻辑坐标**下完整的桌面拼图（2560x1440 的屏在 scale 1.25 下就是 2048x1152），
  多屏是一张连续坐标系，`--eis-click 1024 8` 实测正好点在面板时钟上，日历弹窗打开。
- **EIS 点击不转移键盘焦点**（2026-10-17 实测）：点击落在哪个窗口，键盘焦点都不动。
  所以「点一下输入框再打字」不管用，除非焦点本来就那个窗口——paste 进不去框的原因
  就是它。另一个后果：`key` / `type_text` 发出的按键去的是**当前焦点窗口**，不一定是
  刚点过的那个。
- 授权用 `PersistMode::ExplicitlyRevoked`，restore token 存在
  `$XDG_STATE_HOME/cosmic-desktop-mcp/restore-token`；之后每次连接**静默通过**，
  不再弹框。token 能注入输入，别把它提交进任何仓库。
- 排查时踩过的坑：reis 把请求缓冲在本地，**每个事件循环后必须 `connection.flush()`**，
  否则 `ei_seat.bind` 根本发不出去，表现为「seat 到了但永远没有 device」。
- EIS 会话期间合成器会限制真实输入吗？实测不会——点击、截图都正常。

还没有做的事：MCP 工具层（`click` 等）仍走 uinput；把 EIS 做成长驻会话的后端
（EiEvent 不是 Send，需要独占线程 + 通道）是下一步，收益是丢掉 uinput 依赖和 HiDPI 坐标问题。

## 已知限制

1. **拿不到窗口状态**。`ext-foreign-toplevel-list` 协议本身不带 focused / minimized / geometry，只有 `wlr-foreign-toplevel-management` 带，而 COSMIC 没实现它。所以 `list_windows` 只能告诉你「有哪些窗口」，不能告诉你「焦点在哪」——那个要靠截图看。
2. **`type_text` 一律走剪贴板**。合成 keycode 会先进输入法：fcitx5 在中文模式下把
   `abc123` 吃成「按扁成23」，单个 `z` 也会弹候选栏。所以按键路径是 `mode:"keys"`
   显式指定才走，且只收可打印 ASCII——只适合输入法关着、或确实需要真实按键事件的场景。
   剪贴板由 wl-clipboard 提供（打包时自动带进 PATH，无需手装）。
   已实测：`clipboard_write` 中文读回一致，且同时提供 XWayland 兼容的
   `TEXT`/`STRING`/`UTF8_STRING` 类型，Chrome/Electron 应用可粘贴。
   一个特性：Wayland 选区跟着持有进程走，MCP server 退出后剪贴板内容就没了。
3. **坐标只对主输出（index 0）**。`/dev/uinput` 的绝对坐标范围按主输出尺寸建，多屏时第二块屏的坐标会错。截图本身可以指定 `output`。
4. **HiDPI 下的坐标映射未验证**。这台机器 `scale=2`，`wl_output` 的 mode 报的是物理像素（1920x1080），uinput 的绝对范围也按物理像素建；compositor 把绝对指针映射到逻辑坐标时是乘还是除 scale，需要实测一次 `click` 才能确认。`scale=1` 的机器没有这个问题。EIS 后端没有这个问题。
5. **`/dev/uinput` 需要写权限**。NixOS 上加一条 udev 规则，或者把用户放进 `uinput` 组。EIS 后端不需要。

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
