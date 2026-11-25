use frog::labels::{self, Label, LabelAction, LabelRef, SystemLabel};
use frog::{parse_gmail_fetch, render_plaintext, uid_store_labels};

/// Builds a FETCH response with a literal body. The body deliberately contains
/// `)`, `"` and CRLF so the literal skipping is exercised.
fn fetch_with_body(body: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(
        format!(
            "* 12 FETCH (UID 345 X-GM-MSGID 1278455344230334865 X-GM-THRID 1266894439833179 \
             X-GM-LABELS (\"\\\\Inbox\" \"\\\\Important\" \"Work/Projects\") BODY[] {{{}}}\r\n",
            body.len()
        )
        .as_bytes(),
    );
    v.extend_from_slice(body);
    v.extend_from_slice(b")\r\n");
    v
}

#[test]
fn parses_gmail_extensions_and_literal_body() {
    let body = b"Hello (world) \"quoted\"\r\nSecond line)\r\n";
    let raw = fetch_with_body(body);
    let f = parse_gmail_fetch(&raw).expect("parse");

    assert_eq!(f.seq, 12);
    assert_eq!(f.uid, Some(345));
    assert_eq!(f.msgid, Some(1278455344230334865));
    assert_eq!(f.thrid, Some(1266894439833179));
    assert_eq!(f.labels, vec!["\\Inbox", "\\Important", "Work/Projects"]);
    assert_eq!(f.body, Some(&body[..]));
}

#[test]
fn nil_labels_yield_empty_set() {
    let raw = b"* 3 FETCH (UID 9 X-GM-MSGID 42 X-GM-THRID 7 X-GM-LABELS NIL)\r\n";
    let f = parse_gmail_fetch(raw).unwrap();
    assert!(f.labels.is_empty());
    assert_eq!(f.body, None);
}

#[test]
fn literal_bytes_may_be_non_utf8() {
    let body = [0xffu8, 0x00, b')', b'\r', b'\n', 0x80];
    let raw = fetch_with_body(&body);
    let f = parse_gmail_fetch(&raw).unwrap();
    assert_eq!(f.body, Some(&body[..]));
}

#[test]
fn rejects_non_fetch_responses() {
    assert!(parse_gmail_fetch(b"* 5 EXISTS\r\n").is_err());
    assert!(parse_gmail_fetch(b"* 1 FETCH (UID 5 X-GM-LABELS (\"unterminated").is_err());
}

#[test]
fn classifies_labels() {
    assert_eq!(
        Label::classify("\\Inbox"),
        Label::System(SystemLabel::Inbox)
    );
    assert_eq!(Label::classify("\\Muted"), Label::Other("\\Muted"));
    assert_eq!(
        Label::classify("Work/Projects"),
        Label::User("Work/Projects")
    );
}

#[test]
fn archive_removes_inbox_with_compact_uid_set() {
    let cmd = labels::archive(&[3, 1, 2, 7, 2]).unwrap();
    assert_eq!(cmd, r#"UID STORE 1:3,7 -X-GM-LABELS (\Inbox)"#);
}

#[test]
fn trash_adds_trash_label() {
    let cmd = labels::trash(&[10]).unwrap();
    assert_eq!(cmd, r#"UID STORE 10 +X-GM-LABELS (\Trash)"#);
}

#[test]
fn user_label_names_are_quoted_and_escaped() {
    let cmd = uid_store_labels(
        &[5],
        LabelAction::Add,
        &[
            LabelRef::Name(r#"He said "hi" \o/"#),
            LabelRef::Name("Work/Projects"),
        ],
    )
    .unwrap();
    assert_eq!(
        cmd,
        r#"UID STORE 5 +X-GM-LABELS ("He said \"hi\" \\o/" "Work/Projects")"#
    );
}

#[test]
fn replace_with_empty_list_clears_labels() {
    let cmd = uid_store_labels(&[1], LabelAction::Replace, &[]).unwrap();
    assert_eq!(cmd, "UID STORE 1 X-GM-LABELS ()");
    assert!(uid_store_labels(&[1], LabelAction::Add, &[]).is_none());
    assert!(
        uid_store_labels(
            &[],
            LabelAction::Add,
            &[LabelRef::System(SystemLabel::Spam)]
        )
        .is_none()
    );
}

#[test]
fn renders_plaintext_part_of_multipart() {
    let raw = concat!(
        "From: Ada Lovelace <ada@example.com>\r\n",
        "To: Charles <charles@example.com>, Mary <mary@example.com>\r\n",
        "Subject: Plaintext please\r\n",
        "Date: Tue, 10 Nov 2026 09:30:00 +0000\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/alternative; boundary=\"b1\"\r\n",
        "\r\n",
        "--b1\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Plain body line.\r\n",
        "--b1\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<p>HTML body that should not win</p>\r\n",
        "--b1--\r\n",
    );
    let out = render_plaintext(raw.as_bytes()).unwrap();
    assert!(
        out.contains("From: Ada Lovelace <ada@example.com>"),
        "{out}"
    );
    assert!(out.contains("To: Charles <charles@example.com>"), "{out}");
    assert!(out.contains("Subject: Plaintext please"), "{out}");
    assert!(out.contains("Plain body line."), "{out}");
    assert!(!out.contains("HTML body that should not win"), "{out}");
    assert!(!out.contains('\r'), "{out:?}");
}

#[test]
fn falls_back_to_html_converted_to_text() {
    let raw = concat!(
        "From: a@example.com\r\n",
        "Subject: html only\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<html><body><p>Hello <b>there</b></p></body></html>\r\n",
    );
    let out = render_plaintext(raw.as_bytes()).unwrap();
    assert!(out.contains("Hello"), "{out}");
    assert!(out.contains("there"), "{out}");
    assert!(!out.contains("<b>"), "{out}");
}
