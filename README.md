# FROG

**FROG** (Fast Read Only Gmail) is a small, fast, read-only Gmail client for Windows and Linux. It talks to Gmail over IMAP, keeps a local SQLite cache, and shows Gmail labels as they are.

## Building

```bash
cargo build --release
```

The program is `target/release/frog` (`frog.exe` on Windows).

## Where data is kept

FROG stores its cache and settings under the platform's data directory, in a folder named `gmail-labels-proto`. Passwords go in the system keyring under the same name. These names are kept so existing caches and saved sign-ins still work.
