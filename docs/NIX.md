# Bkpr native runtime

The `bkpr` branch keeps Bkpr's native runtime on Zellij `v0.45.1`. `main`
tracks upstream unchanged. Feature branches retain the individual changes
merged into `bkpr`. The upstream development instructions still apply to `main`.

## Packages

```sh
nix build --accept-flake-config .#zellij
./result/bin/zellij --version
```

The flake supports `x86_64-linux`, `aarch64-linux`, and `aarch64-darwin`.
`packages.<system>.default` and `.zellij` expose the packaged runtime.
`.zellij-unwrapped` exposes the executable derivation, including embedded assets
and generated shell completions. The Nix package version is `0.45.1-bkpr.2`;
the integrated version feature supplies the binary's version string.

Packaging reuses the pinned nixpkgs recipe's native libraries, installation,
and wrapper. Source comes from this checkout, without an upstream source fetch
or patch queue. Builds and regression tests use `--no-default-features`:
no web server, no plugin build from `target`, and system curl/OpenSSL. The
bundled plugin assets in `zellij-utils/assets/plugins` remain embedded.

The package runs Bkpr's program-status, input-parser, popup, geometry, resize,
render-message, tab-switch, and plugin regression checks. These run in the same
release profile and target as the package build. Render-message tests run
serially. The inherited install checks check the binary version and, on glibc,
system curl linkage.

## Incremental development

```sh
nix develop --accept-flake-config
cargo build --locked --offline --no-default-features
cargo test --locked --offline --no-default-features -p zellij-server --lib program_status
./target/debug/zellij --version
```

Alternatively, run `direnv allow` to load `.envrc`. The shell provides the
checkout's pinned Rust toolchain, native compiler, bindgen, pkg-config, curl,
OpenSSL, CMake, and Perl. It doesn't build the release runtime. It prepares the
Cargo vendor directory and puts Cargo's generated configuration under
`target/nix-cargo`; the rest of the incremental artifacts stay in `target`.
Global Cargo settings and credentials aren't changed.

`packages.<system>.cargo-vendor` vendors this fork's `Cargo.lock`, not the
nixpkgs Zellij lockfile. `ZELLIJ_CARGO_VENDOR` points to it in the shell. For
another incremental Cargo environment:

```sh
vendor=$(nix build --accept-flake-config --no-link --print-out-paths .#cargo-vendor)
cargo --config "$vendor/.cargo/config.toml" \
  --config "source.vendored-sources.directory=\"$vendor\"" \
  build --locked --offline --no-default-features
```

After changing `Cargo.lock`, reload the shell to regenerate its source
replacement. New git dependencies also need their fixed-output hashes in the
flake's `importCargoLock` call. Missing hashes fail instead of falling back to
upstream dependencies.

## Pins and cache

The fork owns its `flake.lock`. Consumers should use
`github:calops/bkpr-zellij/bkpr` and keep the fork's inputs intact, without
`follows` overrides. Bkpr's release CI builds and caches this pinned package
as part of its dependency closure at `https://bkpr.cachix.org`.

To update a dependency, change its revision in `flake.nix`, then run
`nix flake lock`. Keep the nixpkgs recipe compatible with this fork's release
base when updating it.

The fork has no Bkpr build or cache workflow. Release builds and cache uploads
belong to the Bkpr repository; no cache credentials are required in this fork.
