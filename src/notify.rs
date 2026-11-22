//! Desktop notifications for new mail.

use frog::settings::NotificationDetail;

/// What a notification needs to say about one message.
pub struct NewMail {
    pub from: String,
    pub subject: String,
}

/// The title and text of a notification for a batch of new mail, at the chosen level of detail.
/// Nothing is shown for an empty batch.
pub fn content(items: &[NewMail], detail: NotificationDetail) -> (String, String) {
    let senders = |n: usize| {
        items
            .iter()
            .take(n)
            .map(|m| m.from.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    match (items, detail) {
        ([], _) => (String::new(), String::new()),
        ([_], NotificationDetail::Hidden) => ("New mail".to_string(), String::new()),
        ([_], NotificationDetail::SenderOnly) => ("New mail".to_string(), senders(1)),
        ([one], NotificationDetail::Full) => (
            "New mail".to_string(),
            format!("{}\n{}", one.from, one.subject),
        ),
        (many, NotificationDetail::Hidden) => {
            (format!("{} new messages", many.len()), String::new())
        }
        (many, NotificationDetail::SenderOnly) => {
            (format!("{} new messages", many.len()), senders(3))
        }
        (many, NotificationDetail::Full) => (
            format!("{} new messages", many.len()),
            many.iter()
                .take(3)
                .map(|m| format!("{}: {}", m.from, m.subject))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    }
}

/// Shows one notification for a batch of new mail. Nothing is shown for an empty batch.
///
/// `on_click` runs when the user clicks the notification. Waiting for that happens on a thread of
/// its own, which ends when the notification goes away.
pub fn new_mail(
    items: &[NewMail],
    detail: NotificationDetail,
    on_click: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    let (summary, body) = content(items, detail);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(from: &str, subject: &str) -> NewMail {
        NewMail {
            from: from.into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn full_detail_shows_sender_and_subject() {
        let (title, body) = content(
            &[mail("Ana <ana@example.com>", "Lunch?")],
            NotificationDetail::Full,
        );
        assert_eq!(title, "New mail");
        assert_eq!(body, "Ana <ana@example.com>\nLunch?");
    }

    #[test]
    fn sender_only_leaves_out_subjects() {
        let items = [mail("Ana", "Secret plan"), mail("Ben", "Other")];
        let (title, body) = content(&items, NotificationDetail::SenderOnly);
        assert_eq!(title, "2 new messages");
        assert_eq!(body, "Ana\nBen");
        assert!(!body.contains("Secret"));
    }

    #[test]
    fn hidden_shows_no_sender_or_subject() {
        let items = [mail("Ana", "Secret plan")];
        let (title, body) = content(&items, NotificationDetail::Hidden);
        assert_eq!(title, "New mail");
        assert!(body.is_empty());
        let (title, body) = content(
            &[mail("Ana", "x"), mail("Ben", "y")],
            NotificationDetail::Hidden,
        );
        assert_eq!(title, "2 new messages");
        assert!(body.is_empty());
    }

    #[test]
    fn an_empty_batch_has_no_text() {
        assert_eq!(
            content(&[], NotificationDetail::Full),
            (String::new(), String::new())
        );
    }
}
