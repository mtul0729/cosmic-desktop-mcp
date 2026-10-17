# Nix packaging for cosmic-desktop-mcp (MIT).
#
# Built with crane, same split as sticky-notes: the vendored crate tree
# (`cargoVendorDir`) and the compiled dependencies (`cargoArtifacts`) are
# separate derivations, so editing src/ only rebuilds this crate — which is
# what matters while the tool set is still moving. There are no git
# dependencies, so no `outputHashes` table is needed.
#
# nix build -> result/bin/cosmic-desktop-mcp
{
  description = "MCP server for driving a COSMIC/Wayland desktop";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
    }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        craneLib = crane.mkLib pkgs;

        # Only what cargo reads. `.jj`, `.git`, `target/` and docs stay out, so
        # the local drv hash matches CI's clean checkout and cachix hits.
        # `rust-toolchain.toml` is deliberately excluded: inside the sandbox it
        # would make cargo try to call rustup.
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
          ];
        };

        # Vendoring needs nothing but the lockfile: crane readFile()s
        # `cargoLock` at eval time and never walks the source tree, so editing
        # src/ cannot invalidate this derivation. Handing it `src` instead would
        # only make it hunt for .cargo/config, which this repo does not have.
        cargoVendorDir = craneLib.vendorCargoDeps {
          cargoLock = ./Cargo.lock;
        };

        package = (craneLib.buildPackage {
          pname = "cosmic-desktop-mcp";
          version = "0.1.0";

          inherit src cargoVendorDir;

          # Nothing to test yet; `--doctor` covers the runtime side and needs a
          # live compositor, which the build sandbox does not have.
          doCheck = false;

          # Clipboard support shells out to wl-copy/wl-paste rather than
          # reimplementing ext-data-control (see src/clipboard.rs).
          nativeBuildInputs = [ pkgs.makeWrapper ];

          separateDebugInfo = true;

          meta = {
            description = "MCP server for driving a COSMIC/Wayland desktop";
            homepage = "https://github.com/mtul0729/cosmic-desktop-mcp";
            license = lib.licenses.mit;
            platforms = lib.platforms.linux;
            mainProgram = "cosmic-desktop-mcp";
          };
        }).overrideAttrs (prev: {
          # `buildPackage` derives the deps-only (`cargoArtifacts`) derivation
          # from these same arguments, and crane only overrides its
          # `installPhase` — any `postFixup` here lands in that derivation too,
          # where `$out/bin` does not exist and `wrapProgram` aborts the build
          # ("Cannot wrap ... because it does not exist"). Wrapping after the
          # fact keeps it on the final package only.
          postFixup = (prev.postFixup or "") + ''
            wrapProgram $out/bin/cosmic-desktop-mcp \
              --prefix PATH : ${lib.makeBinPath [ pkgs.wl-clipboard ]}
          '';
        });
      in
      {
        packages.default = package;

        devShells.default = pkgs.mkShell {
          inputsFrom = [ package ];
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rustfmt
            pkgs.clippy
            # Runtime dependency of the clipboard tools, so `cargo run` behaves
            # like the packaged binary.
            pkgs.wl-clipboard
          ];
        };
      }
    );
}
