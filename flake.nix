{
  description = "Development environment for zprs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    git-hooks.url = "github:cachix/git-hooks.nix";
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    zpg = {
      url = "github:asciimoth/zpg";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.git-hooks.follows = "git-hooks";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      git-hooks,
      rust-overlay,
      zpg,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      toolchainsFor = pkgs: {
        stable = pkgs.rust-bin.stable.latest.minimal.override {
          extensions = [
            "clippy"
            "rustfmt"
            "rust-src"
          ];
        };
        # Keep this tied to the minimum version declared by the crate.
        minimum =
          pkgs.rust-bin.stable."${(builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.rust-version}.0".minimal.override
            {
              extensions = [
                "clippy"
                "rustfmt"
                "rust-src"
              ];
            };
      };
    in
    {
      checks = eachSystem (
        system:
        let
          pkgs = pkgsFor system;
          toolchains = toolchainsFor pkgs;
          cargoDeps = pkgs.rustPlatform.importCargoLock {
            lockFile = ./Cargo.lock;
          };
          cargoConfig = pkgs.writeText "zprs-cargo-config.toml" ''
            [source.crates-io]
            replace-with = "vendored-sources"
            [source.vendored-sources]
            directory = "${cargoDeps}"
          '';
          rustHook = pkgs.writeShellApplication {
            name = "zprs-rust-hook";
            runtimeInputs = [
              toolchains.stable
              pkgs.stdenv.cc
              pkgs.just
              zpg.packages.${system}.zpg
            ];
            text = ''
              zprs_cargo_home=$(mktemp -d)
              trap 'rm -rf "$zprs_cargo_home"' EXIT
              cp ${cargoConfig} "$zprs_cargo_home/config.toml"
              export CARGO_HOME="$zprs_cargo_home"
              export ZPRS_REQUIRE_ZPG=1
              "$@"
            '';
          };
          checkRust =
            toolchain:
            (pkgs.makeRustPlatform {
              cargo = toolchain;
              rustc = toolchain;
            }).buildRustPackage
              {
                pname = "zprs-check";
                version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
                cargoLock.lockFile = ./Cargo.lock;
                nativeBuildInputs = [
                  pkgs.just
                  zpg.packages.${system}.zpg
                ];
                src = pkgs.lib.cleanSource ./.;
                buildPhase = ''
                  runHook preBuild
                  just build
                  runHook postBuild
                '';
                checkPhase = ''
                  runHook preCheck
                  just lint-rust
                  ZPRS_REQUIRE_ZPG=1 just test
                  runHook postCheck
                '';
                installPhase = ''touch "$out"'';
              };
        in
        {
          rust = checkRust toolchains.stable;
          rust175 = checkRust toolchains.minimum;
          pre-commit-check = git-hooks.lib.${system}.run {
            src = ./.;
            default_stages = [ "pre-commit" ];
            hooks = {
              check-added-large-files.enable = true;
              check-case-conflicts.enable = true;
              check-merge-conflicts.enable = true;
              check-toml.enable = true;
              check-yaml.enable = true;
              commitizen.enable = true;
              deadnix.enable = true;
              end-of-file-fixer.enable = true;
              markdownlint.enable = true;
              nixfmt.enable = true;
              statix.enable = true;
              trim-trailing-whitespace.enable = true;
              typos.enable = true;
              just-fmt = {
                enable = true;
                name = "Justfile format";
                entry = "${pkgs.just}/bin/just --fmt --check";
                files = "^justfile$";
                pass_filenames = false;
              };
              rust-fmt = {
                enable = true;
                name = "Rust format";
                entry = "${rustHook}/bin/zprs-rust-hook cargo fmt --all -- --check";
                files = "(\\.rs$|^Cargo\\.toml$)";
                pass_filenames = false;
              };
              rust-check = {
                enable = true;
                name = "Rust build, Clippy, and tests";
                entry = "${rustHook}/bin/zprs-rust-hook just check-rust";
                files = "(\\.rs$|^Cargo\\.(toml|lock)$|^justfile$)";
                pass_filenames = false;
                require_serial = true;
              };
            };
          };
        }
      );

      formatter = eachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt);

      devShells = eachSystem (
        system:
        let
          pkgs = pkgsFor system;
          toolchains = toolchainsFor pkgs;
          hook = self.checks.${system}.pre-commit-check;
          shellFor =
            toolchain:
            pkgs.mkShell {
              packages = [
                toolchain
                pkgs.just
                pkgs.rust-analyzer
                zpg.packages.${system}.zpg
              ]
              ++ hook.enabledPackages;
              RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
              inherit (hook) shellHook;
            };
        in
        {
          default = shellFor toolchains.stable;
          rust175 = shellFor toolchains.minimum;
        }
      );
    };
}
