# FROG 0.1.7

## Changes

- Archive and Trash now change the message in Gmail. They had been sent in a form Gmail didn't accept for system labels.
- Archive and Trash remove the message from the list at once, and show a progress note while Gmail confirms. If Gmail refuses the change, the message comes back and an error is shown.
- A Trash click moves the message out of the Inbox as well as adding Trash, as Gmail's web app does.
- When Gmail's reply doesn't list a changed message, FROG applies the change to its cache, so the list stays right.
- Label changes are recorded in the log, to help diagnose a change that doesn't appear.

## Downloads

- **Linux (x86_64):** `frog.AppImage`. Installed copies of 0.1.0 and later update themselves to this version.
- **macOS (universal):** `FROG-macos-universal.zip`. Not yet signed with a Developer ID.
- **Windows (x86_64):** `frog.exe`. Not yet code-signed. Automatic updates on Windows wait for signing.
