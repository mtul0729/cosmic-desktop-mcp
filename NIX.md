# Nix 打包说明

Rust 项目，crane 打包。结构照 `sticky-notes`，但更简单：**没有 git 依赖**，所以不需要
`outputHashes` 表，也没有 `update-hashes` 这个 app。

## 构建

```bash
nix build
```

产物：`result/bin/cosmic-desktop-mcp`。没有系统库依赖——`wayland-client` 走运行时
dlopen，不需要 `pkg-config` 也不需要 `libwayland` 进 buildInputs。

三段式缓存（crane 默认行为）：

- **vendor 目录**（`cargoVendorDir`）：`craneLib.vendorCargoDeps { src = craneLib.cleanCargoSource ./.; }`。
  `cleanCargoSource` 只保留 `Cargo.toml` / `Cargo.lock` / `*.rs` / `.cargo/config*`，
  改源码不会重算 vendor。
- **依赖编译**（`cargoArtifacts`）：`buildPackage` 先跑 `buildDepsOnly`，把
  wayland-client / image / serde 这些编成一个 `target.tar.zst` 单独进 store。目前 49 个
  crate，产物比 sticky-notes 的 830 MiB 小两个数量级。
- **主 crate**：只有 `src/` 变了才重编。

`src` 用 `lib.fileset.toSource` 白名单，只含 `Cargo.toml`、`Cargo.lock`、`src`。
`.jj`、`.git`、`target`、`rust-toolchain.toml`、文档都不进 store —— 本地和 CI 的 drv
哈希因此一致，cachix 能命中。

`rust-toolchain.toml` 故意不进 fileset：进了沙箱它会让 cargo 去调 rustup，而 nix 的
rustc 就是最终工具链。

## CI

沿用 `helix-flake` / `sticky-notes` 的结构，两个 workflow 共用一个 `setup` 复合 action：

- **`setup`**：`install-nix-action@v31`（带 `access-tokens` 防限流）+ `cachix-action@v17`
  （cache `mtul`，`CACHIX_AUTH_TOKEN` 没配就跳过 push）
- **`build.yml`**：push 到 main / PR / 手动 → `nix flake check --all-systems` + `nix build -L`。
  构建即发布：setup 里的 cachix 把每个构建产物推进 `mtul`，本地 `nix build` 直接 substitute。
- **`update-inputs.yml`**：每天 04:00 UTC 跑 `nix flake update`，构建通过才把 `flake.lock`
  提交回 main。构建是闸门，坏的 input 到不了主干。

`--all-systems` 是因为 flake 同时覆盖 `x86_64-linux` / `aarch64-linux`，不加它会警告
"omitted these incompatible systems"；加上之后对非本机架构只求值不构建。

## 本地验证（不进 nix build）

`cargo check` 比 `nix build` 快，改代码时用：

```bash
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c cargo check
```

`gcc` 是必需的：`image` 的依赖链里有 build script 要调 `cc`，只装 cargo/rustc 会报
`linker 'cc' not found`。

## 踩过的坑

1. **`UI_DEV_CREATE` 是 `_IO` 不是 `_IOW`**。`linux/uinput.h` 里
   `UI_DEV_CREATE _IO(UINPUT_IOCTL_BASE, 1)` 没有方向位也没有 size；按 `_IOW(nr, 0)` 编出来
   是 `0x40005501`，内核直接回 `EINVAL`。正确的是 `0x5501`。
2. **`ext_foreign_toplevel_list_v1` 的 `toplevel` 事件会创建新对象**，`wayland-client` 要求
   显式实现 `Dispatch::event_created_child`，否则运行到那一步直接 panic
   （"Missing event_created_child specialization"）。用 `event_created_child!` 宏补。
3. **COSMIC 的 shm format 是 ABGR8888 / XBGR8888**（`0x34324241` / `0x34324258`），
   不是常见的 ARGB/XRGB。内存字节序是 R,G,B,A，正好可以直接拷进 `RgbaImage`，不用交换。
4. **crane 的 `src` 里不能有 `target/`**，否则每次构建哈希都变。`fileset.toSource` 的白名单
   已经规避了这一点。
5. **锁屏时 capture 会静默挂起**。cosmic-greeter 挂着时 `create_session` 一切正常，
   `capture()` 永远不回事件，只能靠 5 秒超时兜底。排查时差点当成 cosmic-comp 的 shm
   路径没实现——其实是屏幕锁着。
