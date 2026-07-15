/// POSIX single-quote a single shell word.
///
/// Non-empty words made only from shell-safe characters are returned unquoted.
/// Everything else is wrapped in single quotes, with embedded quotes escaped
/// by closing, escaping, and reopening the quoted span.
pub fn quote_word(value: &str) -> String {
    if shell_safe_word(value) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Join pre-split args into one copy/paste-safe command line.
pub fn quote_command(args: impl IntoIterator<Item = String>) -> String {
    args.into_iter()
        .map(|arg| quote_word(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_safe_word(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '/' | '.' | '_' | '-' | ':' | '=' | '+' | '@' | '%')
        })
}

#[cfg(test)]
mod tests {
    use super::{quote_command, quote_word};

    #[test]
    fn quote_word_uses_canonical_single_quote_form() {
        assert_eq!(quote_word("abc"), "abc");
        assert_eq!(quote_word(""), "''");
        assert_eq!(quote_word("a b"), "'a b'");
        assert_eq!(quote_word("it's"), "'it'\\''s'");
        assert_eq!(quote_word("a=b"), "a=b");
        assert_eq!(quote_word("100%"), "100%");
        assert_eq!(quote_word("/opt/u/x"), "/opt/u/x");
        assert_eq!(quote_word("cafe\u{e9}"), "'cafe\u{e9}'");
    }

    #[test]
    fn quote_command_quotes_each_word() {
        assert_eq!(
            quote_command([
                "bowline".to_string(),
                "device".to_string(),
                "approve".to_string(),
                "--code".to_string(),
                "ab cd".to_string()
            ]),
            "bowline device approve --code 'ab cd'"
        );
    }

    #[test]
    fn quote_word_round_trips_through_shell_word_parser() {
        for value in [
            "",
            "plain",
            "a b",
            "it's",
            "a=b",
            "100%",
            "\"quoted\"",
            "cafe\u{e9}",
            "-leading",
            "back\\slash",
        ] {
            assert_eq!(parse_single_shell_word(&quote_word(value)), value);
        }
    }

    fn parse_single_shell_word(value: &str) -> String {
        let mut parsed = String::new();
        let mut chars = value.chars().peekable();
        let mut in_single = false;
        while let Some(ch) = chars.next() {
            match ch {
                '\'' => in_single = !in_single,
                '\\' if !in_single => {
                    if let Some(next) = chars.next() {
                        parsed.push(next);
                    }
                }
                _ => parsed.push(ch),
            }
        }
        parsed
    }
}
