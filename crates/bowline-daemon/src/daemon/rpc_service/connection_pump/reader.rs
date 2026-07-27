use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
    thread::{self, JoinHandle},
};

use bowline_daemon_rpc::{
    CodecError, CodecPhase, DEFAULT_MAX_FRAME_BYTES, IncrementalFrameDecoder,
};
use crossbeam_channel::{Receiver, Sender, bounded};

use super::{MAX_IN_FLIGHT_REQUESTS, RpcConnectionId};

const READER_QUEUE_CAPACITY: usize = MAX_IN_FLIGHT_REQUESTS + 1;

#[cfg(test)]
mod tests;

pub(super) enum ReaderEvent {
    Payload(Vec<u8>),
    CleanEof,
    Failed(CodecError),
}

pub(super) fn spawn(
    stream: UnixStream,
    connection_id: RpcConnectionId,
) -> io::Result<(Receiver<ReaderEvent>, JoinHandle<()>)> {
    let (sender, receiver) = bounded(READER_QUEUE_CAPACITY);
    let reader = thread::Builder::new()
        .name(format!("bowline-rpc-reader-{}", connection_id.get()))
        .spawn(move || read_connection(stream, sender))?;
    Ok((receiver, reader))
}

fn read_connection(mut stream: UnixStream, sender: Sender<ReaderEvent>) {
    let mut decoder = IncrementalFrameDecoder::new(DEFAULT_MAX_FRAME_BYTES);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                let event = match decoder.finish() {
                    Ok(()) => ReaderEvent::CleanEof,
                    Err(error) => ReaderEvent::Failed(error),
                };
                let _receiver_closed = sender.send(event);
                return;
            }
            Ok(count) => {
                let frames = match decoder.append(&chunk[..count]) {
                    Ok(frames) => frames,
                    Err(error) => {
                        let _receiver_closed = sender.send(ReaderEvent::Failed(error));
                        return;
                    }
                };
                for payload in frames {
                    if sender.send(ReaderEvent::Payload(payload)).is_err() {
                        return;
                    }
                }
            }
            Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => {
                let _receiver_closed = sender.send(ReaderEvent::Failed(CodecError::Io {
                    phase: CodecPhase::Payload,
                    source,
                }));
                return;
            }
        }
    }
}
