# Packaging notes

A Rust project, packaged with crane. The layout follows `sticky-notes` but is simpler:
**no git dependencies**, so there is no `outputHashes` table and no `update-hashes` app.

## Building

```bash
nix build
```

The result is `result/bin/cosmic-desktop-mcp`. There are no system library dependencies —
`wayland-client` dlopens at runtime, so neither `pkg-config` nor `libwayland` needs to be in
`buildInputs`.

Three-stage caching (crane's default):

- **Vendor directory** (`cargoVendorDir`): only `cargoLock = ./Cargo.lock` is passed. crane
  `readFile`s the lock at **evaluation time** and never walks the source tree (pitfall 4
  below), so changing sources does not recompute the vendor derivation.
- **Dependency build** (`cargoArtifacts`): `buildPackage` runs `buildDepsOnly` first,
  compiling wayland-client / image / serde and friends into a single `target.tar.zst` in the
  store. That is 99 crates today; the artifact is two orders of magnitude smaller than
  sticky-notes' 830 MiB.
- **The main crate**: rebuilt only when `src/` changes.

`src` uses `craneLib.cleanCargoSource ./.`: it keeps every `*.rs` (including `build.rs`),
every `*.toml` (including `.cargo/config.toml`) and the directory skeleton, and drops
`.jj`, `.git`, `target` and non-rs/toml files (docs, images). Local and CI derivations
therefore hash identically, so cachix hits.

**Do not switch back to a hand-written whitelist.** The earlier
`fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ]` looked more precise and was a trap:
when `build.rs` is missing cargo does not complain — its semantics are "this crate has no
build script" — so the sandbox binary and the devShell `cargo build` output are **not the
same artifact, with no warning at all**. `.cargo/config.toml` loses its rustflags the same
way. A whitelist that misses an `include_bytes!` image fails loudly, which is the good
outcome. **Fail loudly rather than build the wrong thing quietly.**

`rust-toolchain.toml` does end up in the store now: crane's filter keeps every `*.toml`.
That is harmless — only rustup reads the file, and nix's `cargo` is a real binary, not a
rustup shim (verified: point the channel at a nonexistent version and `cargo metadata`
still runs).

## CI

Follows the `helix-flake` / `sticky-notes` structure, with both workflows sharing one
`setup` composite action:

- **`setup`**: `install-nix-action@v31` (with `access-tokens` against rate limits) +
  `cachix-action@v17` (cache `mtul`; pushes are skipped when `CACHIX_AUTH_TOKEN` is unset)
- **`build.yml`**: push to main / PR / manual → `nix flake check --all-systems` +
  `nix build -L`. Building is publishing: the cachix step in `setup` pushes every result to
  `mtul`, so a local `nix build` substitutes directly.
- **`update-inputs.yml`**: runs `nix flake update` daily at 04:00 UTC and commits
  `flake.lock` back to main only if the build passes. The build is the gate; a broken input
  never reaches the branch.

`--all-systems` is there because the flake covers both `x86_64-linux` and `aarch64-linux`;
without it nix warns "omitted these incompatible systems". With it, foreign architectures
are evaluated but not built.

## Local verification (not through `nix build`)

`cargo check` is faster than `nix build` while editing:

```bash
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc -c cargo check
```

`gcc` is required: something in `image`'s dependency chain has a build script that calls
`cc`, and cargo/rustc alone fail with `linker 'cc' not found`.

## Pitfalls

1. **`ext_foreign_toplevel_list_v1`'s `toplevel` event creates a new object**, and
   `wayland-client` requires an explicit `Dispatch::event_created_child` implementation;
   without it the run panics at that point ("Missing event_created_child specialization").
   The `event_created_child!` macro covers it.
2. **COSMIC's shm formats are ABGR8888 / XBGR8888** (`0x34324241` / `0x34324258`), not the
   usual ARGB/XRGB. The bytes in memory are R,G,B,A, so they go straight into `RgbaImage`
   with no swizzle.
3. **`src` must not contain `target/`**, or the hash changes on every build.
   `cleanCargoSource` goes through `lib.cleanSource` (which strips target/.git/editor
   temporaries) before applying the cargo filter, so this is already handled.
4. **`vendorCargoDeps` strips `cleanSourceWith`.** Internally it does
   `origSrc = src: if src._isLibCleanSourceWith or false then src.origSrc else src;`, so
   passing `cleanCargoSource ./.` hands it the raw directory — the filter never applied in
   the vendor context. It really only uses two things: it looks for `.cargo/config` and it
   `readFile`s the lock. This repository therefore passes `cargoLock = ./Cargo.lock` and no
   `src` at all.
5. **`buildPackage`'s args are inherited by the deps derivation.** `cargoArtifacts` is
   derived from the same args (`buildDepsOnly (args // memoizedArgs // { installPhase =
   ...; })`); crane only overrides `installPhase` — its comment says "we intentionally don't
   run any caller-provided hooks here" — but `postFixup` / `postInstall` pass straight
   through. The deps `$out` holds only `target.tar.zst` and no `bin/`, so `wrapProgram`
   fails with "Cannot wrap ... because it does not exist".
   **Rule: always add `postFixup` / `postInstall` through `.overrideAttrs`.**
6. **Capture silently hangs while the screen is locked.** With cosmic-greeter running,
   `create_session` succeeds and `capture()` never returns an event; only the 5-second
   timeout catches it. While debugging this I nearly blamed an unimplemented shm path in
   cosmic-comp — the screen was simply locked.
