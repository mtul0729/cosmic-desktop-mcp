# 工具参考

[English](tool-reference.md) | **简体中文**

本页说明 `cosmic-desktop-mcp` 通过 MCP 提供的全部工具。大多数工具返回结构化 JSON 文本；
日志和剪贴板读取返回纯文本，简单操作返回一行状态。指针和窗口工具还会返回足够的信息，
用于核对实际执行结果。

## 通用约定

### 窗口选择器

带 `window` 参数的工具接受：

1. `list_windows` 返回的精确协议 `identifier`（推荐）；
2. 精确匹配的 `app_id`；
3. 精确匹配、忽略大小写的唯一标题子串。

如果名称或标题匹配多个窗口，工具会返回歧义错误，而不会随便选一个。对于多个终端窗口等
情况，请使用 identifier。

### 副作用

- **观察：** `env_info`、`list_windows`、`screenshot`、`screenshot_window`。这些工具不会
  打开 EIS 输入会话。
- **准备：** `prepare_input`。可能弹出 RemoteDesktop 授权对话框。
- **操作：** 指针、键盘、工作区、面板小程序和配置工具。输入操作会在第一次使用时惰性打开
  EIS，因此也可能弹出同一个对话框。

如果客户端提供了 `_meta.progressToken`，服务在等待授权期间每两秒发送一次 MCP 进度通知。
超时、拒绝和 portal 不可用分别报告为 `authorization_timeout`、`authorization_denied` 和
`authorization_unavailable`。

### 坐标

- `screenshot` 生成**输出截图像素**。传给 `click`、`move`、`scroll` 时，必须同时传入相同的
  `output` 索引。
- `screenshot_window` 生成**窗口图片像素**。把它的完整 `coordinates` 对象原样传给
  `click_window`、`move_window`、`scroll_window`。
- **逻辑坐标**只在内部使用。没有工具直接接收逻辑坐标。

不要把窗口原点加到坐标上再调用 `click`。窗口工具会校验窗口身份和截图是否过期，再在内部
完成所有坐标转换。

## 状态与准备

### `env_info`

报告桌面拓扑和输入状态。无参数，无副作用。

返回服务版本、协议版本，以及每个输出的索引、连接器名称、物理尺寸。如果 EIS 会话已经存在，
还会返回逻辑区域和 scale。尚未授权输入时，逻辑区域和 scale 为 `null`，这是正常状态。

适合在第一次点击前、插拔显示器或修改缩放后调用，也适合排查点击为什么没有反应。

### `prepare_input`

打开或复用 EIS 输入会话。无参数。

第一次使用时可能弹出系统 RemoteDesktop 授权对话框并等待人工操作。响应会说明是否新打开了
会话，并列出输入设备和每个输出的 EIS 区域。

输入操作本身会惰性打开会话，所以这个调用是可选的。它的价值在于提前触发并诊断授权流程。

## 截图与窗口

### `screenshot`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `output` | integer | `0` | `env_info` 报告的输出索引 |

截取一个输出，不会打开输入会话。返回 PNG 路径、图片尺寸、输出信息和 `screen_locked`
（logind 无法回答时为 `null`）。图片使用该输出的完整截图分辨率，也就是
`click` / `move` / `scroll` 在相同 `output` 下接受的像素空间。

如果 `screen_locked` 为 `true`，图片是锁屏或黑屏桌面，不是用户当前桌面。锁屏时截图也可能
超时。

### `list_windows`

列出打开的窗口。无参数。

每个窗口包含：

- `identifier`、`title`、`app_id`
- `state`：可能包含 `maximized`、`minimized`、`activated`、`fullscreen`、`sticky`
- `output`：索引、连接器名称和尺寸
- `logical`：合成器报告的、相对于输出位置的逻辑矩形
- `pixels`：同一矩形在该输出截图像素空间中的位置
- `workspaces` 和 `on_active_workspace`

EIS 会话存在前，`pixels` 为 `null`，因为真实缩放比例来自 EIS 区域。COSMIC 未暴露工作区
归属时，`workspaces` 和 `on_active_workspace` 也可能为 `null`；`null` 表示未知，不是
“不在任何工作区”。

几何位置是最后一次报告的位置，不代表当前可见。被遮挡的窗口或位于非当前工作区的窗口仍会
保留几何位置。如果图片内容和点击落点必须一致，请先激活目标窗口。

空闲桌面上，首次几何报告最多可能需要约 600 ms。如果窗口暂时没有几何位置，重新调用
`list_windows`。

### `screenshot_window`

| 参数 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `window` | string | 是 | 窗口选择器 |

截取单个窗口，不会打开输入会话。响应包括：

- `path` 和 `image`
- `capture.source`：`window` 表示直接窗口捕捉，`output_crop` 表示输出裁剪
- `capture.coordinate_safe`
- `window`、`output`、`logical`、`pixels`、`workspaces`、`on_active_workspace`
- `coordinates`：原样传给窗口指针工具

优先使用直接窗口捕捉。即使目标被其他窗口遮挡，直接捕捉也可能显示目标本身；但 COSMIC
可能省略服务端标题栏。如果直接捕捉没有覆盖完整的逻辑窗口，工具不会猜测。

如果 EIS 会话已经打开，它可以回退到输出裁剪。如果会话尚未打开且没有安全的直接画面，工具会
返回 `status: "needs_input_session"` 的结构化错误。先调用 `prepare_input`，让人工批准，
然后重新截取窗口。

### `window_action`

| 参数 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `window` | string | 是 | 窗口选择器 |
| `action` | string | 是 | `activate`、`close`、`maximize`、`unmaximize`、`minimize`、`unminimize`、`fullscreen` 或 `unfullscreen` |

向合成器发送请求，但不等待也不保证目标状态已经生效。请重新调用 `list_windows` 确认结果。

**输入文字前先调用 `activate`。** 在 COSMIC 上，EIS 点击不会移动键盘焦点；如果不激活，
`type_text` 和 `key` 会发送到之前获得焦点的窗口。

## 指针操作

指针工具会返回实际使用的输出和逻辑坐标、坐标下方的窗口命中测试结果，以及锁屏状态。操作
看起来没有生效时，优先查看 `hit.window`、`hit.ambiguous`、`on_active_workspace` 和
`other_output_match`。

### `click`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `x`、`y` | integer | 必填 | `screenshot` 中的像素坐标 |
| `output` | integer | `0` | 这些坐标所属的输出索引 |
| `button` | string | `left` | `left`、`right` 或 `middle` |
| `count` | integer | `1` | `1` 为单击，`2` 为双击 |

输出索引绝不会根据 `x`/`y` 推断。来自某块屏幕截图的坐标，必须传入那块屏幕的输出索引。
如果点来自 `screenshot_window`，改用 `click_window`。

### `move`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `x`、`y` | integer | 必填 | `screenshot` 中的像素坐标 |
| `output` | integer | `0` | 这些坐标所属的输出索引 |

移动指针但不点击。适合测试 hover 状态，也可以作为只读探测。

### `scroll`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `x`、`y` | integer | 必填 | `screenshot` 中的像素坐标 |
| `output` | integer | `0` | 这些坐标所属的输出索引 |
| `dy` | integer | `0` | 垂直滚轮格数；正数向上 |
| `dx` | integer | `0` | 水平滚轮格数；正数向右 |

正 `dy` 遵循内核 `REL_WHEEL` 约定，表示显示更早的内容；负 `dy` 向下滚动。在列表底部测试
向下滚动不能证明滚动功能有效，检查时请从靠近顶部的位置开始。

### `click_window`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `window` | string | 必填 | 窗口选择器 |
| `x`、`y` | integer | 必填 | 该窗口截图中的像素坐标 |
| `coordinates` | object | 必填 | `screenshot_window` 返回的完整 `coordinates` 对象 |
| `button` | string | `left` | `left`、`right` 或 `middle` |
| `count` | integer | `1` | `1` 为单击，`2` 为双击 |

工具会验证 `coordinates` 属于所选窗口，并检查截图是否过期，然后把图片坐标转换为输出像素和
逻辑桌面坐标。不要重建或修改 `coordinates` 对象。

### `move_window`

参数与 `click_window` 相同，但没有 `button` 和 `count`。把指针移动到窗口图片中的指定点。

### `scroll_window`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `window` | string | 必填 | 窗口选择器 |
| `x`、`y` | integer | 必填 | 该窗口截图中的像素坐标 |
| `coordinates` | object | 必填 | `screenshot_window` 返回的完整 `coordinates` 对象 |
| `dy` | integer | `0` | 垂直滚轮格数；正数向上 |
| `dx` | integer | `0` | 水平滚轮格数；正数向右 |

在窗口图片中的指定点滚动。截图新鲜度和坐标校验与 `click_window` 相同。

## 键盘与剪贴板

### `type_text`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `text` | string | 必填 | 要输入的文字 |
| `mode` | string | `paste` | `paste` 使用剪贴板 + Ctrl+V；`keys` 发送键码 |

`paste` 是可靠模式，支持非 ASCII 文字，但会覆盖当前剪贴板，目标也必须接受 Ctrl+V。

`keys` 合成键码，只接受可打印 ASCII、空格、制表符和换行。它适合输入法未启用，或确实需要
真实按键事件的场景。fcitx5 处于中文模式时，ASCII 可能被当作拼音并转换成候选词。

文字会发送到**当前获得焦点**的窗口。请先激活目标窗口。

### `key`

| 参数 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `combo` | string | 是 | 组合键，例如 `ctrl+s`、`alt+tab` 或 `Return` |

向当前焦点窗口发送一个组合键。键表假设使用 US 布局；其他布局输入文字请使用 `type_text`。

### `clipboard_write`

| 参数 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `text` | string | 是 | 写入剪贴板的文字 |

写入文本但不立即粘贴。Wayland selection 归 MCP 服务进程所有，服务退出后内容会消失。

### `clipboard_read`

以文本形式读取当前剪贴板。无参数。

## 工作区

### `workspaces`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `activate` | string | 省略 | 工作区名称（`"2"`）或网格位置（`"2,0"`） |
| `group` | integer | 省略 | 输出对应的分组索引，来自列表结果 |

不传 `activate` 时，列出工作区分组、名称、网格位置，以及 active/urgent/hidden 状态。
COSMIC 为每个输出维护一个工作区分组，因此不同分组通常都有名为 `"1"`、`"2"` 的工作区。
同一个名称存在于多个分组时，必须显式传 `group`，工具不会猜测。

## 面板小程序与 COSMIC 配置

### `applets`

从 `/proc` 列出正在运行的面板小程序：进程名、PID、父进程、内存和运行时长。每个输出通常
有一个面板，因此每个小程序可能对应多个副本。无参数。

### `restart_applet`

| 参数 | 类型 | 必填 | 含义 |
|---|---|---|---|
| `name` | string | 是 | 完整小程序名或后缀，例如 `cosmic-applet-time` 或 `time` |

短暂清空并重写面板的 `plugins_*` 配置，让 COSMIC 重新启动小程序。这是**面板区域刷新**，
不是只重启一个进程：同一区域的其他小程序也可能以新 PID 重启。

原始配置文本会恢复，备份保存在
`~/.local/state/cosmic-desktop-mcp/panel-backups/`。不要用 `kill`/`SIGTERM`；COSMIC
不一定会重新拉起被杀掉的小程序。

小程序日志有意不做成 MCP 工具。[操作 skill](../skill/cosmic-desktop-ops/SKILL.md)给出了
配合 `applets` 返回的进程名使用的 `journalctl` 配方。

### `config_read`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `app` | string | 省略 | COSMIC app ID |
| `key` | string | 省略 | 设置项名称 |

- 无参数：列出已知 app ID。
- 只传 `app`：列出该 app 的所有键和值。
- 传 `app` 和 `key`：读取一个值及其文件路径。

COSMIC 配置位于 `~/.config/cosmic/<app-id>/v1/<key>`，每个文件保存一个原始 RON 值。

### `config_write`

| 参数 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `app` | string | 必填 | COSMIC app ID |
| `key` | string | 必填 | 设置项名称 |
| `value` | string | 省略 | 原始 RON 文本；省略或传 `null` 表示删除 |

值会原样写入，因此字符串保留引号，枚举保留 variant 名称。响应会返回旧值，请保留用于回滚。
大多数 COSMIC 消费者会监听配置目录，但只在启动时读取配置的小程序可能还需要
`restart_applet`。

## 相关文档

- [快速开始](../README.zh-CN.md)
- [适用范围与设计原则](design.zh-CN.md)
- [English tool reference](tool-reference.md)
- [工程笔记](engineering-notes.md)
