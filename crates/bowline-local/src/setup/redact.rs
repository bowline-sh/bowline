#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedSetupText {
    pub text: String,
    pub rules: Vec<String>,
}

pub fn redact_setup_text(text: &str) -> RedactedSetupText {
    let mut rules = Vec::new();
    let mut redacted = redact_env_assignments(text, &mut rules);
    redacted = redact_token_fragments(&redacted, &mut rules);
    redacted = redact_home_paths(&redacted, &mut rules);
    normalize_rules(&mut rules);

    RedactedSetupText {
        text: redacted,
        rules,
    }
}

pub fn redact_setup_text_with_values(text: &str, values: &[String]) -> RedactedSetupText {
    let mut known_value_rules = Vec::new();
    let mut text = text.to_string();
    for value in values {
        if !value.is_empty() && text.contains(value) {
            text = text.replace(value, "[redacted]");
            known_value_rules.push("known-env-values".to_string());
        }
    }
    let mut redacted = redact_setup_text(&text);
    redacted.rules.extend(known_value_rules);
    normalize_rules(&mut redacted.rules);
    redacted
}

fn normalize_rules(rules: &mut Vec<String>) {
    rules.sort();
    rules.dedup();
}

fn redact_env_assignments(text: &str, rules: &mut Vec<String>) -> String {
    text.split_whitespace()
        .map(|part| redact_assignment_part(part, rules))
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_assignment_part(part: &str, rules: &mut Vec<String>) -> String {
    let Some(index) = part.find('=') else {
        return part.to_string();
    };
    let raw_key = &part[..index];
    let key_start = raw_key
        .char_indices()
        .find(|(_, character)| character.is_ascii_alphabetic() || *character == '_')
        .map(|(index, _)| index);
    let Some(key_start) = key_start else {
        return part.to_string();
    };
    let key_prefix = &raw_key[..key_start];
    let key = &raw_key[key_start..];
    if !is_env_key(key) {
        return part.to_string();
    }

    let value = &part[index + 1..];
    let value_suffix = trailing_shell_punctuation(value);
    rules.push("env-assignments".to_string());
    format!("{key_prefix}{key}=[redacted]{value_suffix}")
}

fn redact_token_fragments(text: &str, rules: &mut Vec<String>) -> String {
    text.split_whitespace()
        .map(|part| {
            let trimmed = part.trim_matches(|character: char| {
                matches!(character, '"' | '\'' | ',' | ';' | ')' | ']' | '}')
            });
            if looks_like_token(trimmed) {
                rules.push("token-looking-values".to_string());
                part.replace(trimmed, "[redacted]")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_home_paths(text: &str, rules: &mut Vec<String>) -> String {
    let redacted = replace_home_marker(text, "/Users/");
    let redacted = replace_home_marker(&redacted, "/home/");
    let redacted = replace_home_marker(&redacted, "C:\\Users\\");
    let redacted = replace_normalized_home_prefix(&redacted, "Users/");
    let redacted = replace_normalized_home_prefix(&redacted, "home/");

    if redacted != text {
        rules.push("home-paths".to_string());
    }
    redacted
}

fn replace_home_marker(text: &str, marker: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(index) = remaining.find(marker) {
        output.push_str(&remaining[..index]);
        output.push('~');

        let after_marker = &remaining[index + marker.len()..];
        remaining = after_user_segment(after_marker);
    }

    output.push_str(remaining);
    output
}

fn replace_normalized_home_prefix(text: &str, marker: &str) -> String {
    let Some(after_marker) = text.strip_prefix(marker) else {
        return text.to_string();
    };

    format!("~{}", after_user_segment(after_marker))
}

fn after_user_segment(path: &str) -> &str {
    path.find(['/', '\\']).map_or("", |index| &path[index..])
}

fn is_env_key(key: &str) -> bool {
    let Some(first) = key.chars().next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && key.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

fn trailing_shell_punctuation(value: &str) -> &str {
    let suffix_start = value
        .char_indices()
        .rev()
        .find(|(_, character)| !is_shell_punctuation(*character))
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    &value[suffix_start..]
}

fn is_shell_punctuation(character: char) -> bool {
    matches!(character, '"' | '\'' | ',' | ';' | ')' | ']' | '}')
}

fn looks_like_token(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    let lower = text.to_ascii_lowercase();
    text.len() >= 12
        && (upper.starts_with("SK-")
            || upper.starts_with("TOKEN_")
            || upper.starts_with("SECRET_")
            || upper.starts_with("AKIA")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("xoxb-")
            || lower.starts_with("xoxp-")
            || looks_like_jwt(text))
}

fn looks_like_jwt(text: &str) -> bool {
    let token = text.trim_matches(|character: char| {
        matches!(character, '"' | '\'' | ',' | ';' | ')' | ']' | '}')
    });
    if token.len() < 20 || !token.starts_with("eyJ") {
        return false;
    }

    let segments = token.split('.').collect::<Vec<_>>();
    segments.len() == 3
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.chars().all(|character| {
                    character.is_ascii_alphanumeric() || character == '-' || character == '_'
                })
        })
}
