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
- **键鼠** — 走 EIS（libei），经 portal 的 RemoteDesktop v2 授权：
  坐标是**逻辑像素**且横跨全部输出，HiDPI 不成问题；代价是首次要过一次授权框
  （restore token 可持久化，之后静默）。见下面「EIS 后端」。
  早期版本用 `/dev/uinput` 合成内核设备，要设备权限、绝对坐标只按单输出物理像素建、
  多屏和混合缩放都会错——2026-10-17 已整个删掉，不再有回退开关。

## 现状（2026-10-17 实测）

| 能力 | 状态 |
|---|---|
| `list_windows` | ✅ 通，输出和 `lswt -j` 一致 |
| 键鼠（`click` / `move` / `scroll` / `type_text` / `key`） | ✅ 走 EIS（libei），不需要 `/dev/uinput` |
| 滚轮方向 | ✅ 实测：dy 正数向上、负数向下（REL_WHEEL 惯例） |
| `screenshot` | ✅ 通，颜色/方向正确 |

**一个重要行为**：锁屏时（cosmic-greeter 挂着）COSMIC 会挂起 capture——`create_session`
一切正常，但 frame 的 `capture()` 永远不回事件，直到解锁。这是合理的（锁屏不该被拍），
但 MCP 调用会等到 5 秒超时。排查这个时曾误判成「shm 路径没实现」，实际是屏幕锁着。

## 工具

| 工具 | 做什么 | 走什么 |
|---|---|---|
| `screenshot` | 截一个输出，存 PNG，返回路径 | ext-image-copy-capture + wl_shm |
| `list_windows` | 列出窗口的 identifier / title / app_id | ext-foreign-toplevel-list |
| `click` | 按截图坐标点击（先移动、等悬停、再按下） | EIS |
| `move` | 移动指针悬停 | EIS |
| `scroll` | 滚轮，`dy` 正数向上、负数向下 | EIS |
| `type_text` | **一律走剪贴板 + ctrl+v**（默认）；`mode:"keys"` 才发 keycode | wl-clipboard + EIS |
| `key` | 按组合键，如 `ctrl+s` | EIS |
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

- **`scroll` 的 `dy` 是轮格数，正数向上**（跟内核 `REL_WHEEL` 一致），负数向下。
  `ei_scroll` 的 y 轴正方向朝下，所以 `src/input.rs` 传进去的是 `-dy`——
  在滚动区底部传 `dy=-1` 看到的是「什么也没发生」，别误判成事件没送到。
- 滚动是用自建可控窗口验证的：`kitty -o font_size=26 sh -c 'seq 1 300; exec sleep 900'`，
  内容带行号，向上滚 3 格视口从 295–300 变成 280–300，向下 15 格精确回底。
  面板时钟那种目标不可靠——它可能压根不响应滚轮。
  新窗口会开在**焦点屏**上（COSMIC 当前的默认行为，没有规范保证），所以两块 output
  都要截图找窗口；要它落在指定屏，先点一下那块屏。

MCP 工具层在 2026-10-17 已切到 EIS：`src/input.rs` 做后端分发，默认 EIS，
EIS 会话常驻（`EiEvent` 不是 `Send`，独占一个线程 + 通道注入命令），
键位表在 `src/keymap.rs`，`/dev/uinput` 已彻底移除。

## 已知限制

1. **拿不到窗口状态**。`ext-foreign-toplevel-list` 协议本身不带 focused / minimized / geometry，只有 `wlr-foreign-toplevel-management` 带，而 COSMIC 没实现它。所以 `list_windows` 只能告诉你「有哪些窗口」，不能告诉你「焦点在哪」——那个要靠截图看。
2. **`type_text` 一律走剪贴板**。合成 keycode 会先进输入法：fcitx5 在中文模式下把
   `abc123` 吃成「按扁成23」，单个 `z` 也会弹候选栏。所以按键路径是 `mode:"keys"`
   显式指定才走，且只收可打印 ASCII——只适合输入法关着、或确实需要真实按键事件的场景。
   剪贴板由 wl-clipboard 提供（打包时自动带进 PATH，无需手装）。
   已实测：`clipboard_write` 中文读回一致，且同时提供 XWayland 兼容的
   `TEXT`/`STRING`/`UTF8_STRING` 类型，Chrome/Electron 应用可粘贴。
   一个特性：Wayland 选区跟着持有进程走，MCP server 退出后剪贴板内容就没了。
3. **点击不转移键盘焦点**。`click` 之后 `type_text` 去的仍是点击**之前**的焦点窗口，
   不是刚点过的那个。要往某个窗口输入，先让用户或窗口管理器把焦点给它。
4. **`key` 的键位表是 US 布局**。事件进合成器的输入管线，由它套布局，而布局问不到；
   非 US 布局请走剪贴板。

## 为什么手写 MCP 协议层，而不是用 SDK

`src/mcp.rs` 的协议层是手写的，约 60 行：stdin 逐行读、解析 JSON-RPC、按 `method` 分发、
逐行写回。只实现 `initialize` / `ping` / `tools/list` / `tools/call`。

理由是依赖体量：官方 Rust SDK `rmcp`（v3.5.1，2026-03 出 1.0，支持规范 2026-07-28）
自带 tokio 运行时抽象和 schemars，会让 crane 的 `cargoArtifacts` 哈希显著变重，
而工具还在快速迭代期，缓存复用率会掉。

**代价是真实的**，不是「反正够用」：

| 缺什么 | 后果 |
|---|---|
| `protocolVersion` 钉在 `2024-11-05` | 新客户端会降级协商；`server/discover`、长任务、订阅都没有 |
| 无 progress notification | 截图、等 portal 授权时客户端看不到任何进度 |
| 无 cancellation | 调用发出去取消不了（EIS 首次授权有 60s 超时） |
| schema 手写 | 与实现分开维护；`scroll` 的 `dy` 描述就曾和实现相反 |

**什么时候该换成 `rmcp`**——出现任意一条就不再省：

1. 需要 progress 反馈（批量截图、等授权这类长任务）
2. 需要 cancellation
3. 要同时服务多个客户端
4. 要 streamable HTTP 传输
5. 工具继续变多，手写 schema 不划算

**迁移路径**，不必一步到位：

1. 先把工具 schema 改成从 Rust 结构体 derive（自己引 `schemars`，不引 rmcp），
   根除 schema 漂移——`dy` 那次就是它导致的。
2. 真需要 progress / 长任务 / HTTP 时再整体换 `rmcp` 的 `#[tool]` + `ServerHandler`；
   `dispatch()` 里那些 `tool_*` 函数的签名基本能原样搬过去。

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
toplevels: 2
  WorkBuddy [yKlXztLTFkE6KoyiT4A5RQgQ9OxjLidx] WorkBuddy
  kitty [yMeUdgSO2ua7TSqYvAllKYApFGasyKEg] hx ~/S/p/cosmic-desktop-mcp
capture #0: 1920x1080 ok (8294400 bytes rgba)
capture #1: 2560x1440 ok (14745600 bytes rgba)
backend: eis (libei through the RemoteDesktop portal)
device "virtual keyboard" serial=2 absolute=false button=false keyboard=true pointer=false scroll=false
device "virtual pointer" serial=3 absolute=false button=true keyboard=false pointer=true scroll=true
device "virtual absolute pointer" serial=4 absolute=true button=true keyboard=false pointer=false scroll=true
region Some("eDP-1") logical x=384 y=1152 1280x720 scale=1.5 -> physical 1920x1080
region Some("DP-2") logical x=0 y=0 2048x1152 scale=1.25 -> physical 2560x1440
```

最后那段 `backend:` 及其后是输入后端自述：会列出合成器给的三类虚拟设备
（键盘 / 相对指针 / 绝对指针）和每个输出的 region，region 就是坐标换算的依据。

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
- `ei` (libei) — https://libei.readthedocs.io/
- `input-event-codes` — 键位/按钮码表，`src/keymap.rs` 里手写，只覆盖用得到的部分

## License

MIT
