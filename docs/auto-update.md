# Auto-update plan

FROG should update itself, so a new release doesn't mean downloading the executable, closing the app, replacing the file and reopening it by hand.

## Decisions

| Question | Decision |
|---|---|
| Library | [Velopack](https://docs.rs/velopack): an installer and update framework with a Rust crate. `vpk` packages Windows, macOS and Linux (AppImage). |
| Hosting | GitHub Releases. Velopack reads them directly, and GitHub has macOS runners. |
| macOS | Later, but soon. Build Windows and Linux first, and keep macOS in mind. |
| Update behaviour | Download in the background, then offer "Restart to update". |

### Why Velopack

- It's a mature project: the successor to Squirrel, a long-established Windows updater.
- One tool builds the installer or package for each platform. The Linux output is an AppImage.
- The crate handles checking, downloading, applying and restarting.
- Cost: `vpk` needs the .NET SDK on the build machine. The library in the app is Rust.

Alternatives considered:

- `cargo-packager` with `cargo-packager-updater`: all Rust, with minisign signatures, but version 0.2.
- `self_update`: replaces only the binary. The AppImage and macOS bundle would be ours to handle.

## Phases

1. **Versioning and releases.** The app is always `0.1.0` now. Bump the version in `Cargo.toml` for each release, tag it, and build the release in CI from the tag.
2. **Trial on Linux.** Package the Linux build with `vpk`, publish it to GitHub Releases, and update from one version to the next. The trial must settle:
   - whether the Velopack crate builds for `x86_64-pc-windows-gnu`, our Windows target;
   - whether `vpk` can package the Windows build on Linux, or needs a Windows runner;
   - how Velopack checks that a downloaded update is genuine (HTTPS alone, its own hashes, or OS code signing).
3. **Release CI on GitHub.** Build, package and publish Windows and Linux. The Velopack AppImage replaces `packaging/appimage/build.sh`.
4. **In the app.**
   - Run `VelopackApp` at the start of `main`.
   - Check for updates on a background thread at startup and every few hours, apart from the mail worker.
   - Download in the background, then show "FROG x.y.z is ready – Restart to update" in the status bar.
   - Add an Updates section in Settings: check automatically (on/off), Check now, and the last check time.
   - Settings, the cache and the keyring stay where they are, so an update doesn't touch user data.
5. **Signing.** An updater downloads and runs code, so signing protects the update path as well as getting past Windows Smart App Control. Velopack's installer and updater need signing too. The options are in the code signing notes: Artifact Signing (Microsoft) or a certificate from a commercial CA. macOS will need an Apple Developer ID and notarization.
6. **macOS.** Add a macOS runner, packaging, signing and notarization.
7. **Rollout.** Install the first Velopack build by hand. Later releases arrive through the app. Test a bad release and how to recover from it.

## Open questions

- Should the repository move to GitHub, or only the releases and release CI?
- Release channels: is one stable channel enough, or is a test channel worth having?
- Smart App Control will block unsigned releases on Windows, so signing may need to come before Windows updates are useful.
