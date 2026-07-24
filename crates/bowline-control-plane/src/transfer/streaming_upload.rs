use std::{
    io::{self, BufRead, BufReader, Read},
    sync::{Arc, Mutex},
};

use bowline_storage::{ByteStoreError, ObjectKey, ReopenableObjectSource, TransferOperation};
use reqwest::blocking::{Body, Client};

use super::map_http_error;

pub(super) struct StreamingPutRequest<'a> {
    pub key: &'a ObjectKey,
    pub source: &'a dyn ReopenableObjectSource,
    pub byte_len: u64,
    pub expected_hash: &'a str,
    pub checksum_sha256: &'a str,
    pub create_only: bool,
}

pub(super) fn send_streaming_put(
    http: &Client,
    url: &str,
    key: &ObjectKey,
    source: &dyn ReopenableObjectSource,
    byte_len: u64,
    expected_hash: &str,
    checksum_sha256: &str,
) -> Result<reqwest::blocking::Response, ByteStoreError> {
    send_streaming_put_with_create_only(
        http,
        url,
        StreamingPutRequest {
            key,
            source,
            byte_len,
            expected_hash,
            checksum_sha256,
            create_only: true,
        },
    )
}

/// When `create_only` is false, omits `If-None-Match: *` so a mismatched
/// pre-existing R2 object (re-sealed ciphertext or greenfield residue) can be
/// overwritten after create-only put returned 412.
pub(super) fn send_streaming_put_with_create_only(
    http: &Client,
    url: &str,
    upload: StreamingPutRequest<'_>,
) -> Result<reqwest::blocking::Response, ByteStoreError> {
    let observed = Arc::new(Mutex::new(ObservedUpload::default()));
    let reader = HashingReader {
        inner: upload.source.open()?,
        observed: observed.clone(),
    };
    let mut request = http
        .put(url)
        .header(reqwest::header::CONTENT_LENGTH, upload.byte_len)
        .header("x-amz-checksum-sha256", upload.checksum_sha256)
        .body(Body::sized(reader, upload.byte_len));
    if upload.create_only {
        request = request.header(reqwest::header::IF_NONE_MATCH, "*");
    }
    let response = request
        .send()
        .map_err(|error| map_http_error(TransferOperation::Upload, error))?;
    if !response.status().is_success() {
        return Ok(response);
    }
    let observed = observed.lock().map_err(|_| ByteStoreError::CorruptObject {
        key: upload.key.clone(),
        reason: "streamed upload identity state was unavailable",
    })?;
    let observed_hash = format!("b3_{}", observed.hasher.clone().finalize().to_hex());
    if observed.byte_len != upload.byte_len || observed_hash != upload.expected_hash {
        return Err(ByteStoreError::CorruptObject {
            key: upload.key.clone(),
            reason: "streamed upload bytes did not match immutable identity",
        });
    }
    Ok(response)
}

#[derive(Default)]
struct ObservedUpload {
    byte_len: u64,
    hasher: blake3::Hasher,
}

struct HashingReader {
    inner: Box<dyn Read + Send>,
    observed: Arc<Mutex<ObservedUpload>>,
}

impl Read for HashingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 {
            return Ok(0);
        }
        let mut observed = self
            .observed
            .lock()
            .map_err(|_| io::Error::other("streamed upload identity state was unavailable"))?;
        observed.byte_len = observed
            .byte_len
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("streamed upload length overflowed"))?;
        observed.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

pub(super) fn verify_matching_readers(
    key: &ObjectKey,
    actual: &mut dyn Read,
    expected: &mut dyn Read,
    expected_len: u64,
    expected_hash: &str,
) -> Result<(), ByteStoreError> {
    let mut actual = BufReader::with_capacity(64 * 1024, actual);
    let mut expected = BufReader::with_capacity(64 * 1024, expected);
    let mut hasher = blake3::Hasher::new();
    let mut byte_len = 0_u64;
    loop {
        let actual_bytes = actual.fill_buf()?;
        let expected_bytes = expected.fill_buf()?;
        if actual_bytes.is_empty() || expected_bytes.is_empty() {
            if actual_bytes.is_empty() && expected_bytes.is_empty() {
                break;
            }
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "existing upload differs from retry bytes",
            });
        }
        let compared = actual_bytes.len().min(expected_bytes.len());
        if actual_bytes[..compared] != expected_bytes[..compared] {
            return Err(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "existing upload differs from retry bytes",
            });
        }
        hasher.update(&actual_bytes[..compared]);
        byte_len = byte_len
            .checked_add(compared as u64)
            .ok_or(ByteStoreError::CorruptObject {
                key: key.clone(),
                reason: "existing upload length overflowed",
            })?;
        actual.consume(compared);
        expected.consume(compared);
    }
    let actual_hash = format!("b3_{}", hasher.finalize().to_hex());
    if byte_len != expected_len || actual_hash != expected_hash {
        return Err(ByteStoreError::CorruptObject {
            key: key.clone(),
            reason: "existing upload does not match retry metadata",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bowline_storage::stable_object_hash;
    use std::{
        io::{Cursor, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    const TEST_CHECKSUM_SHA256: &str = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

    #[derive(Clone)]
    struct TestSource {
        bytes: Arc<Vec<u8>>,
        flip_after: Option<usize>,
        opens: Arc<Mutex<usize>>,
        max_read: Arc<Mutex<usize>>,
    }

    impl ReopenableObjectSource for TestSource {
        fn open(&self) -> io::Result<Box<dyn Read + Send>> {
            *self.opens.lock().expect("open counter") += 1;
            Ok(Box::new(TestReader {
                bytes: self.bytes.clone(),
                offset: 0,
                flip_after: self.flip_after,
                max_read: self.max_read.clone(),
            }))
        }
    }

    struct TestReader {
        bytes: Arc<Vec<u8>>,
        offset: usize,
        flip_after: Option<usize>,
        max_read: Arc<Mutex<usize>>,
    }

    impl Read for TestReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let read = buffer.len().min(4096).min(self.bytes.len() - self.offset);
            let mut max_read = self.max_read.lock().expect("read counter");
            *max_read = (*max_read).max(buffer.len());
            drop(max_read);
            buffer[..read].copy_from_slice(&self.bytes[self.offset..self.offset + read]);
            if self.flip_after.is_some_and(|offset| self.offset >= offset) {
                buffer[..read].iter_mut().for_each(|byte| *byte ^= 0xff);
            }
            self.offset += read;
            Ok(read)
        }
    }

    struct ChunkedReader {
        inner: Cursor<Vec<u8>>,
        chunks: Vec<usize>,
        index: usize,
    }

    impl ChunkedReader {
        fn new(bytes: Vec<u8>, chunks: Vec<usize>) -> Self {
            Self {
                inner: Cursor::new(bytes),
                chunks,
                index: 0,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = self.chunks[self.index % self.chunks.len()].min(buffer.len());
            self.index += 1;
            self.inner.read(&mut buffer[..limit])
        }
    }

    fn put_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.contains("if-none-match: *"));
            assert!(headers.contains(&format!("x-amz-checksum-sha256: {TEST_CHECKSUM_SHA256}")));
            let len = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .expect("length")
                .parse::<usize>()
                .expect("numeric length");
            while request.len() - header_end < len {
                let read = stream.read(&mut buffer).expect("body");
                request.extend_from_slice(&buffer[..read]);
            }
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").expect("response");
        });
        format!("http://{address}/upload")
    }

    fn client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    #[test]
    fn put_streams_bounded_bytes_and_reopens_identically() {
        let bytes = Arc::new(vec![0x5a; 256 * 1024]);
        let source = TestSource {
            bytes: bytes.clone(),
            flip_after: None,
            opens: Arc::new(Mutex::new(0)),
            max_read: Arc::new(Mutex::new(0)),
        };
        let key =
            ObjectKey::new("b_00112233445566d100112233445566d100112233445566d100112233445566d1")
                .expect("key");
        let hash = stable_object_hash(&bytes);
        for _ in 0..2 {
            send_streaming_put(
                &client(),
                &put_server(),
                &key,
                &source,
                bytes.len() as u64,
                &hash,
                TEST_CHECKSUM_SHA256,
            )
            .expect("put");
        }
        assert_eq!(*source.opens.lock().expect("opens"), 2);
        assert!(*source.max_read.lock().expect("max read") < bytes.len());
    }

    #[test]
    fn put_rejects_same_length_preopen_and_midstream_mutation() {
        let expected = Arc::new(vec![0x4d; 128 * 1024]);
        let key =
            ObjectKey::new("b_00112233445566d200112233445566d200112233445566d200112233445566d2")
                .expect("key");
        let hash = stable_object_hash(&expected);
        for source in [
            TestSource {
                bytes: Arc::new(vec![0xa4; expected.len()]),
                flip_after: None,
                opens: Arc::new(Mutex::new(0)),
                max_read: Arc::new(Mutex::new(0)),
            },
            TestSource {
                bytes: expected.clone(),
                flip_after: Some(expected.len() / 2),
                opens: Arc::new(Mutex::new(0)),
                max_read: Arc::new(Mutex::new(0)),
            },
        ] {
            assert!(matches!(
                send_streaming_put(
                    &client(),
                    &put_server(),
                    &key,
                    &source,
                    expected.len() as u64,
                    &hash,
                    TEST_CHECKSUM_SHA256,
                ),
                Err(ByteStoreError::CorruptObject { .. })
            ));
        }
    }

    #[test]
    fn verification_is_independent_of_read_boundaries() {
        let key =
            ObjectKey::new("b_00112233445566d300112233445566d300112233445566d300112233445566d3")
                .expect("key");
        let bytes = (0..200_000).map(|index| index as u8).collect::<Vec<_>>();
        let hash = stable_object_hash(&bytes);
        verify_matching_readers(
            &key,
            &mut ChunkedReader::new(bytes.clone(), vec![1, 7, 4093]),
            &mut ChunkedReader::new(bytes.clone(), vec![8192, 3, 65]),
            bytes.len() as u64,
            &hash,
        )
        .expect("asymmetric chunks");
        verify_matching_readers(
            &key,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Cursor::new(Vec::<u8>::new()),
            0,
            &stable_object_hash(&[]),
        )
        .expect("empty EOF");
        for candidate in [
            bytes[..bytes.len() - 1].to_vec(),
            [bytes.clone(), vec![1]].concat(),
            {
                let mut changed = bytes.clone();
                changed[100_000] ^= 0xff;
                changed
            },
        ] {
            assert!(
                verify_matching_readers(
                    &key,
                    &mut ChunkedReader::new(candidate, vec![5, 1, 2048]),
                    &mut ChunkedReader::new(bytes.clone(), vec![17, 4096]),
                    bytes.len() as u64,
                    &hash,
                )
                .is_err()
            );
        }
    }
}
