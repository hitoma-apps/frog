# FROG

**FROG** (Fast Read-Oriented Gmail) is a small, fast Gmail client built for reading for Windows and Linux. It talks to Gmail over IMAP, keeps a local SQLite cache, and shows Gmail labels as they are.

## Building

```bash
cargo build --release
```

The program is `target/release/frog` (`frog.exe` on Windows).

## Installing on Linux

Wayland desktops take a window's icon from an installed desktop entry, not from the window itself. To show the FROG icon in the taskbar and launcher, run:

```bash
packaging/linux/install.sh
```

It copies `frog.desktop` and the icon into your user folders. It doesn't need root.

## Where data is kept

FROG stores its cache and settings under the platform's data directory, in a folder named `gmail-labels-proto`. Passwords go in the system keyring under the same name. These names are kept so existing caches and saved sign-ins still work.
