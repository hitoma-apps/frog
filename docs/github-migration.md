# Moving FROG to GitHub

The repository moves from Forgejo to the public repo `github.com/hitoma-apps/frog`. The move also brings GitHub Actions on public runners, a macOS build, Windows signing with Azure Artifact Signing, and auto-updates.

Each step has a ticket. Steps are in dependency order.

| Step | Ticket | Depends on |
|---|---|---|
| 1. Prepare the repo for public release | #70 | — |
| 2. Create `hitoma-apps/frog` and push `main` | #71 | 1 |
| 3. Migrate issues with their numbers | #72 | 2 |
| 4. GitHub Actions on public runners | #73 | 2 |
| 5. macOS build | #74 | 4 |
| 6. Sign Windows builds with Azure Artifact Signing | #75 | 4 |
| 7. Releases and auto-update | #69, plan in [`auto-update.md`](auto-update.md) | 4, 5, 6 |

## What the pre-move scans found

- **History:** 137 commits, all authored as `Claude <noreply@anthropic.com>`. Searching every diff and commit message found no keys, tokens, passwords, personal email addresses or home paths. The matches were test fixtures (`me@gmail.com`, `app-pass`), `imap.gmail.com` and public Forgejo action URLs.
- **Files:** the only certificate ever committed is `signing/gmail-labels-test-signing.cer`, a public certificate with no private key.
- **Branches:** the Forgejo hostname appears once, in a deleted probe workflow on the parked `workflows-split` branch. Only `main` moves.
- **Issues:** numbered from 1 with no gaps and no pull requests. No labels or milestones. Searching issue text and comments found nothing sensitive.

A text search isn't a guarantee, so step 1 includes a gitleaks scan before the first push.

## Steps

### 1. Prepare the repo for public release (#70)

- Add a `LICENSE` and update the README.
- Delete the test certificate and the `.forgejo` workflows.
- Optionally rename the package from `gmail-imap-proto` to `frog`. The data folder and keyring name stay the same, so existing caches and saved sign-ins still work.
- Run gitleaks over the full history.

### 2. Create `hitoma-apps/frog` and push `main` (#71)

- The org owner creates the empty repo, with no README, so issue numbers start at 1.
- Push `main` with its history.
- Set up branch protection on `main`.

### 3. Migrate issues with their numbers (#72)

- Create every Forgejo issue in order, before any pull request exists, so the numbers match and the `Closes #N` references in commit messages stay correct.
- Close the issues that are closed on Forgejo.
- Post the comments in order, each noting its original date. They'll appear as posted by the migrating account.
- Pace the requests to stay under GitHub's limits on creating content.
- Archive the Forgejo repo with a pointer to GitHub.

### 4. GitHub Actions on public runners (#73)

- Run format, clippy and tests on `ubuntu-latest`, with build caching.
- Use `upload-artifact@v4` or later; `v3` no longer works on GitHub.
- Build Windows natively on `windows-latest` with the MSVC target. Microsoft's signing tooling and Velopack's Windows packaging both expect Windows.
- Keep the AppImage job.

### 5. macOS build (#74)

- Build on `macos-latest` (Apple Silicon), for ARM only or as a universal binary.
- The first builds are unsigned, so Gatekeeper warns on first open.
- Signing and notarization need an Apple Developer account ($99/year). Artifact Signing doesn't cover macOS.

### 6. Sign Windows builds with Azure Artifact Signing (#75)

Needed from the account owner:

- an Azure subscription;
- an Artifact Signing account, with identity validation, likely as hitoma-apps;
- a public-trust certificate profile;
- a federated credential, so GitHub logs in to Azure over OIDC with no stored secret.

In CI: Azure login over OIDC, then signing on the Windows runner. Velopack may be able to call Artifact Signing itself.

### 7. Releases and auto-update (#69)

Versioned tags build GitHub Releases, and Velopack packages each platform and publishes to the release. The app downloads updates in the background and offers "Restart to update". The full plan is in [`auto-update.md`](auto-update.md). Smart App Control blocks unsigned Windows updates, so step 6 comes first.

## Decisions pending

1. History: keep it (recommended, since the scans were clean), or start from a fresh commit?
2. License: for example MIT, Apache-2.0, or both.
3. Rename the package and crate to `frog`?
4. Forgejo afterwards: archive, mirror or delete? And the earlier stray GitHub copy, `shawngmc/gmail-imap-labels-experiment`: delete it?
5. Windows: build natively with MSVC (recommended), or keep cross-compiling?
6. macOS: Apple Silicon only, or universal?
7. Artifact Signing: does hitoma-apps qualify for identity validation?
