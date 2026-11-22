//! Automatic updates, through Velopack. A background thread checks GitHub Releases for a newer
//! version, downloads it, and waits. It applies the update only when the user asks, since
//! applying it restarts the app.
//!
//! The thread owns the update manager, so the window never waits on the network. Builds that
//! weren't installed by Velopack (for example `cargo run`) have no manager, and report that
//! instead of checking.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

/// Where releases are published. Velopack reads the release list from here.
pub const REPO_URL: &str = "https://github.com/hitoma-apps/frog";

/// How often the thread checks by itself, while automatic checks are on.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What the app asks of the update thread.
pub enum Command {
    /// Check now, whatever the schedule says.
    CheckNow,
    /// Check on a schedule (on), or only when asked (off).
    SetAutomatic(bool),
    /// Apply the downloaded update and restart. The app exits if this succeeds.
    ApplyNow,
}

/// What the update thread reports back.
pub enum Event {
    /// This build wasn't installed by Velopack, so there's nothing to update.
    NotInstalled,
    /// A check finished. `available` is the version on offer, if it's newer than this one.
    Checked { available: Option<String> },
    /// A newer version is downloaded and ready to apply.
    Ready(String),
    /// A check or download failed. The text is for the user.
    Failed(String),
}

/// The app's end of the update thread.
pub struct Updates {
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
}

/// Starts the update thread. `wake` is called whenever it has something to report, so the window
/// redraws.
pub fn start(automatic: bool, wake: impl Fn() + Send + 'static) -> Updates {
    let (commands, command_rx) = channel();
    let (event_tx, events) = channel();
    std::thread::spawn(move || {
        let report = |event: Event| {
            let _ = event_tx.send(event);
            wake();
        };
        run(command_rx, automatic, report);
    });
    Updates { commands, events }
}

fn run(commands: Receiver<Command>, mut automatic: bool, report: impl Fn(Event)) {
    let manager = match UpdateManager::new(GithubSource::new(REPO_URL, None, false), None, None) {
        Ok(manager) => manager,
        Err(_) => {
            report(Event::NotInstalled);
            return;
        }
    };
    let mut pending: Option<UpdateInfo> = None;
    // The first check happens at startup, if automatic checks are on.
    let mut next_check = Instant::now();
    loop {
        if automatic && Instant::now() >= next_check {
            check(&manager, &mut pending, &report);
            next_check = Instant::now() + CHECK_EVERY;
        }
        let wait = next_check.saturating_duration_since(Instant::now());
        let wait = if automatic {
            wait
        } else {
            Duration::from_secs(3600)
        };
        match commands.recv_timeout(wait) {
            Ok(Command::CheckNow) => {
                check(&manager, &mut pending, &report);
                next_check = Instant::now() + CHECK_EVERY;
            }
            Ok(Command::SetAutomatic(on)) => {
                automatic = on;
                next_check = Instant::now();
            }
            Ok(Command::ApplyNow) => {
                if let Some(info) = &pending {
                    // On success this restarts the app, so it doesn't return.
                    if let Err(e) = manager.apply_updates_and_restart(info) {
                        report(Event::Failed(format!("Could not apply the update: {e}")));
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Checks for a newer release, and downloads it if there is one.
fn check(manager: &UpdateManager, pending: &mut Option<UpdateInfo>, report: &impl Fn(Event)) {
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(info)) => match manager.download_updates(&info, None) {
            Ok(()) => {
                let version = info.TargetFullRelease.Version.clone();
                *pending = Some(*info);
                report(Event::Ready(version));
            }
            Err(e) => report(Event::Failed(format!("Could not download the update: {e}"))),
        },
        Ok(_) => report(Event::Checked { available: None }),
        Err(e) => report(Event::Failed(format!("Could not check for updates: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_not_installed_by_velopack_reports_that_and_stops() {
        // Tests run from cargo, so there's no Velopack install to update.
        let updates = start(true, || {});
        let event = updates
            .events
            .recv_timeout(Duration::from_secs(10))
            .expect("the update thread reported nothing");
        assert!(matches!(event, Event::NotInstalled));
        // The thread has ended, so nothing else is reported.
        assert!(
            updates
                .events
                .recv_timeout(Duration::from_millis(200))
                .is_err()
        );
    }

    #[test]
    fn the_repository_is_the_public_one() {
        assert!(REPO_URL.starts_with("https://github.com/hitoma-apps/"));
        assert!(!REPO_URL.ends_with('/'));
    }
}
