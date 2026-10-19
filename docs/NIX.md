# Bkpr native runtime

The `main` branch integrates Bkpr's native changes on Zellij `v0.45.1`.
Retained `feature/*` branches hold the individual changes and submit PRs into
`main`. Stable upstream releases arrive through human-reviewed update PRs;
see [fork maintenance](RELEASE.md#maintaining-the-bkpr-fork).

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
render-message, tab-switch, plugin, and terminal-palette regression checks.
Palette checks cover actual-color parsing, batched and duplicate updates,
client-specific snapshots and pending-event replay, pasted and foreign DCS
preservation, permission gating, and backward-compatible IPC. These run in the
same release profile and target as the package build. Render-message tests run
serially. The inherited install checks check the binary version and, on glibc,
system curl linkage.

## Terminal palette metadata

Plugins with `ReadApplicationState` permission can subscribe to `CustomMessage`
and receive the host-originated `bkpr_terminal_colors` snapshot:

```json
{"truecolor":false,"foreground":null,"background":null,"ansi":[null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null]}
```

Foreground/background come from the attached terminal's OSC 10/11 responses;
the sixteen ANSI entries come from OSC 4. Unknown values remain `null`.
Native truecolor requires a positive XTGETTCAP `RGB` or `Tc` response from that
client's terminal, including across SSH. RGB theme settings, OSC color values,
and server-side terminal environment variables do not establish that capability.
The browser client uses its known xterm.js renderer and the resolved colors
sent to that renderer.

The host queries once at attachment and refreshes palette colors on terminal
theme notifications, without polling or per-render queries. Replies from one
input batch produce one metadata update; unchanged snapshots are suppressed.
Each client receives its own snapshot, including during plugin loading and
permission replay. Subscription and permission-grant bootstrap new helpers;
detach discards the client's cached palette. Worker messages cannot impersonate
the reserved event name. Bracketed-paste contents and unrelated DCS input remain
literal input, not capability advertisements.

Client/server transport adds optional metadata to the existing `ColorRegisters`
protobuf message, rather than introducing a new message kind. Older servers
ignore that extension and continue processing the legacy palette registers.
Older clients omit it, leaving unknown capability/color data unset. Consumers
must retain terminal-default foreground/background and native ANSI fallbacks;
they must not substitute a guessed RGB palette.

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
`github:calops/bkpr-zellij/main` and keep the fork's inputs intact, without
`follows` overrides. Bkpr's release CI builds and caches this pinned package
as part of its dependency closure at `https://bkpr.cachix.org`.

Feature branches require a PR into protected `main`, human validation, and an
explicit human-performed merge commit. Do not auto-merge, squash, rebase-merge,
or push directly to `main`. Update a consumer's fork pin only after that manual
merge; a local feature build is for review and isolated verification, not a
replacement for the merge gate.

To update a dependency, change its revision in `flake.nix`, then run
`nix flake lock`. Keep the nixpkgs recipe compatible with this fork's release
base when updating it.

The fork builds and runs packaged regression checks only for upstream-update
PRs. It does not publish releases or upload caches. Release builds and cache
uploads belong to the Bkpr repository; no cache credentials are required here.
