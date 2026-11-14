//! Sender authentication results from a message's `Authentication-Results` headers.
//! The receiving server records the SPF, DKIM and DMARC verdicts there.

/// One verdict, such as `spf=pass`, from one clause of the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCheck {
    /// The method, lowercase: `spf`, `dkim`, `dmarc`, `arc`, and so on.
    pub method: String,
    /// The result, lowercase: `pass`, `fail`, `none`, and so on.
    pub result: String,
    /// The clause without its comments, for a tooltip: e.g. `dmarc=pass header.from=example.com`.
    pub detail: String,
}

/// The server whose verdicts are trusted. Gmail adds its own header when it receives a
/// message, above anything the sender wrote.
const TRUSTED_AUTHSERV: &str = "mx.google.com";

/// The checks Gmail recorded in a header block. Only the topmost `Authentication-Results`
/// header counts, and only if Gmail wrote it: a sender can put any number of these headers
/// in a message, so a lower one, or one from another server, proves nothing. Verdicts
/// nested inside an ARC clause aren't returned either.
pub fn checks(headers: &[u8]) -> Vec<AuthCheck> {
    let text = String::from_utf8_lossy(headers);
    let Some(top) = header_values(&text, "authentication-results")
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    let clauses = top_level_clauses(&top);
    let authserv = clauses
        .first()
        .and_then(|c| c.split_whitespace().next())
        .unwrap_or("");
    if !authserv.eq_ignore_ascii_case(TRUSTED_AUTHSERV) {
        return Vec::new();
    }
    clauses.iter().filter_map(|c| parse_clause(c)).collect()
}

/// The values of every header with this name, with folded lines joined.
fn header_values(text: &str, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() {
            break; // the end of the header block
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(value) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some(value) = current.take() {
            values.push(value);
        }
        if let Some((field, value)) = line.split_once(':')
            && field.trim().eq_ignore_ascii_case(name)
        {
            current = Some(value.trim().to_string());
        }
    }
    values.extend(current);
    values
}

/// Splits a header value at its `;` separators, ignoring any inside comments or quotes.
fn top_level_clauses(value: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut current = String::new();
    let (mut depth, mut quoted) = (0u32, false);
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth = depth.saturating_sub(1),
            ';' if depth == 0 && !quoted => {
                clauses.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    clauses.push(current);
    clauses
}

/// Reads `method=result ...` from one clause. Clauses that don't start that way, such as
/// the authserv-id at the front of the header, give `None`.
fn parse_clause(clause: &str) -> Option<AuthCheck> {
    let clause = clause.trim();
    let end = clause
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(clause.len());
    let (method, result) = clause[..end].split_once('=')?;
    if method.is_empty() || result.is_empty() {
        return None;
    }
    Some(AuthCheck {
        method: method.to_ascii_lowercase(),
        result: result.to_ascii_lowercase(),
        detail: without_comments(clause),
    })
}

/// The text with parenthesized comments removed and whitespace tidied.
fn without_comments(clause: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in clause.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOGLE: &str = "Authentication-Results: mx.google.com; dkim=pass header.i=@nextpointgroup.com header.s=selector1 header.b=Npnb1Dz5; arc=pass (i=1 spf=pass spfdomain=nextpointgroup.com dkim=pass dkdomain=nextpointgroup.com dmarc=pass fromdomain=nextpointgroup.com); spf=pass (google.com: domain of smcnaughton@nextpointgroup.com designates 2a01:111:f403:c111::7 as permitted sender) smtp.mailfrom=smcnaughton@nextpointgroup.com; dmarc=pass (p=QUARANTINE sp=QUARANTINE dis=NONE) header.from=nextpointgroup.com\r\nARC-Seal: i=1; a=rsa-sha256; s=arcselector10001; d=microsoft.com; cv=none;\r\n\r\n";

    fn find<'a>(checks: &'a [AuthCheck], method: &str) -> Vec<&'a AuthCheck> {
        checks.iter().filter(|c| c.method == method).collect()
    }

    #[test]
    fn reads_the_message_verdicts_and_not_the_arc_ones() {
        let all = checks(GOOGLE.as_bytes());
        let methods: Vec<&str> = all.iter().map(|c| c.method.as_str()).collect();
        assert_eq!(methods, vec!["dkim", "arc", "spf", "dmarc"]);
        let spf = find(&all, "spf");
        assert_eq!(
            spf.len(),
            1,
            "the spf inside the arc clause is not reported"
        );
        assert_eq!(spf[0].result, "pass");
        assert_eq!(
            spf[0].detail,
            "spf=pass smtp.mailfrom=smcnaughton@nextpointgroup.com"
        );
        let dmarc = find(&all, "dmarc");
        assert_eq!(dmarc[0].result, "pass");
        assert_eq!(dmarc[0].detail, "dmarc=pass header.from=nextpointgroup.com");
        assert_eq!(find(&all, "dkim")[0].result, "pass");
    }

    #[test]
    fn folded_headers_are_joined() {
        let headers = b"From: a@example.com\r\nAuthentication-Results: mx.google.com;\r\n\tspf=fail smtp.mailfrom=x@example.com;\r\n dmarc=none\r\n\r\nBody: spf=pass\r\n";
        let all = checks(headers);
        assert_eq!(find(&all, "spf")[0].result, "fail");
        assert_eq!(find(&all, "dmarc")[0].result, "none");
        assert!(find(&all, "body").is_empty(), "the body is not a header");
    }

    #[test]
    fn a_forged_header_below_gmails_is_ignored() {
        // Gmail's real verdict is a fail. The sender added a header claiming a pass.
        let headers = b"Authentication-Results: mx.google.com; dmarc=fail header.from=bank.example\r\nFrom: Bank <alerts@bank.example>\r\nAuthentication-Results: mx.google.com; dmarc=pass header.from=bank.example\r\n\r\n";
        let all = checks(headers);
        assert_eq!(all.len(), 1, "{all:?}");
        assert_eq!(all[0].result, "fail");
    }

    #[test]
    fn a_header_from_another_server_is_not_trusted() {
        let headers = b"Authentication-Results: mail.attacker.example; spf=pass; dkim=pass; dmarc=pass\r\n\r\n";
        assert!(checks(headers).is_empty());
    }

    #[test]
    fn a_message_without_the_header_has_no_checks() {
        assert!(checks(b"From: a@example.com\r\nSubject: hi\r\n\r\n").is_empty());
        assert!(checks(b"").is_empty());
    }

    #[test]
    fn semicolons_inside_comments_do_not_split_clauses() {
        let headers = b"Authentication-Results: mx.google.com; spf=softfail (no record; see notes) smtp.mailfrom=x\r\n\r\n";
        let all = checks(headers);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].result, "softfail");
    }
}
