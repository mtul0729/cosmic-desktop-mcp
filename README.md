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
| `list_windows` | ✅ 通，带状态与几何（见「窗口几何」一节的实测） |
| `screenshot_window` | ✅ 通，DP-2 1.25 缩放下裁剪严丝合缝；被盖住的窗口要先 activate |
| `window_action` | ✅ activate/close/maximize/minimize/fullscreen 实测生效，state 数组跟随变化 |
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
| `env_info` | 桌面拓扑 + 输入后端状态（无副作用，不建 EIS 会话） | wl_output + EIS region |
| `list_windows` | 窗口列表 + 状态 + **几何**（logical 与截图像素两套坐标） | ext-foreign-toplevel-list + zcosmic-toplevel-info |
| `screenshot_window` | 找到窗口所在输出，整屏截图后裁出窗口，返回图内偏移 | capture + zcosmic-toplevel-info + EIS region |
| `window_action` | activate / close / maximize / minimize / fullscreen 等 | zcosmic-toplevel-manager |
| `click` | 按截图坐标点击（先移动、等悬停、再按下） | EIS |
| `move` | 移动指针悬停 | EIS |
| `scroll` | 滚轮，`dy` 正数向上、负数向下 | EIS |
| `type_text` | **一律走剪贴板 + ctrl+v**（默认）；`mode:"keys"` 才发 keycode | wl-clipboard + EIS |
| `key` | 按组合键，如 `ctrl+s` | EIS |
| `clipboard_write` | 写剪贴板（不粘贴） | wl-copy（来自 wl-clipboard） |
| `clipboard_read` | 读剪贴板 | wl-paste |
| `workspaces` | 列/切 workspace（group + name + 状态） | ext-workspace-manager |
| `applets` | 列面板 applet 进程：名字 / PID / 父进程 / 内存 / 年龄 | /proc |
| `restart_applet` | 刷新面板上的 applet（重写 `plugins_*` 配置） | cosmic-panel 配置文件 |
| `app_logs` | 读某个 applet 的 journal 日志 | journalctl |
| `config_read` | 读 cosmic-config 键（RON） | `~/.config/cosmic` |
| `config_write` | 写/删 cosmic-config 键，返回旧值便于回滚 | `~/.config/cosmic` |

坐标一律是**截图像素的坐标**，截图不缩放，所以不需要换算。

给 agent 用的**工作流**（先 `env_info`、`activate` 再打字、不要 `kill` applet 之类的
顺序与禁忌）不在工具描述里，而是单独一个配套 skill：`cosmic-desktop-ops`
（[`mtul0729/agent-skills`](https://github.com/mtul0729/agent-skills)）。工具 schema
只管"这次调用做什么"，而每次 `tools/list` 都要为它付费；判断该调哪个、会踩什么坑属于
按需加载的知识，塞进 schema 是净亏。

## COSMIC 专用操作

操纵桌面只是用途之一；调试 COSMIC 自己的应用、widgets 和 applets 是另一条主线。
下面几条都是在这台机器（COSMIC 1.9.0）上实测的，决定了工具的形状。

### applet 是进程，但 panel 不监督它

每个 applet 都是独立进程，由 `cosmic-panel` 启动，两块屏各一套。`cosmic-panel` 源码里确实
建了 launch-pad 的 `ProcessManager` 并设了 `RestartMode::ExponentialBackoff`，但**实测
SIGTERM 之后 panel 不会把 applet 拉回来**，槽位就那么空着。

唯一观测到有效的重启手段是**重写 panel 的 `plugins_*` 配置**：把列出该 applet 的键写成
`None`，等 config watcher 看到，再把原文写回。panel 会重建整个 section，所以
`restart_applet` 实际是「刷新面板」——同一面板上的 time / workspaces / audio 都会拿到新
PID。工具无条件写回原文，备份放在 `~/.local/state/cosmic-desktop-mcp/panel-backups/`。

```
~/.config/cosmic/com.system76.CosmicPanel/v1/entries          -> ["Panel", "Dock"]
~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_center
~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings
```

### workspace 没有 id，而且每块屏一组

`ext_workspace_manager_v1` 的 `id` 事件是协议要求必须发的，但 COSMIC 不发。所以 workspace
只能用「group + name + 网格坐标」指认。又因为 workspace **按输出分组**，两块屏都有叫 `"1"`
和 `"2"` 的 —— 名字歧义时必须显式给 `group`，工具会报错而不是猜哪块屏。group 只能按
compositor 报告顺序编号：`workspace_enter` 给的 `wl_output` 代理在这里不带连接器名。

### AT-SPI 拿不到 widget 树

`at-spi2-registryd` 在跑，但 a11y 总线上只有它自己和 WorkBuddy 的 magpie，**没有任何 COSMIC
应用注册**。libcosmic 的 accesskit 走 `cosmic_a11y_manager_v1`，不是 AT-SPI。所以「查某个
applet 里有哪些按钮」对 COSMIC 是断的，widget 级调试只能是黑盒：截图 + 点击 + 日志。

### 窗口几何：`zcosmic_toplevel_info_v1`（2026-10-17 实测接通）

标准协议不给窗口的屏幕位置（`ext_foreign_toplevel_list_v1` 只有 identifier/title/app_id），
位置只在 COSMIC 私有的 `zcosmic_toplevel_info_v1`（v2+）里。实测结论：

- **不用自带 XML + build.rs**：crates.io 的 `cosmic-protocols 0.2` 直接带生成好的绑定
  和它要扫的 XML，加依赖就行，`cleanCargoSource` 一个字都不用改。之前「下一步要自带 XML」
  的判断过时了。
- **geometry 是相对输出的逻辑坐标**（实测核对：DP-2 scale 1.25，kitty logical 1012x1104
  at (1028,40) → 截图裁出 1265x1380 at (1285,50)，整数倍严丝合缝）。换算成截图像素只乘
  region.scale，不碰 `wl_output.scale`（它恒报 2，不可信）。
- **初始状态走 compositor 的帧提交路径**：`get_cosmic_toplevel` 之后，cosmic-comp 要等
  下一次渲染帧才推 states/geometry（`ToplevelInfoState::refresh` 挂在 commit 流程里）。
  空闲桌面什么都不发。所以代码里轮询至多 600ms，看到第一个几何就收手。
- **geometry 是「最后报告的位置」，不是可见性**：mpv 全屏后 kitty 的 geometry 还停在
  tile 位置，按它裁出来的是盖在上面的 mpv。要真实内容先 `window_action activate` 把
  窗口带到顶层。
- **states 数组实测可用**（activated / maximized / minimized / fullscreen / sticky），
  且 `window_action activate` 真的换焦点——这补上了「EIS 点击不转移键盘焦点」的坑：
  点完再 activate，`type_text` 就能落到刚点过的窗口。

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
  修复路径：`window_action {action:"activate"}` 走 zcosmic-toplevel-manager 换焦点，
  然后再 `type_text`。另一个后果：`key` / `type_text` 发出的按键去的是**当前焦点窗口**，
  不一定是刚点过的那个。
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

1. **`type_text` 一律走剪贴板**。合成 keycode 会先进输入法：fcitx5 在中文模式下把
   `abc123` 吃成「按扁成23」，单个 `z` 也会弹候选栏。所以按键路径是 `mode:"keys"`
   显式指定才走，且只收可打印 ASCII——只适合输入法关着、或确实需要真实按键事件的场景。
   剪贴板由 wl-clipboard 提供（打包时自动带进 PATH，无需手装）。
   已实测：`clipboard_write` 中文读回一致，且同时提供 XWayland 兼容的
   `TEXT`/`STRING`/`UTF8_STRING` 类型，Chrome/Electron 应用可粘贴。
   一个特性：Wayland 选区跟着持有进程走，MCP server 退出后剪贴板内容就没了。
2. **点击不转移键盘焦点**。`click` 之后 `type_text` 去的仍是点击**之前**的焦点窗口，
   不是刚点过的那个。现在有修复路径：`window_action {action:"activate"}` 走
   zcosmic-toplevel-manager 把焦点切过去（实测生效），再 `type_text`。
3. **geometry 是最后报告的位置，不是可见性**。窗口被盖住（全屏视频）或在其它 workspace 时，
   按它的坐标裁出来的是屏幕上那个区域的实际像素。要真实内容先 `window_action activate`。
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
  # 注意：这里**不要**写 inputs.cosmic-desktop-mcp.inputs.nixpkgs.follows = "nixpkgs"。
  # 这个包的产物已经推到 cachix `mtul`，drv 是按它自己的 nixpkgs 算出来的；一旦 follow，
  # 你会得到另一个 drv，缓存命中不了，每次 switch 都要在本机重编 Rust 依赖。
  # 代价是多一份 nixpkgs 求值进 store，RPATH 指向它自带的那份 glibc，运行时无差别。

  outputs = { self, nixpkgs, cosmic-desktop-mcp, ... }: {
    # ...
    environment.systemPackages = [ cosmic-desktop-mcp.packages.${pkgs.system}.default ];
  };
}
```

## 自检

自检分两层，给不同的人用：

- **`--doctor` 是人的一次性活**：验证安装、顺手把 EIS 的 portal 授权框点掉。
  它会真截屏、真建 EIS 会话，有副作用，不适合交给 agent 随时调用。
- **`env_info` 工具是 agent 的状态查询**：接了 MCP 之后 agent 可以随时调用，
  无副作用——它用 `eis::peek` 只读已建立的会话，绝不新建，所以不会弹授权框。

接 MCP 前先跑一次 doctor：

```bash
cosmic-desktop-mcp --doctor
```

它依次验证四件事，任何一步失败都会直接指出是哪一步。上面这台机器的真实输出：

```
wayland: Ok("wayland-1")
output #0: 1920x1080 scale=2 name=eDP-1
output #1: 2560x1440 scale=2 name=DP-2
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

### agent 的状态查询：env_info

MCP 会话里 agent 调 `env_info` 得到一份 JSON：

- `outputs`：每块屏的连接器名（`DP-2`）、物理尺寸、以及匹配到的 EIS region
  （逻辑原点、逻辑尺寸、真实 scale）。region 与输出的对应**先按名字对齐**
  （`wl_output.name` == region 的 `mapping_id`），两边都缺名字才退回物理尺寸匹配，
  而且只在唯一匹配时采用——**同型号双屏（物理尺寸相同）无法区分时会直接报错**，
  不再像以前那样静默把 output 1 的点击落到 output 0 上。
- `input.connected`：EIS 会话是否已建立。`false` 时 notes 会说明「第一次 click
  会弹 portal 授权框，需要人点」——这正是 agent 判断「为什么点了没反应」的依据。
- `notes`：坐标约定（工具收的是截图像素，逻辑换算在内部做）。

### 什么时候状态会过期

- 授权可以在系统设置里被 revoke，撤销后下次建立会话又会弹框；
- 插拔屏幕、改缩放、改排列都会改变 region。输出列表每次实时枚举
  （`input.rs` 不缓存），所以拓扑变化后不用重启 server；
- 锁屏时 capture 会静默挂起（见 NIX.md），表现为 screenshot 超时。

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
