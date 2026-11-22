# FROG 0.1.1

Dependency updates and fixes.

## Changes

- Updated the file dialog, the message parser (mail-parser 1.0) and the local database library (rusqlite 0.43), and the GitHub Actions they depend on.
- Group recipients in a message (`To: Team: a@x, b@x;`) are listed one by one in the message view and in "Show original".

## Downloads

- **Linux (x86_64):** `frog.AppImage`. Installed copies of 0.1.0 update themselves to this version.
- **macOS (universal):** `FROG-macos-universal.zip`. Not yet signed with a Developer ID.
- **Windows (x86_64):** `frog.exe`. Not yet code-signed. Automatic updates on Windows wait for signing.
