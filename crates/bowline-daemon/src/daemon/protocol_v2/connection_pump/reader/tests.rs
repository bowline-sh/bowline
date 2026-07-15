use super::*;
use bowline_core::wire::generated::DaemonRpcRequest;
use bowline_daemon_rpc::FrameCodec;
use serde_json::json;
use std::{io::Write, net::Shutdown, time::Duration};

#[test]
fn dropping_full_reader_queue_unblocks_reader_join() {
    let (server, mut client) = UnixStream::pair().expect("socket pair");
    client
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    let (receiver, reader) = spawn(server, RpcConnectionId::new(1)).expect("reader starts");
    let codec = FrameCodec::default();

    for index in 0..=READER_QUEUE_CAPACITY {
        codec
            .write(
                &mut client,
                &DaemonRpcRequest {
                    request_id: format!("request-{index}"),
                    method: "daemon.ping".to_string(),
                    params: json!({}),
                    deadline_ms: Some(2_000),
                },
            )
            .expect("pipelined request writes");
    }
    client.flush().expect("pipelined requests flush");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while receiver.len() < READER_QUEUE_CAPACITY && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(receiver.len(), READER_QUEUE_CAPACITY);

    drop(receiver);
    let _shutdown = client.shutdown(Shutdown::Both);
    reader.join().expect("reader exits when receiver drops");
}
