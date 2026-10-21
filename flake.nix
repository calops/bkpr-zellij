{
  description = "Bkpr's native Zellij runtime";

  nixConfig = {
    extra-substituters = [ "https://bkpr.cachix.org" ];
    extra-trusted-public-keys = [ "bkpr.cachix.org-1:uAJ+wudjeXgYbimGGreY8LgWfk0j9JaxNBjDxQ0h18w=" ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/b1b875982b17dabde9b4a37f3e229e74913e6db3";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/f91010a5c1b6125b715eb78a84fb5dd8fb74a2b0";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatformFor =
        pkgs:
        pkgs.makeRustPlatform {
          cargo = toolchainFor pkgs;
          rustc = toolchainFor pkgs;
        };
      vendorFor =
        pkgs:
        (rustPlatformFor pkgs).importCargoLock {
          lockFile = ./Cargo.lock;
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          cargoVendor = vendorFor pkgs;
          unwrapped =
            (pkgs.zellij-unwrapped.override {
              rustPlatform = rustPlatformFor pkgs;
              withWebServer = false;
            }).overrideAttrs
              (old: {
                version = "0.45.1-bkpr.3";
                src = pkgs.lib.cleanSourceWith {
                  name = "zellij-source";
                  src = ./.;
                  filter =
                    path: type:
                    !(builtins.elem (baseNameOf path) [
                      ".git"
                      ".direnv"
                      ".github"
                      "target"
                      "result"
                      "flake.nix"
                      "flake.lock"
                      ".envrc"
                    ])
                    && pkgs.lib.cleanSourceFilter path type;
                };
                # Never inherit nixpkgs' upstream dependency snapshot or source patches.
                cargoDeps = cargoVendor;
                cargoHash = null;
                patches = [ ];
                postPatch = "";
                cargoBuildNoDefaultFeatures = true;
                cargoCheckNoDefaultFeatures = true;
                nativeBuildInputs = old.nativeBuildInputs ++ [ (rustPlatformFor pkgs).bindgenHook ];
                doCheck = true;
                cargoTestFlags = [
                  "--package"
                  "zellij-server"
                  "--lib"
                  "program_status"
                ];
                postCheck = ''
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-utils --lib clipboard
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib clipboard
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib shared_sidebar_ -- --test-threads=1
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-client --lib stdin_ansi_parser
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-client --lib stdin_handler
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib pinned_
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib passive_popup
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib viewport_geometry_regressions
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib test_ctrl_drag_resizes
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib render_messages_tests -- --test-threads=1
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib screen_tab_switch_
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib screen_plugin_
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib terminal_colors
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib terminal_palette_
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib terminal_reply_batch
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-server --lib cached_event_tests
                  cargo test --offline --locked --release --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} --no-default-features --package zellij-utils --lib terminal_palette_extension_remains_a_legacy_color_register_message
                '';
                passthru = { };
                meta = old.meta // {
                  homepage = "https://github.com/calops/bkpr-zellij";
                  changelog = "https://github.com/calops/bkpr-zellij/blob/main/CHANGELOG.md";
                };
              });
          zellij = pkgs.zellij.override { zellij-unwrapped = unwrapped; };
        in
        {
          default = zellij;
          inherit zellij;
          zellij-unwrapped = unwrapped;
          cargo-vendor = cargoVendor;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          cargoVendor = vendorFor pkgs;
        in
        {
          default = pkgs.mkShell {
            packages = [
              (toolchainFor pkgs)
              pkgs.git
              pkgs.pkg-config
              pkgs.cmake
              pkgs.perl
              (pkgs.lib.getDev pkgs.curl)
              (rustPlatformFor pkgs).bindgenHook
            ];
            buildInputs = [
              pkgs.curl
              pkgs.openssl
            ];
            OPENSSL_NO_VENDOR = 1;
            ZELLIJ_CARGO_VENDOR = cargoVendor;
            shellHook = ''
              export CARGO_HOME="$PWD/target/nix-cargo"
              mkdir -p "$CARGO_HOME"
              substitute ${cargoVendor}/.cargo/config.toml "$CARGO_HOME/config.toml" \
                --replace-fail '"cargo-vendor-dir"' '"${cargoVendor}"'
            '';
          };
        }
      );
    };
}
