# FROG 0.1.5

## Changes

- **One-time codes.** The reader finds one-time login codes in a message and shows a **Copy** button for each, so the code can be pasted without selecting it. Nothing is copied until you press the button.
- **Copy from a notification.** When a single new message has a code in its subject, its notification offers **Copy code**. The button doesn't show the number. It's only offered when notifications show the subject.
- **Verification codes.** Tools → Verification codes lists cached messages whose subjects look like code emails, so you can check them and move them to Trash. Trash moves whole threads. The list matches subjects only, not message bodies.
- **Logging.** Lines are written as `<UTC time> - <SUBSYSTEM> - <what happened>` (APP, IMAP, WORKER, CACHE, UPDATE). The Log window shows them, and they're also written to `frog.log` in the data folder. They never include mail content.
- **Sync progress.** The first sync after a long gap fetches the headers of new messages in batches, and the window shows progress between them.
- **Updates.** The update check logs its steps to `frog.log`.

## Downloads

- **Linux (x86_64):** `frog.AppImage`. Installed copies update themselves to this version.
- **macOS (universal):** `FROG-macos-universal.zip`. Not yet signed with a Developer ID.
- **Windows (x86_64):** `frog.exe`. Not yet code-signed. Automatic updates on Windows wait for signing.
