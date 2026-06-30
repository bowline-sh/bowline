use std::{collections::BTreeMap, fmt};

#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for SecretBytes {
    fn from(value: &str) -> Self {
        Self::new(value.as_bytes().to_vec())
    }
}

impl From<Vec<u8>> for SecretBytes {
    fn from(value: Vec<u8>) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretBytes")
            .field("len", &self.0.len())
            .field("value", &"[redacted]")
            .finish()
    }
}

impl fmt::Display for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEnvFile {
    pub source_path: String,
    pub profile: String,
    pub lines: Vec<EnvLine>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvLine {
    pub ordinal: usize,
    pub line_number: usize,
    pub raw: Vec<u8>,
    pub ending: String,
    pub kind: EnvLineKind,
}

impl fmt::Debug for EnvLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvLine")
            .field("ordinal", &self.ordinal)
            .field("line_number", &self.line_number)
            .field("raw_len", &self.raw.len())
            .field("ending", &self.ending)
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvLineKind {
    Blank,
    Comment,
    KeyValue(EnvKeyValue),
    Opaque(EnvOpaqueLine),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteStyle {
    Unquoted,
    Single,
    Double,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvKeyValue {
    pub key: String,
    pub occurrence_index: usize,
    pub export_prefix: bool,
    pub quote_style: QuoteStyle,
    pub value: SecretBytes,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
}

impl fmt::Debug for EnvKeyValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvKeyValue")
            .field("key", &self.key)
            .field("occurrence_index", &self.occurrence_index)
            .field("export_prefix", &self.export_prefix)
            .field("quote_style", &self.quote_style)
            .field("value", &self.value)
            .field("prefix_len", &self.prefix.len())
            .field("suffix_len", &self.suffix.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct EnvOpaqueLine {
    pub bytes: SecretBytes,
}

impl fmt::Debug for EnvOpaqueLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvOpaqueLine")
            .field("bytes", &self.bytes)
            .finish()
    }
}

pub fn parse_env_text(
    source_path: impl Into<String>,
    profile: impl Into<String>,
    bytes: &[u8],
) -> ParsedEnvFile {
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut line_number = 1;

    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| start + offset + 1);
        let (content, ending) = split_line_ending(&bytes[start..end]);
        let kind = parse_line(content, &mut occurrences);
        lines.push(EnvLine {
            ordinal: lines.len(),
            line_number,
            raw: content.to_vec(),
            ending: ending.to_string(),
            kind,
        });
        start = end;
        line_number += 1;
    }

    ParsedEnvFile {
        source_path: source_path.into(),
        profile: profile.into(),
        lines,
    }
}

fn split_line_ending(line: &[u8]) -> (&[u8], &str) {
    if line.ends_with(b"\r\n") {
        (&line[..line.len() - 2], "\r\n")
    } else if line.ends_with(b"\n") {
        (&line[..line.len() - 1], "\n")
    } else {
        (line, "")
    }
}

fn parse_line(line: &[u8], occurrences: &mut BTreeMap<String, usize>) -> EnvLineKind {
    let trimmed_start = skip_ascii_whitespace(line, 0);
    if trimmed_start == line.len() {
        return EnvLineKind::Blank;
    }
    if line[trimmed_start] == b'#' {
        return EnvLineKind::Comment;
    }

    match parse_key_value(line) {
        Some(mut value) => {
            let next = occurrences.entry(value.key.clone()).or_default();
            value.occurrence_index = *next;
            *next += 1;
            EnvLineKind::KeyValue(value)
        }
        None => EnvLineKind::Opaque(EnvOpaqueLine {
            bytes: SecretBytes::new(line.to_vec()),
        }),
    }
}

fn parse_key_value(line: &[u8]) -> Option<EnvKeyValue> {
    let mut cursor = skip_ascii_whitespace(line, 0);
    let export_prefix = line[cursor..].starts_with(b"export")
        && line
            .get(cursor + "export".len())
            .is_some_and(|byte| byte.is_ascii_whitespace());
    if export_prefix {
        cursor = skip_ascii_whitespace(line, cursor + "export".len());
    }

    let key_start = cursor;
    let first = *line.get(cursor)?;
    if !is_key_start(first) {
        return None;
    }
    cursor += 1;
    while line.get(cursor).is_some_and(|byte| is_key_continue(*byte)) {
        cursor += 1;
    }
    let key = std::str::from_utf8(&line[key_start..cursor])
        .ok()?
        .to_string();
    cursor = skip_ascii_whitespace(line, cursor);
    if line.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor += 1;
    cursor = skip_ascii_whitespace(line, cursor);

    let (quote_style, value_start, value_end, suffix_start) = match line.get(cursor) {
        Some(b'\'') => quoted_span(line, cursor, b'\'')
            .map(|(end, suffix)| (QuoteStyle::Single, cursor + 1, end, suffix))?,
        Some(b'"') => quoted_span(line, cursor, b'"')
            .map(|(end, suffix)| (QuoteStyle::Double, cursor + 1, end, suffix))?,
        _ => {
            let (end, suffix) = unquoted_span(line, cursor);
            (QuoteStyle::Unquoted, cursor, end, suffix)
        }
    };

    Some(EnvKeyValue {
        key,
        occurrence_index: 0,
        export_prefix,
        quote_style,
        value: SecretBytes::new(line[value_start..value_end].to_vec()),
        prefix: line[..value_start].to_vec(),
        suffix: line[suffix_start..].to_vec(),
    })
}

fn quoted_span(line: &[u8], quote_start: usize, quote: u8) -> Option<(usize, usize)> {
    let mut cursor = quote_start + 1;
    while cursor < line.len() {
        if quote == b'"' && line[cursor] == b'\\' {
            cursor = cursor.saturating_add(2);
            continue;
        }
        if line[cursor] == quote {
            return Some((cursor, cursor));
        }
        cursor += 1;
    }
    None
}

fn unquoted_span(line: &[u8], value_start: usize) -> (usize, usize) {
    let mut cursor = value_start;
    while cursor < line.len() {
        if line[cursor].is_ascii_whitespace()
            && line[cursor..]
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
                == Some(&b'#')
        {
            return (cursor, cursor);
        }
        cursor += 1;
    }
    (line.len(), line.len())
}

fn skip_ascii_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    cursor
}

fn is_key_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_key_continue(byte: u8) -> bool {
    is_key_start(byte) || byte.is_ascii_digit()
}
