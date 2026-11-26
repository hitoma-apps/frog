# FROG 0.1.8

## Changes

- The dark theme's text is brighter, including the grey counts in the sidebar.
- Sidebar sections (Search, Favorites, Mailboxes, Tools, Labels) share one heading style.
- Unread threads have a green dot, since bold isn't available.
- A conversation's actions and labels sit in one card.
- Rows and sidebar items have a faint hover fill. The selected one has an accent bar.
- Toolbar buttons are grouped: moving mail, read state, and labels.
- Save in a conversation is a menu, with .EML or Plaintext.
- Thread and message IDs are behind a (!) tooltip.
- Label chips in a conversation have an × to remove that label.
- Add label is a text field that filters your labels as you type.
- Small text is a little larger, for chips and dates.
- The Log window draws only the lines on screen, and keeps fewer in memory. The log file keeps every line.
- Links open only when they're web addresses. Anything else is shown as text.
- Attachment and thread file names are safe on Windows: device names such as CON and NUL are changed, and long names are shortened.
- On Linux, notification text from mail is shown as text, so it can't add links or emphasis.

## Downloads

- **Linux (x86_64):** `frog.AppImage`. Installed copies of 0.1.0 and later update themselves to this version.
- **macOS (universal):** `FROG-macos-universal.zip`. Not yet signed with a Developer ID.
- **Windows (x86_64):** `frog.exe`. Not yet code-signed. Automatic updates on Windows wait for signing.
