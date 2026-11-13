//! Desktop notifications for new mail.

/// What a notification needs to say about one message.
pub struct NewMail {
    pub from: String,
    pub subject: String,
}

/// Shows one notification for a batch of new mail. Nothing is shown for an empty batch.
pub fn new_mail(items: &[NewMail]) -> Result<(), String> {
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
    notify_rust::Notification::new()
        .appname("Gmail Labels Prototype")
        .summary(&summary)
        .body(&body)
        .show()
        .map(drop)
        .map_err(|e| e.to_string())
}
