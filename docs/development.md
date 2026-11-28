# Development

## Requirements

- macOS on Apple silicon to run devices (versions per feature: [Support matrix](#support-matrix)): Xcode (`xcode-select -p`, or `DEVELOPER_DIR`) with at
  least one simulator runtime for iOS; for Android, an SDK system image (`emu -provision` fetches
  one, no Java needed).
  Xcode 26 and 27 both work, and several can be installed side by side: `DEVELOPER_DIR` picks one
  per run. CoreSimulator is the system's (`/Library/Developer/PrivateFrameworks`) whichever Xcode
  is selected; SimulatorKit is the selected Xcode's, in `Contents/SharedFrameworks` from Xcode 27
  and `Contents/Developer/Library/PrivateFrameworks` before it (tried in that order). Xcode 27.2's
  SimulatorKit keeps every HID builder, the HID client class and selectors simee uses, and builds
  byte-identical messages to Xcode 26.6's apart from timestamps.
- Rust 1.99.1 (pinned in `rust-toolchain.toml`).
- On Linux the shared core, the API, the CLI and the desktop window build and test (CI runs them
  there); there are no devices to run, since both runtimes need macOS.

## Support matrix

`simee doctor` (`--json` for the same as JSON) checks this host against the table below. It
prints the chip, the macOS and Xcode versions, whether Hypervisor.framework is available
(`kern.hv_support`) and whether `emu` is signed with the hypervisor entitlement, the host's thermal
and power state (the `host` method), and which features are on or off and why. `emu` checks the
Android rows when it starts: below the Android floor it refuses to boot and names the floor, and it
prints a line for each optional feature that is off. The table is `sim_core::support::FLOORS`; a
test fails if a row here goes missing.

**Chips.** Apple silicon, M1 and later, is supported. Intel Macs are not yet: iOS on Intel is
planned in [#241](https://github.com/kaeawc/simee/issues/241) and Android on Intel (x86_64 images)
in [#242](https://github.com/kaeawc/simee/issues/242). Every macOS release that runs on Apple
silicon runs on M1, so a feature's floor is the same on every Apple silicon chip.

| Feature | Id | macOS | What sets the floor | Below it |
| --- | --- | --- | --- | --- |
| iOS simulator devices | `ios` | Xcode's own floor | Xcode with a simulator runtime | no iOS devices |
| iOS audio capture and mute | `audio_capture` | 14.2 | the Core Audio process tap, `AudioHardwareCreateProcessTap` | `audio_capture` and `audio_mute` fail with "needs macOS 14.2" |
| Android emulator (`emu`) | `android` | 15.0 | the in-kernel GICv3, `hv_gic_create` (emu has no userspace GIC) | `emu` refuses to boot with "needs macOS 15" |
| 4 KiB guest memory granule | `granule_4k` | 26.0 | `hv_vm_config_set_ipa_granule` | guest memory is mapped in 16 KiB pages, so host-visible GPU memory the guest maps at 4 KiB offsets can't be; `emu` says which granule it got |

Where each floor comes from:

- `AudioHardwareCreateProcessTap` is `API_AVAILABLE(macos(14.2))` in the macOS 26.5 SDK's
  `CoreAudio/AudioHardwareTapping.h`, and Apple's reference lists macOS 14.2
  ([AudioHardwareCreateProcessTap](https://developer.apple.com/documentation/coreaudio/audiohardwarecreateprocesstap(_:_:))).
  The audio helper (`crates/sim-audio/native/simaudio.swift`) marks its tap `@available(macOS 14.2, *)`
  and answers an error naming the floor before it; `sim-audio` checks the floor before it builds the
  helper.
- Every `hv_gic_*` function emu calls is `API_AVAILABLE(macos(15.0))` in the SDK's
  `Hypervisor/hv_gic*.h`, and Apple's reference lists macOS 15.0
  ([hv_gic_create](https://developer.apple.com/documentation/hypervisor/hv_gic_create(_:))).
- `hv_vm_config_set_ipa_granule` and `HV_IPA_GRANULE_4KB` are `API_AVAILABLE(macos(26.0))` in
  `Hypervisor/hv_vm_config.h`, and Apple's reference lists macOS 26.0
  ([hv_vm_config_set_ipa_granule](https://developer.apple.com/documentation/hypervisor/hv_vm_config_set_ipa_granule(_:_:))).
- The VM configuration (`hv_vm_config_create`, the IPA size) is macOS 13.0
  ([hv_vm_config_create](https://developer.apple.com/documentation/hypervisor/hv_vm_config_create())),
  below the Android floor.
- emu-hvf links only the Hypervisor.framework functions every Apple silicon macOS has and looks the
  newer ones up at run time (`crates/emu-hvf/src/ffi.rs`): a linked function the framework lacks
  stops dyld from loading the binary, so emu would exit before `main` on an older macOS instead of
  naming what it needs. emu also links CoreAudio weakly (`apps/emu/build.rs`), because cpal imports
  the macOS 14.2 process tap functions; below macOS 15 emu exits at startup, before any audio code.
  `scripts/check-imports.sh` (run by `scripts/check.sh` on macOS) fails if a release `emu` or
  `simee` strongly imports a symbol newer than macOS 11.0.

**Xcode.** simee is tested with Xcode 26 and 27 (the SimulatorKit path change between them is
under Requirements); `simee doctor` marks an older Xcode "unverified". Apple's
[Xcode system requirements](https://developer.apple.com/xcode/system-requirements/): Xcode 26.0 to
26.3 need macOS 15.6, Xcode 26.4 and later need macOS 26.2, and Xcode 27 needs macOS 26.6. So the
tested iOS floor is macOS 15.6 with Xcode 26.3 or earlier.

**Unverified.** Every floor above is read from the SDK headers and Apple's documentation, and is
exercised only on macOS 26 (M3 Max). No host below a floor has run simee: the macOS 14 and 15
behaviour (the audio helper's error, emu's refusal, the 16 KiB granule) is the code's, not yet
observed; nor has M1 or M2 hardware run it.


Build and check commands, the working rules and the code map are in [AGENTS.md](../AGENTS.md).

## Build times

Dependencies build unoptimized in the `dev` profile except the few on a runtime path (the kernel's
gunzip, the NAT, checksums, base64, PNG, audio), which are listed in the root `Cargo.toml`: a
dependency that a hot loop in `emu` or `simee` calls into belongs on that list, or a debug `emu`
gets slow. The desktop's `cargo test` builds Rune unoptimized too; its `cargo build`/`run` keep
opt-level 2. Numbers and what was tried are in [benchmarks.md](benchmarks.md#build-times-local).

## Benchmarks

Every script under `scripts/bench/` waits for a quiet host (`scripts/bench/quiet.sh`) before it
measures, refuses with exit status 4 and a list of what is busy when the host stays busy, and writes
a host record beside its results. `--no-quiet-gate` skips the gate for a smoke run, and `run-all.sh
--quiet` is the run whose numbers get published. The protocol, the record's fields and the noise
budgets are in [research/bench-protocol.md](research/bench-protocol.md); its tests are
`scripts/bench/test-quiet.sh`, `test-run-all.sh`, `test-renderers.sh` and `test-ios-slim.sh`,
which need no device.

## Harness

[auto-mobile](https://github.com/kaeawc/auto-mobile) maps both the public and private Apple
simulator APIs and is the reference for behavior. Three layers check simee against it and against
`simctl`:

| Check | What it proves |
| --- | --- |
| `cargo t` | pure logic: selection, gestures, key maps, pixel conversion |
| `SIMEE_E2E_UDID=<udid> cargo nextest run -p simee --test e2e` | simee commands on a real device; identity matches `simctl` |
| `scripts/parity.sh <udid>` | identity, screenshot size and pixels, and app list match `simctl` |
| `scripts/automobile-harness.sh <udid>` | auto-mobile acquires, observes and drives a simee-booted device (private daemon; touches no other daemon) |

Android checks (Apple silicon): `cargo run -p simee-emu -- -avd <name> -show-kernel -verbose`, then
`adb -s emulator-5554 shell getprop sys.boot_completed`;
`SIMEE_E2E_AVD=<avd> cargo nextest run -p simee --test e2e_android`;
`scripts/automobile-harness-android.sh emulator-5554` (AutoMobile's tools against emu).

Many devices of both platforms at once: `scripts/automobile-farm.sh <udid> emulator-5554 emulator-5556`
runs each harness through one private AutoMobile daemon, one session per device.

## Install

From crates.io (`--locked` builds with the dependency versions simee was tested with):

```bash
cargo install --locked simee       # the CLI, `simee serve` (the API) and `simee mcp`
cargo install --locked simee-emu   # `emu`, the Android emulator; simee finds it beside itself
# macOS: Hypervisor.framework only runs binaries with its entitlement (this repo's file)
codesign -s - -f --entitlements scripts/hypervisor.entitlements ~/.cargo/bin/emu
```

Or a binary archive from the [GitHub releases](https://github.com/kaeawc/simee/releases):
`simee-vX.Y.Z-<target>.tar.gz` holds `simee` and `emu` for `aarch64-apple-darwin` (emu already
signed), `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.

## Releasing

Every workspace crate is published to crates.io under a `simee-` name (`simee-core`,
`simee-emu-vmm`, …; the CLI is `simee`, the emulator `simee-emu`), at the one workspace version,
with `=X.Y.Z` pins between them. Each library is named after its package, so a downstream user
writes `simee-core = "0.1"` and `use simee_core::...`. Workspace code keeps the old names:
`[workspace.dependencies]` maps the key `sim-core` to the `simee-core` package, so a dependent
says `use sim_core::...`, while a crate's own tests and examples use its package name
(`simee_core::`) and cargo's `-p` takes the package name (`cargo test -p simee-core`). Every
crate ships the MIT text through a `LICENSE` symlink to the root one. The desktop app
(`desktop/`) is not published.

1. Add what changed under `## [Unreleased]` in [CHANGELOG.md](../CHANGELOG.md) as you go.
2. `scripts/release.sh 0.2.0` branches from `origin/main`, bumps the workspace version and the
   pins (and both lockfiles), moves the Unreleased entries under `## [0.2.0] - <date>`, pushes
   `release/v0.2.0` and opens the PR.
3. Merging it runs [release.yml](../.github/workflows/release.yml): binaries for
   `aarch64-apple-darwin` (`emu` ad-hoc signed with `scripts/hypervisor.entitlements`) and Linux
   x86_64 and aarch64, `cargo package --workspace` on macOS (each crate built from its `.crate`),
   `scripts/publish.sh` with a crates.io token from trusted publishing, then the `v0.2.0` tag and
   GitHub release with the archives and the CHANGELOG section as notes.

Every step can be rerun: `scripts/publish.sh` skips crates whose version is already on crates.io,
and an existing release gets its assets replaced. Dry runs: `scripts/publish.sh --dry-run`
(packages and verifies what is missing, uploads nothing), or run the Release workflow by hand
(builds and packages on any branch, publishes nothing).

Trusted publishing (no long-lived token in the repository): the `publish` job runs in the
`crates-io` GitHub environment with `id-token: write`, and `rust-lang/crates-io-auth-action`
trades the job's OIDC token for a short-lived crates.io token. Each crate's settings on crates.io
name the trusted publisher: repository `kaeawc/simee`, workflow `release.yml`, environment
`crates-io`. crates.io only offers that once a crate exists, so a new crate's first version is
published with a personal token and gets its trusted publisher added afterwards.

First publish (no simee crate on crates.io yet; the Release workflow skips with a notice until
one is):

1. On crates.io, create an API token scoped to `publish-new` and `publish-update`, with the
   crate name pattern `simee*` and a short expiry. `cargo login` with it.
2. From an up-to-date `main` checkout: `scripts/publish.sh --dry-run`, then `scripts/publish.sh`.
   crates.io rate-limits new crates, so expect HTTP 429 partway through; wait and rerun, it
   skips what is already up.
3. For every crate, add the trusted publisher above in its crates.io settings.
4. Delete the token on crates.io and `cargo logout`.
5. Push the `vX.Y.Z` tag at the commit you published from (`git tag v0.1.0 <sha> && git push
   origin v0.1.0`): the Release workflow builds the binaries and creates the GitHub release, and
   its `scripts/publish.sh` skips every crate, now that each one is up.

## Linters

Scripts, docs and workflows have their own linters, each configured by a file at the root.
`scripts/lint.sh` runs them: CI's `docs-scripts` job does, and so does `scripts/check.sh`, first,
before its docs-only exit. A linter that isn't installed prints a skip line (`--require` makes it
fail, as in CI). Name tools to run just those: `scripts/lint.sh shfmt shellcheck`.

| Tool | Checks | Config |
| --- | --- | --- |
| [shellcheck](https://www.shellcheck.net) | every shell script in the tree (`*.sh`, `.githooks/*`, sh and bash shebangs), with a few optional checks on | `.shellcheckrc` |
| [shfmt](https://github.com/mvdan/sh) | the same scripts' formatting: 2-space indents, `case` branches indented (`shfmt -w <file>` fixes it) | `.editorconfig` |
| [lychee](https://lychee.cli.rs) | every Markdown link to a file in the tree, and the heading it points at; offline, so web links aren't fetched | `lychee.toml` |
| [typos](https://github.com/crate-ci/typos) | misspellings in code, docs and file names (`typos -w` fixes them) | `_typos.toml` |
| [actionlint](https://github.com/rhysd/actionlint) | the workflows, with shellcheck over their `run:` blocks | none |
| [yamllint](https://yamllint.readthedocs.io) | every YAML file | `.yamllint.yml` |

On macOS: `brew install shellcheck shfmt lychee typos-cli actionlint yamllint`. CI installs pinned
versions in `.github/workflows/ci.yml`'s `docs-scripts` job; bump them there.

Fix what a linter reports rather than silencing it. A word spelled that way on purpose goes in
`_typos.toml` with the reason; a shellcheck finding that is wrong for one line gets a
`# shellcheck disable=SCxxxx` directive on that line with a comment saying why. A file excluded
in a config while another branch owns it carries a TODO naming that branch: drop the exclusion
when it merges.

