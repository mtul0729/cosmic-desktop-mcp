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

- **vendor 目录**（`cargoVendorDir`）：只传 `cargoLock = ./Cargo.lock`。crane 在 **eval 阶段**
  用 `readFile` 读锁文件，从不遍历源码树（见「踩过的坑」第 4 条），所以改源码不会重算 vendor。
- **依赖编译**（`cargoArtifacts`）：`buildPackage` 先跑 `buildDepsOnly`，把
  wayland-client / image / serde 这些编成一个 `target.tar.zst` 单独进 store。目前 49 个
  crate，产物比 sticky-notes 的 830 MiB 小两个数量级。
- **主 crate**：只有 `src/` 变了才重编。

`src` 用 `craneLib.cleanCargoSource ./.`：保留全部 `*.rs`（含 `build.rs`）、`*.toml`
（含 `.cargo/config.toml`）和目录骨架，剔掉 `.jj`、`.git`、`target` 和非 rs/toml 文件
（文档、图片）。本地和 CI 的 drv 哈希因此一致，cachix 能命中。

**不要改回手工白名单。** 曾经写的是
`fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ]`，看着更精确，实际是个陷阱：漏掉
`build.rs` 时 cargo 不会报错，它的语义是「这个 crate 没有构建脚本」，于是沙箱里编出来的
二进制和 devShell 里 `cargo build` 的产物**不是同一个东西，且没有任何告警**；
`.cargo/config.toml` 丢掉的 rustflags 同理。白名单漏掉 `include_bytes!` 的图片会编译失败，
那是好事。**宁可大声失败，不要安静地编错。**

`rust-toolchain.toml` 现在会进 store：crane 的过滤器保留所有 `*.toml`。无害——只有 rustup
读这个文件，而 nix 的 `cargo` 是真实二进制不是 rustup shim（实测：把 channel 写成不存在的
版本，`cargo metadata` 照样跑）。

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

1. **`ext_foreign_toplevel_list_v1` 的 `toplevel` 事件会创建新对象**，`wayland-client` 要求
   显式实现 `Dispatch::event_created_child`，否则运行到那一步直接 panic
   （"Missing event_created_child specialization"）。用 `event_created_child!` 宏补。
2. **COSMIC 的 shm format 是 ABGR8888 / XBGR8888**（`0x34324241` / `0x34324258`），
   不是常见的 ARGB/XRGB。内存字节序是 R,G,B,A，正好可以直接拷进 `RgbaImage`，不用交换。
3. **`src` 里不能有 `target/`**，否则每次构建哈希都变。`cleanCargoSource` 走的是
   `lib.cleanSource`（先剔 target/.git/编辑器临时文件）再叠 cargo 过滤，已经规避。
4. **`vendorCargoDeps` 会把 `cleanSourceWith` 拆掉**。它内部是
   `origSrc = src: if src._isLibCleanSourceWith or false then src.origSrc else src;`，
   也就是说传 `cleanCargoSource ./.` 进去等于传原始目录，那层过滤在 vendor 上下文里
   从未生效。它真正只用两个东西：找 `.cargo/config`、`readFile` 锁文件。
   所以本仓库直接给 `cargoLock = ./Cargo.lock`，连 `src` 都不传。
5. **`buildPackage` 的 args 会被 deps 派生继承**。`cargoArtifacts` 是用同一份 args 派生的
   （`buildDepsOnly (args // memoizedArgs // { installPhase = ...; })`），crane 只覆盖
   `installPhase`，注释里写着 "we intentionally don't run any caller-provided hooks here"，
   但 `postFixup` / `postInstall` 原样透传。deps 的 `$out` 里只有 `target.tar.zst`，
   没有 `bin/`，于是 `wrapProgram` 报 "Cannot wrap ... because it does not exist"。
   **规矩：所有 `postFixup` / `postInstall` 一律用 `.overrideAttrs` 加。**
6. **锁屏时 capture 会静默挂起**。cosmic-greeter 挂着时 `create_session` 一切正常，
   `capture()` 永远不回事件，只能靠 5 秒超时兜底。排查时差点当成 cosmic-comp 的 shm
   路径没实现——其实是屏幕锁着。
