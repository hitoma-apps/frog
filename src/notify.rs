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

/// Escapes the characters that start markup, so the text is shown exactly as it was written.
pub fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Shows one notification for a batch of new mail. Nothing is shown for an empty batch.
///
/// `on_click` runs when the user clicks the notification. Waiting for that happens on a thread of
/// its own, which ends when the notification goes away.
pub fn new_mail(
    items: &[NewMail],
    detail: NotificationDetail,
    has_code: bool,
    on_click: impl FnOnce() + Send + 'static,
    on_copy: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    let (summary, body) = content(items, detail);
    // On Linux the notification server reads the text as light markup, so mail text must not be
    // able to add links or emphasis. Other systems show the text as it is.
    let (summary, body) = if cfg!(all(unix, not(target_os = "macos"))) {
        (escape_markup(&summary), escape_markup(&body))
    } else {
        (summary, body)
    };
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("FROG")
        .summary(&summary)
        .body(&body)
        .action("default", "Open");
    // A code is offered only for one message, and only when the notification shows the subject.
    // The button says "Copy code" rather than the code, since a locked screen can show it.
    let offer_copy = has_code && items.len() == 1 && detail == NotificationDetail::Full;
    if offer_copy {
        notification.action("copy", "Copy code");
    }
    let handle = notification.show().map_err(|e| e.to_string())?;
    let mut on_click = Some(on_click);
    let mut on_copy = Some(on_copy);
    std::thread::spawn(move || {
        handle.wait_for_action(|action| match action {
            "default" => {
                if let Some(run) = on_click.take() {
                    run();
                }
            }
            "copy" => {
                if let Some(run) = on_copy.take() {
                    run();
                }
            }
            _ => {}
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
    fn markup_in_mail_text_is_shown_as_text() {
        assert_eq!(
            escape_markup("<a href=\"https://evil.example\">Your bank</a> & more"),
            "&lt;a href=\"https://evil.example\"&gt;Your bank&lt;/a&gt; &amp; more"
        );
    }

    #[test]
    fn an_empty_batch_has_no_text() {
        assert_eq!(
            content(&[], NotificationDetail::Full),
            (String::new(), String::new())
        );
    }
}
