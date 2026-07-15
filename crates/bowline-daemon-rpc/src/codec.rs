use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
};

use serde::{Serialize, de::DeserializeOwned};

pub const CONNECTION_MAGIC: [u8; 8] = *b"BWLNRPC2";
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecPhase {
    Magic,
    LengthPrefix,
    Payload,
    Serialize,
    Deserialize,
}

#[derive(Debug)]
pub enum CodecError {
    Io {
        phase: CodecPhase,
        source: io::Error,
    },
    CleanEof,
    UnexpectedEof {
        phase: CodecPhase,
        expected: usize,
        received: usize,
    },
    InvalidMagic {
        received: [u8; 8],
    },
    FrameTooLarge {
        declared: usize,
        maximum: usize,
    },
    Serialize(serde_json::Error),
    MalformedJson(serde_json::Error),
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { phase, source } => write!(formatter, "RPC {phase:?} I/O failed: {source}"),
            Self::CleanEof => formatter.write_str("RPC peer closed the connection"),
            Self::UnexpectedEof {
                phase,
                expected,
                received,
            } => write!(
                formatter,
                "RPC peer closed during {phase:?}: expected {expected} bytes, received {received}",
            ),
            Self::InvalidMagic { received } => write!(
                formatter,
                "RPC connection magic is invalid: {:?}",
                String::from_utf8_lossy(received),
            ),
            Self::FrameTooLarge { declared, maximum } => write!(
                formatter,
                "RPC frame declares {declared} bytes, exceeding the {maximum}-byte limit",
            ),
            Self::Serialize(source) => {
                write!(formatter, "RPC frame serialization failed: {source}")
            }
            Self::MalformedJson(source) => {
                write!(formatter, "RPC frame JSON is malformed: {source}")
            }
        }
    }
}

impl Error for CodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) | Self::MalformedJson(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCodec {
    max_frame_bytes: usize,
}

impl FrameCodec {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self { max_frame_bytes }
    }

    pub fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    pub fn write_magic<W: Write>(self, writer: &mut W) -> Result<(), CodecError> {
        write_all(writer, &CONNECTION_MAGIC, CodecPhase::Magic)
    }

    pub fn read_magic<R: Read>(self, reader: &mut R) -> Result<(), CodecError> {
        let mut received = [0_u8; CONNECTION_MAGIC.len()];
        read_exact_bounded(reader, &mut received, CodecPhase::Magic, false)?;
        if received != CONNECTION_MAGIC {
            return Err(CodecError::InvalidMagic { received });
        }
        Ok(())
    }

    pub fn write<T: Serialize, W: Write>(
        self,
        writer: &mut W,
        value: &T,
    ) -> Result<(), CodecError> {
        let payload = serde_json::to_vec(value).map_err(CodecError::Serialize)?;
        if payload.len() > self.max_frame_bytes || payload.len() > u32::MAX as usize {
            return Err(CodecError::FrameTooLarge {
                declared: payload.len(),
                maximum: self.max_frame_bytes.min(u32::MAX as usize),
            });
        }
        let length = (payload.len() as u32).to_be_bytes();
        write_all(writer, &length, CodecPhase::LengthPrefix)?;
        write_all(writer, &payload, CodecPhase::Payload)
    }

    pub fn read<T: DeserializeOwned, R: Read>(self, reader: &mut R) -> Result<T, CodecError> {
        let payload = self.read_payload(reader)?;
        serde_json::from_slice(&payload).map_err(CodecError::MalformedJson)
    }

    pub fn read_payload<R: Read>(self, reader: &mut R) -> Result<Vec<u8>, CodecError> {
        let mut prefix = [0_u8; 4];
        read_exact_bounded(reader, &mut prefix, CodecPhase::LengthPrefix, true)?;
        let declared = u32::from_be_bytes(prefix) as usize;
        if declared > self.max_frame_bytes {
            return Err(CodecError::FrameTooLarge {
                declared,
                maximum: self.max_frame_bytes,
            });
        }
        let mut payload = vec![0_u8; declared];
        read_exact_bounded(reader, &mut payload, CodecPhase::Payload, false)?;
        Ok(payload)
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

#[derive(Debug)]
pub struct IncrementalFrameDecoder {
    max_frame_bytes: usize,
    buffer: Vec<u8>,
}

impl IncrementalFrameDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes,
            buffer: Vec::new(),
        }
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, CodecError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(prefix) = self.buffer.get(..4) {
            let declared =
                u32::from_be_bytes(prefix.try_into().map_err(|_| CodecError::UnexpectedEof {
                    phase: CodecPhase::LengthPrefix,
                    expected: 4,
                    received: prefix.len(),
                })?) as usize;
            if declared > self.max_frame_bytes {
                return Err(CodecError::FrameTooLarge {
                    declared,
                    maximum: self.max_frame_bytes,
                });
            }
            let frame_length = 4_usize.saturating_add(declared);
            if self.buffer.len() < frame_length {
                break;
            }
            let frame = self.buffer.drain(..frame_length).skip(4).collect();
            frames.push(frame);
        }
        Ok(frames)
    }

    pub fn finish(&self) -> Result<(), CodecError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.buffer.len() < 4 {
            return Err(CodecError::UnexpectedEof {
                phase: CodecPhase::LengthPrefix,
                expected: 4,
                received: self.buffer.len(),
            });
        }
        let declared = u32::from_be_bytes(self.buffer[..4].try_into().map_err(|_| {
            CodecError::UnexpectedEof {
                phase: CodecPhase::LengthPrefix,
                expected: 4,
                received: self.buffer.len(),
            }
        })?) as usize;
        Err(CodecError::UnexpectedEof {
            phase: CodecPhase::Payload,
            expected: declared,
            received: self.buffer.len().saturating_sub(4),
        })
    }
}

fn write_all<W: Write>(writer: &mut W, bytes: &[u8], phase: CodecPhase) -> Result<(), CodecError> {
    writer
        .write_all(bytes)
        .map_err(|source| CodecError::Io { phase, source })
}

fn read_exact_bounded<R: Read>(
    reader: &mut R,
    output: &mut [u8],
    phase: CodecPhase,
    clean_eof_at_start: bool,
) -> Result<(), CodecError> {
    let mut received = 0;
    while received < output.len() {
        match reader.read(&mut output[received..]) {
            Ok(0) if received == 0 && clean_eof_at_start => return Err(CodecError::CleanEof),
            Ok(0) => {
                return Err(CodecError::UnexpectedEof {
                    phase,
                    expected: output.len(),
                    received,
                });
            }
            Ok(count) => received += count,
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => return Err(CodecError::Io { phase, source }),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Example {
        value: String,
    }

    struct FragmentedReader {
        inner: Cursor<Vec<u8>>,
        fragment_size: usize,
    }

    #[derive(Default)]
    struct FragmentedWriter {
        bytes: Vec<u8>,
        fragment_size: usize,
    }

    impl Write for FragmentedWriter {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let count = input.len().min(self.fragment_size);
            self.bytes.extend_from_slice(&input[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for FragmentedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let limit = output.len().min(self.fragment_size);
            self.inner.read(&mut output[..limit])
        }
    }

    #[test]
    fn fragmented_prefix_and_payload_round_trip() {
        let codec = FrameCodec::new(1024);
        let expected = Example {
            value: "fragmented".to_string(),
        };
        let mut bytes = Vec::new();
        codec.write(&mut bytes, &expected).expect("frame writes");
        let mut reader = FragmentedReader {
            inner: Cursor::new(bytes),
            fragment_size: 1,
        };
        let actual: Example = codec.read(&mut reader).expect("frame reads");
        assert_eq!(actual, expected);
    }

    #[test]
    fn incremental_decoder_retains_partial_frames_across_idle_boundaries() {
        let codec = FrameCodec::new(1024);
        let expected = Example {
            value: "fragmented".to_string(),
        };
        let mut encoded = Vec::new();
        codec.write(&mut encoded, &expected).expect("frame writes");
        let mut decoder = IncrementalFrameDecoder::new(1024);

        assert!(
            decoder
                .append(&encoded[..2])
                .expect("prefix fragment")
                .is_empty()
        );
        assert!(
            decoder
                .append(&encoded[2..7])
                .expect("payload fragment")
                .is_empty()
        );
        let frames = decoder.append(&encoded[7..]).expect("remaining payload");

        assert_eq!(frames.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Example>(&frames[0]).expect("payload decodes"),
            expected
        );
        decoder.finish().expect("no trailing partial frame");
    }

    #[test]
    fn fragmented_writes_emit_one_complete_frame() {
        let codec = FrameCodec::new(1024);
        let expected = Example {
            value: "fragmented-write".to_string(),
        };
        let mut writer = FragmentedWriter {
            bytes: Vec::new(),
            fragment_size: 2,
        };
        codec.write(&mut writer, &expected).expect("frame writes");
        let actual: Example = codec
            .read(&mut Cursor::new(writer.bytes))
            .expect("emitted frame reads");
        assert_eq!(actual, expected);
    }

    #[test]
    fn multiple_frames_are_read_independently() {
        let codec = FrameCodec::new(1024);
        let mut bytes = Vec::new();
        codec
            .write(
                &mut bytes,
                &Example {
                    value: "first".to_string(),
                },
            )
            .expect("first frame");
        codec
            .write(
                &mut bytes,
                &Example {
                    value: "second".to_string(),
                },
            )
            .expect("second frame");
        let mut reader = Cursor::new(bytes);
        let first: Example = codec.read(&mut reader).expect("first reads");
        let second: Example = codec.read(&mut reader).expect("second reads");
        assert_eq!(first.value, "first");
        assert_eq!(second.value, "second");
    }

    #[test]
    fn oversize_length_is_rejected_before_payload_read() {
        let codec = FrameCodec::new(16);
        let mut reader = Cursor::new((17_u32).to_be_bytes().to_vec());
        assert!(matches!(
            codec.read_payload(&mut reader),
            Err(CodecError::FrameTooLarge {
                declared: 17,
                maximum: 16
            })
        ));
        assert_eq!(reader.position(), 4);
    }

    #[test]
    fn malformed_json_is_structured() {
        let codec = FrameCodec::new(1024);
        let mut bytes = (1_u32).to_be_bytes().to_vec();
        bytes.push(b'{');
        assert!(matches!(
            codec.read::<Example, _>(&mut Cursor::new(bytes)),
            Err(CodecError::MalformedJson(_))
        ));
    }

    #[test]
    fn clean_and_partial_eof_are_distinct() {
        let codec = FrameCodec::new(1024);
        assert!(matches!(
            codec.read_payload(&mut Cursor::new(Vec::new())),
            Err(CodecError::CleanEof)
        ));
        assert!(matches!(
            codec.read_payload(&mut Cursor::new(vec![0, 0])),
            Err(CodecError::UnexpectedEof {
                phase: CodecPhase::LengthPrefix,
                expected: 4,
                received: 2
            })
        ));
        let mut partial_payload = (4_u32).to_be_bytes().to_vec();
        partial_payload.extend_from_slice(b"ab");
        assert!(matches!(
            codec.read_payload(&mut Cursor::new(partial_payload)),
            Err(CodecError::UnexpectedEof {
                phase: CodecPhase::Payload,
                expected: 4,
                received: 2
            })
        ));
    }

    #[test]
    fn connection_magic_is_exact() {
        let codec = FrameCodec::default();
        let mut bytes = Vec::new();
        codec.write_magic(&mut bytes).expect("magic writes");
        codec
            .read_magic(&mut Cursor::new(bytes))
            .expect("magic reads");
        assert!(matches!(
            codec.read_magic(&mut Cursor::new(*b"BWLNRPC1")),
            Err(CodecError::InvalidMagic { .. })
        ));
    }
}
