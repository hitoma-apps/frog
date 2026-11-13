//! Turns a plaintext message into what the reader draws: text lines,
//! collapsible quoted replies, and text with clickable links.
//!
//! This is plain logic with no UI, so it can be tested directly.

/// Quoted runs shorter than this stay visible. Longer runs collapse.
pub const COLLAPSE_QUOTES_AT: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// One line of text, as it appears in the message.
    Line(String),
    /// A run of quoted lines, with the `>` markers removed.
    Quote(Vec<String>),
}

/// A piece of one line: plain text, or a web link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    Link(&'a str),
}

/// Groups a message's lines into blocks. Consecutive quoted lines form one
/// block. Short quotes are kept as plain lines, so only long ones collapse.
pub fn blocks(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut quote: Vec<String> = Vec::new();
    for line in text.lines() {
        match strip_quote(line) {
            Some(inner) => quote.push(inner),
            None => {
                flush_quote(&mut out, &mut quote);
                out.push(Block::Line(line.to_string()));
            }
        }
    }
    flush_quote(&mut out, &mut quote);
    out
}

fn flush_quote(out: &mut Vec<Block>, quote: &mut Vec<String>) {
    if quote.is_empty() {
        return;
    }
    let lines = std::mem::take(quote);
    if lines.len() >= COLLAPSE_QUOTES_AT {
        out.push(Block::Quote(lines));
    } else {
        out.extend(lines.into_iter().map(|l| Block::Line(format!("> {l}"))));
    }
}

/// If `line` is quoted (starts with `>`, possibly nested), returns its content.
fn strip_quote(line: &str) -> Option<String> {
    let mut rest = line.trim_start();
    if !rest.starts_with('>') {
        return None;
    }
    while let Some(r) = rest.strip_prefix('>') {
        rest = r.trim_start();
    }
    Some(rest.to_string())
}

const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Splits a line into text and links. Trailing punctuation after a link is text.
pub fn split_links(line: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = next_link_start(rest) {
        if start > 0 {
            out.push(Segment::Text(&rest[..start]));
        }
        let tail = &rest[start..];
        let word_len = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let word = &tail[..word_len];
        let url = word.trim_end_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '>' | '"' | '\''
            )
        });
        let scheme_len = SCHEMES
            .iter()
            .find(|s| url.starts_with(*s))
            .map_or(0, |s| s.len());
        // A link consumes only its URL. Trailing punctuation is scanned as text.
        let (segment, used) = if url.len() > scheme_len {
            (Segment::Link(url), url.len())
        } else {
            // A bare scheme isn't a link.
            (Segment::Text(word), word.len())
        };
        out.push(segment);
        rest = &tail[used..];
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest));
    }
    out
}

fn next_link_start(s: &str) -> Option<usize> {
    SCHEMES.iter().filter_map(|scheme| s.find(scheme)).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_quote_runs_collapse_and_short_ones_stay() {
        let text = "Hi,\n> one\n> two\n> three\nReply\n> just one\nBye";
        let b = blocks(text);
        assert_eq!(
            b,
            vec![
                Block::Line("Hi,".into()),
                Block::Quote(vec!["one".into(), "two".into(), "three".into()]),
                Block::Line("Reply".into()),
                Block::Line("> just one".into()),
                Block::Line("Bye".into()),
            ]
        );
    }

    #[test]
    fn nested_quote_markers_are_removed() {
        let b = blocks(">> a\n> > b\n>  c\n");
        assert_eq!(
            b,
            vec![Block::Quote(vec!["a".into(), "b".into(), "c".into()])]
        );
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(
            blocks("a\n\nb"),
            vec![
                Block::Line("a".into()),
                Block::Line(String::new()),
                Block::Line("b".into()),
            ]
        );
    }

    #[test]
    fn links_are_split_out_of_text() {
        assert_eq!(
            split_links("see https://example.com/a?b=1 now"),
            vec![
                Segment::Text("see "),
                Segment::Link("https://example.com/a?b=1"),
                Segment::Text(" now"),
            ]
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_link() {
        assert_eq!(
            split_links("Go to http://example.org."),
            vec![
                Segment::Text("Go to "),
                Segment::Link("http://example.org"),
                Segment::Text(".")
            ]
        );
    }

    #[test]
    fn angle_bracketed_and_multiple_links() {
        assert_eq!(
            split_links("<https://a.example> and https://b.example/x"),
            vec![
                Segment::Text("<"),
                Segment::Link("https://a.example"),
                Segment::Text("> and "),
                Segment::Link("https://b.example/x"),
            ]
        );
    }

    #[test]
    fn bare_scheme_is_text() {
        let segs = split_links("https:// nothing");
        assert!(
            segs.iter().all(|s| matches!(s, Segment::Text(_))),
            "{segs:?}"
        );
    }

    #[test]
    fn line_without_links_is_one_text_segment() {
        assert_eq!(split_links("plain line"), vec![Segment::Text("plain line")]);
        assert!(split_links("").is_empty());
    }
}
