use bowline_core::{
    ids::{ContentId, ContentLayoutId, NamespacePageId},
    namespace_snapshot::NamespaceReadError,
    policy::{AccessFlag, MaterializationMode, PathClassification},
    workspace_graph::{
        ContentLayout, FileExecutability, HydrationState, NamespaceEntry, NamespaceEntryKind,
    },
};

use super::types::{
    MetadataIdentityKey, NAMESPACE_PAGE_FORMAT_VERSION, NAMESPACE_PAGE_MAX_BYTES,
    NAMESPACE_PAGE_MIN_BYTES,
};

const NAMESPACE_PAGE_MAGIC: &[u8; 4] = b"BWNP";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamespaceEntryValue {
    pub kind: NamespaceEntryKind,
    pub classification: PathClassification,
    pub mode: MaterializationMode,
    pub access: Vec<AccessFlag>,
    pub content_id: Option<ContentId>,
    pub content_layout_id: Option<ContentLayoutId>,
    pub symlink_target: Option<String>,
    pub byte_len: Option<u64>,
    pub executability: FileExecutability,
    pub hydration_state: HydrationState,
}

impl NamespaceEntryValue {
    pub(crate) fn from_entry(entry: &NamespaceEntry, layout_id: Option<ContentLayoutId>) -> Self {
        Self {
            kind: entry.kind,
            classification: entry.classification,
            mode: entry.mode,
            access: entry.access.clone(),
            content_id: entry.content_id.clone(),
            content_layout_id: layout_id,
            symlink_target: entry.symlink_target.clone(),
            byte_len: entry.byte_len,
            executability: entry.executability,
            hydration_state: entry.hydration_state,
        }
    }

    pub(crate) fn into_entry(self, path: String, layout: Option<ContentLayout>) -> NamespaceEntry {
        NamespaceEntry {
            path,
            kind: self.kind,
            classification: self.classification,
            mode: self.mode,
            access: self.access,
            content_id: self.content_id,
            content_layout: layout,
            symlink_target: self.symlink_target,
            byte_len: self.byte_len,
            executability: self.executability,
            hydration_state: self.hydration_state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NamespacePage {
    Leaf {
        common_prefix: Vec<u8>,
        entries: Vec<(Vec<u8>, NamespaceEntryValue)>,
    },
    Branch {
        common_prefix: Vec<u8>,
        children: Vec<(u8, NamespacePageId)>,
        value: Option<NamespaceEntryValue>,
    },
}

pub(crate) fn encode_namespace_page(page: &NamespacePage) -> Result<Vec<u8>, NamespaceReadError> {
    let mut encoder = Encoder::new(NAMESPACE_PAGE_MAGIC, NAMESPACE_PAGE_FORMAT_VERSION);
    match page {
        NamespacePage::Leaf {
            common_prefix,
            entries,
        } => {
            encoder.u8(0);
            encoder.bytes(common_prefix)?;
            encoder.len(entries.len())?;
            let mut previous: Option<&[u8]> = None;
            for (suffix, value) in entries {
                if previous.is_some_and(|prior| prior >= suffix.as_slice()) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "leaf entry suffix",
                    });
                }
                encoder.bytes(suffix)?;
                encode_entry_value(&mut encoder, value)?;
                previous = Some(suffix);
            }
        }
        NamespacePage::Branch {
            common_prefix,
            children,
            value,
        } => {
            encoder.u8(1);
            encoder.bytes(common_prefix)?;
            encoder.option(value.as_ref(), encode_entry_value)?;
            encoder.len(children.len())?;
            let mut previous = None;
            for (edge, page_id) in children {
                if previous.is_some_and(|prior| prior >= *edge) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "branch child edge",
                    });
                }
                encoder.u8(*edge);
                encoder.logical_id(page_id.as_str(), "nsp")?;
                previous = Some(*edge);
            }
        }
    }
    encoder.pad_to_minimum(NAMESPACE_PAGE_MIN_BYTES)?;
    encoder.finish("namespace page", NAMESPACE_PAGE_MAX_BYTES)
}

pub(crate) fn decode_namespace_page(bytes: &[u8]) -> Result<NamespacePage, NamespaceReadError> {
    if bytes.len() > NAMESPACE_PAGE_MAX_BYTES {
        return Err(NamespaceReadError::OversizedRecord {
            record: "namespace page",
            encoded_bytes: bytes.len() as u64,
            maximum_bytes: NAMESPACE_PAGE_MAX_BYTES as u64,
        });
    }
    let mut decoder = Decoder::new(
        bytes,
        NAMESPACE_PAGE_MAGIC,
        "namespace page",
        NAMESPACE_PAGE_FORMAT_VERSION,
    )?;
    let page = match decoder.u8()? {
        0 => {
            let common_prefix = decoder.bytes()?.to_vec();
            let count = decoder.len()?;
            let mut entries = Vec::with_capacity(count);
            let mut previous: Option<Vec<u8>> = None;
            for _ in 0..count {
                let suffix = decoder.bytes()?.to_vec();
                if previous.as_ref().is_some_and(|prior| prior >= &suffix) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "leaf entry suffix",
                    });
                }
                let value = decode_entry_value(&mut decoder)?;
                previous = Some(suffix.clone());
                entries.push((suffix, value));
            }
            validate_leaf_prefix_normal_form(&common_prefix, &entries)?;
            NamespacePage::Leaf {
                common_prefix,
                entries,
            }
        }
        1 => {
            let common_prefix = decoder.bytes()?.to_vec();
            let value = decoder.option(decode_entry_value)?;
            let count = decoder.len()?;
            let mut children = Vec::with_capacity(count);
            let mut previous = None;
            for _ in 0..count {
                let edge = decoder.u8()?;
                if previous.is_some_and(|prior| prior >= edge) {
                    return Err(NamespaceReadError::NonCanonicalOrder {
                        field: "branch child edge",
                    });
                }
                let page_id = NamespacePageId::new(decoder.logical_id("nsp")?);
                previous = Some(edge);
                children.push((edge, page_id));
            }
            if children.is_empty() || (children.len() == 1 && value.is_none()) {
                return Err(NamespaceReadError::CorruptGraph {
                    reason: "non-canonical branch shape",
                });
            }
            NamespacePage::Branch {
                common_prefix,
                children,
                value,
            }
        }
        _ => {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "unknown namespace page discriminant",
            });
        }
    };
    decoder.canonical_padding()?;
    decoder.finish()?;
    if encode_namespace_page(&page)?.as_slice() != bytes {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "non-canonical namespace page encoding",
        });
    }
    Ok(page)
}

fn validate_leaf_prefix_normal_form(
    common_prefix: &[u8],
    entries: &[(Vec<u8>, NamespaceEntryValue)],
) -> Result<(), NamespaceReadError> {
    if entries.is_empty() {
        if common_prefix.is_empty() {
            return Ok(());
        }
        return Err(NamespaceReadError::CorruptGraph {
            reason: "non-canonical empty namespace leaf prefix",
        });
    }
    let first = entries[0].0.as_slice();
    let shared_suffix_bytes = entries
        .iter()
        .skip(1)
        .fold(first.len(), |shared, (suffix, _)| {
            first[..shared]
                .iter()
                .zip(suffix)
                .take_while(|(left, right)| left == right)
                .count()
        });
    if shared_suffix_bytes != 0 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "namespace leaf prefix is not maximally compressed",
        });
    }
    Ok(())
}

fn encode_entry_value(
    encoder: &mut Encoder,
    value: &NamespaceEntryValue,
) -> Result<(), NamespaceReadError> {
    encoder.u8(kind_tag(value.kind));
    encoder.u8(classification_tag(value.classification));
    encoder.u8(mode_tag(value.mode));
    let mut previous = None;
    for flag in &value.access {
        let tag = access_tag(*flag);
        if previous.is_some_and(|prior| prior >= tag) {
            return Err(NamespaceReadError::NonCanonicalOrder {
                field: "entry access flags",
            });
        }
        previous = Some(tag);
    }
    encoder.len(value.access.len())?;
    for flag in &value.access {
        encoder.u8(access_tag(*flag));
    }
    encoder.option(value.content_id.as_ref(), |encoder, id| {
        encoder.string(id.as_str())
    })?;
    encoder.option(value.content_layout_id.as_ref(), |encoder, id| {
        encoder.logical_id(id.as_str(), "ctl")
    })?;
    encoder.option(value.symlink_target.as_ref(), |encoder, target| {
        encoder.string(target)
    })?;
    encoder.option(value.byte_len.as_ref(), |encoder, length| {
        encoder.u64(*length);
        Ok(())
    })?;
    encoder.u8(executability_tag(value.executability));
    encoder.u8(hydration_tag(value.hydration_state));
    Ok(())
}

fn decode_entry_value(
    decoder: &mut Decoder<'_>,
) -> Result<NamespaceEntryValue, NamespaceReadError> {
    let kind = kind_from_tag(decoder.u8()?)?;
    let classification = classification_from_tag(decoder.u8()?)?;
    let mode = mode_from_tag(decoder.u8()?)?;
    let access_count = decoder.len()?;
    let mut access = Vec::with_capacity(access_count);
    let mut previous = None;
    for _ in 0..access_count {
        let tag = decoder.u8()?;
        if previous.is_some_and(|prior| prior >= tag) {
            return Err(NamespaceReadError::NonCanonicalOrder {
                field: "entry access flags",
            });
        }
        access.push(access_from_tag(tag)?);
        previous = Some(tag);
    }
    let content_id = decoder.option(|decoder| Ok(ContentId::new(decoder.string()?)))?;
    let content_layout_id =
        decoder.option(|decoder| Ok(ContentLayoutId::new(decoder.logical_id("ctl")?)))?;
    let symlink_target = decoder.option(|decoder| decoder.string())?;
    let byte_len = decoder.option(|decoder| decoder.u64())?;
    let executability = executability_from_tag(decoder.u8()?)?;
    let hydration_state = hydration_from_tag(decoder.u8()?)?;
    Ok(NamespaceEntryValue {
        kind,
        classification,
        mode,
        access,
        content_id,
        content_layout_id,
        symlink_target,
        byte_len,
        executability,
        hydration_state,
    })
}

pub(crate) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(crate) fn new(magic: &[u8; 4], version: u16) -> Self {
        let mut bytes = Vec::with_capacity(256);
        bytes.extend_from_slice(magic);
        bytes.extend_from_slice(&version.to_be_bytes());
        Self { bytes }
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn len(&mut self, value: usize) -> Result<(), NamespaceReadError> {
        let value = u32::try_from(value).map_err(|_| NamespaceReadError::OversizedRecord {
            record: "canonical collection",
            encoded_bytes: u64::MAX,
            maximum_bytes: u32::MAX as u64,
        })?;
        self.u32(value);
        Ok(())
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) -> Result<(), NamespaceReadError> {
        self.len(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(crate) fn string(&mut self, value: &str) -> Result<(), NamespaceReadError> {
        self.bytes(value.as_bytes())
    }

    pub(crate) fn option<T>(
        &mut self,
        value: Option<T>,
        encode: impl FnOnce(&mut Self, T) -> Result<(), NamespaceReadError>,
    ) -> Result<(), NamespaceReadError> {
        match value {
            Some(value) => {
                self.u8(1);
                encode(self, value)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }

    pub(crate) fn logical_id(
        &mut self,
        value: &str,
        prefix: &'static str,
    ) -> Result<(), NamespaceReadError> {
        let digest = logical_id_digest(value, prefix)?;
        self.bytes.extend_from_slice(&digest);
        Ok(())
    }

    pub(crate) fn finish(
        self,
        record: &'static str,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, NamespaceReadError> {
        if self.bytes.len() > maximum_bytes {
            return Err(NamespaceReadError::OversizedRecord {
                record,
                encoded_bytes: self.bytes.len() as u64,
                maximum_bytes: maximum_bytes as u64,
            });
        }
        Ok(self.bytes)
    }

    pub(crate) fn pad_to_minimum(
        &mut self,
        minimum_bytes: usize,
    ) -> Result<(), NamespaceReadError> {
        let padding = minimum_bytes.saturating_sub(self.bytes.len().saturating_add(4));
        self.len(padding)?;
        self.bytes
            .resize(self.bytes.len().saturating_add(padding), 0);
        Ok(())
    }
}

pub(crate) struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(
        bytes: &'a [u8],
        magic: &[u8; 4],
        record: &'static str,
        expected_version: u16,
    ) -> Result<Self, NamespaceReadError> {
        if bytes.len() < 6 || &bytes[..4] != magic {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "invalid canonical record magic",
            });
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != expected_version {
            return Err(NamespaceReadError::UnsupportedFormat { record, version });
        }
        Ok(Self { bytes, offset: 6 })
    }

    pub(crate) fn u8(&mut self) -> Result<u8, NamespaceReadError> {
        let value = *self
            .take(1)?
            .first()
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "truncated canonical integer",
            })?;
        Ok(value)
    }

    pub(crate) fn u16(&mut self) -> Result<u16, NamespaceReadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, NamespaceReadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, NamespaceReadError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    pub(crate) fn len(&mut self) -> Result<usize, NamespaceReadError> {
        let len = usize::try_from(self.u32()?).map_err(|_| NamespaceReadError::CorruptGraph {
            reason: "canonical length does not fit this platform",
        })?;
        if len > self.bytes.len().saturating_sub(self.offset) {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "canonical collection length exceeds remaining record bytes",
            });
        }
        Ok(len)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], NamespaceReadError> {
        let len = self.len()?;
        self.take(len)
    }

    pub(crate) fn string(&mut self) -> Result<String, NamespaceReadError> {
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes).map(str::to_string).map_err(|_| {
            NamespaceReadError::CorruptGraph {
                reason: "canonical string is not UTF-8",
            }
        })
    }

    pub(crate) fn option<T>(
        &mut self,
        decode: impl FnOnce(&mut Self) -> Result<T, NamespaceReadError>,
    ) -> Result<Option<T>, NamespaceReadError> {
        match self.u8()? {
            0 => Ok(None),
            1 => decode(self).map(Some),
            _ => Err(NamespaceReadError::CorruptGraph {
                reason: "invalid canonical option discriminant",
            }),
        }
    }

    pub(crate) fn logical_id(&mut self, prefix: &str) -> Result<String, NamespaceReadError> {
        let digest = self.take(32)?;
        Ok(format!("{prefix}_{}", encode_hex(digest)))
    }

    pub(crate) fn finish(self) -> Result<(), NamespaceReadError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(NamespaceReadError::CorruptGraph {
                reason: "trailing bytes in canonical record",
            })
        }
    }

    pub(crate) fn canonical_padding(&mut self) -> Result<(), NamespaceReadError> {
        let padding = self.bytes()?;
        if padding.iter().any(|byte| *byte != 0) {
            return Err(NamespaceReadError::CorruptGraph {
                reason: "namespace page padding is non-zero",
            });
        }
        Ok(())
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], NamespaceReadError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "canonical record length overflow",
            })?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(NamespaceReadError::CorruptGraph {
                reason: "truncated canonical record",
            })?;
        self.offset = end;
        Ok(value)
    }
}

pub(crate) fn logical_id(prefix: &str, key: MetadataIdentityKey, bytes: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new_keyed(&key.as_bytes());
    hasher.update(prefix.as_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    format!("{prefix}_{}", hasher.finalize().to_hex())
}

fn logical_id_digest(value: &str, prefix: &'static str) -> Result<[u8; 32], NamespaceReadError> {
    let expected = format!("{prefix}_");
    let Some(hex) = value.strip_prefix(&expected) else {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "logical ID has the wrong record prefix",
        });
    };
    if hex.len() != 64 {
        return Err(NamespaceReadError::CorruptGraph {
            reason: "logical ID digest has the wrong length",
        });
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = (decode_nibble(pair[0])? << 4) | decode_nibble(pair[1])?;
    }
    Ok(digest)
}

fn decode_nibble(value: u8) -> Result<u8, NamespaceReadError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(NamespaceReadError::CorruptGraph {
            reason: "logical ID digest is not lowercase hexadecimal",
        }),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[(byte >> 4) as usize]));
        encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    encoded
}

fn kind_tag(value: NamespaceEntryKind) -> u8 {
    match value {
        NamespaceEntryKind::Directory => 0,
        NamespaceEntryKind::File => 1,
        NamespaceEntryKind::Symlink => 2,
        NamespaceEntryKind::Placeholder => 3,
        NamespaceEntryKind::Tombstone => 4,
    }
}
fn kind_from_tag(value: u8) -> Result<NamespaceEntryKind, NamespaceReadError> {
    match value {
        0 => Ok(NamespaceEntryKind::Directory),
        1 => Ok(NamespaceEntryKind::File),
        2 => Ok(NamespaceEntryKind::Symlink),
        3 => Ok(NamespaceEntryKind::Placeholder),
        4 => Ok(NamespaceEntryKind::Tombstone),
        _ => Err(bad_enum()),
    }
}
fn classification_tag(value: PathClassification) -> u8 {
    match value {
        PathClassification::WorkspaceSync => 0,
        PathClassification::ProjectEnv => 1,
        PathClassification::Generated => 2,
        PathClassification::Dependency => 3,
        PathClassification::Cache => 4,
        PathClassification::LargeFile => 5,
        PathClassification::SecretLooking => 6,
        PathClassification::LocalOnly => 7,
        PathClassification::Blocked => 8,
    }
}
fn classification_from_tag(value: u8) -> Result<PathClassification, NamespaceReadError> {
    match value {
        0 => Ok(PathClassification::WorkspaceSync),
        1 => Ok(PathClassification::ProjectEnv),
        2 => Ok(PathClassification::Generated),
        3 => Ok(PathClassification::Dependency),
        4 => Ok(PathClassification::Cache),
        5 => Ok(PathClassification::LargeFile),
        6 => Ok(PathClassification::SecretLooking),
        7 => Ok(PathClassification::LocalOnly),
        8 => Ok(PathClassification::Blocked),
        _ => Err(bad_enum()),
    }
}
fn mode_tag(value: MaterializationMode) -> u8 {
    match value {
        MaterializationMode::WorkspaceSync => 0,
        MaterializationMode::ProjectEnv => 1,
        MaterializationMode::EncryptedSync => 2,
        MaterializationMode::Lazy => 3,
        MaterializationMode::StructureOnly => 4,
        MaterializationMode::LocalRegenerate => 5,
        MaterializationMode::LocalCache => 6,
        MaterializationMode::Ignore => 7,
        MaterializationMode::LocalOnly => 8,
        MaterializationMode::Blocked => 9,
    }
}
fn mode_from_tag(value: u8) -> Result<MaterializationMode, NamespaceReadError> {
    match value {
        0 => Ok(MaterializationMode::WorkspaceSync),
        1 => Ok(MaterializationMode::ProjectEnv),
        2 => Ok(MaterializationMode::EncryptedSync),
        3 => Ok(MaterializationMode::Lazy),
        4 => Ok(MaterializationMode::StructureOnly),
        5 => Ok(MaterializationMode::LocalRegenerate),
        6 => Ok(MaterializationMode::LocalCache),
        7 => Ok(MaterializationMode::Ignore),
        8 => Ok(MaterializationMode::LocalOnly),
        9 => Ok(MaterializationMode::Blocked),
        _ => Err(bad_enum()),
    }
}
fn access_tag(value: AccessFlag) -> u8 {
    match value {
        AccessFlag::HumanReadable => 0,
        AccessFlag::AgentReadable => 1,
        AccessFlag::AgentHidden => 2,
        AccessFlag::LeaseOnly => 3,
    }
}
fn access_from_tag(value: u8) -> Result<AccessFlag, NamespaceReadError> {
    match value {
        0 => Ok(AccessFlag::HumanReadable),
        1 => Ok(AccessFlag::AgentReadable),
        2 => Ok(AccessFlag::AgentHidden),
        3 => Ok(AccessFlag::LeaseOnly),
        _ => Err(bad_enum()),
    }
}
fn executability_tag(value: FileExecutability) -> u8 {
    match value {
        FileExecutability::Regular => 0,
        FileExecutability::Executable => 1,
    }
}
fn executability_from_tag(value: u8) -> Result<FileExecutability, NamespaceReadError> {
    match value {
        0 => Ok(FileExecutability::Regular),
        1 => Ok(FileExecutability::Executable),
        _ => Err(bad_enum()),
    }
}
fn hydration_tag(value: HydrationState) -> u8 {
    match value {
        HydrationState::Local => 0,
        HydrationState::Cold => 1,
        HydrationState::StructureOnly => 2,
        HydrationState::Missing => 3,
    }
}
fn hydration_from_tag(value: u8) -> Result<HydrationState, NamespaceReadError> {
    match value {
        0 => Ok(HydrationState::Local),
        1 => Ok(HydrationState::Cold),
        2 => Ok(HydrationState::StructureOnly),
        3 => Ok(HydrationState::Missing),
        _ => Err(bad_enum()),
    }
}
fn bad_enum() -> NamespaceReadError {
    NamespaceReadError::CorruptGraph {
        reason: "unknown namespace entry enum discriminant",
    }
}
