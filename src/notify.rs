//! Desktop notifications for new mail.

/// What a notification needs to say about one message.
pub struct NewMail {
    pub from: String,
    pub subject: String,
}

/// Shows one notification for a batch of new mail. Nothing is shown for an empty batch.
///
/// `on_click` runs when the user clicks the notification. Waiting for that happens on a thread of
/// its own, which ends when the notification goes away.
pub fn new_mail(items: &[NewMail], on_click: impl FnOnce() + Send + 'static) -> Result<(), String> {
    let (summary, body) = match items {
        [] => return Ok(()),
        [one] => (
            "New mail".to_string(),
            format!("{}\n{}", one.from, one.subject),
        ),
        many => (
            format!("{} new messages", many.len()),
            many.iter()
                .take(3)
                .map(|m| format!("{}: {}", m.from, m.subject))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    };
    let handle = notify_rust::Notification::new()
        .appname("FROG")
        .summary(&summary)
        .body(&body)
        .action("default", "Open")
        .show()
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        handle.wait_for_action(|action| {
            if action == "default" {
                on_click();
            }
        });
    });
    Ok(())
}
