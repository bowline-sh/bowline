use super::*;

use crossbeam_channel::{Receiver, Sender, bounded};

pub(super) struct ConnectionExecutor {
    work: Option<Sender<ConnectionTask>>,
    completions: Receiver<()>,
    workers: Vec<ConnectionWorker>,
}

struct ConnectionTask {
    stream: UnixStream,
    state: Arc<DaemonServerState>,
    socket_owner_uid: Option<u32>,
    rpc_executor: Arc<super::super::protocol_v2::RpcExecutor>,
    acceptor_wake: AcceptorWake,
}

struct ConnectionWorker {
    handle: std::thread::JoinHandle<()>,
    done: Receiver<()>,
}

impl ConnectionExecutor {
    pub(super) fn start(worker_count: usize) -> io::Result<Self> {
        if worker_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the daemon connection executor requires at least one worker",
            ));
        }
        let (work_tx, work_rx) = bounded(worker_count);
        let (completion_tx, completions) = bounded(worker_count);
        let mut workers = Vec::<ConnectionWorker>::with_capacity(worker_count);
        for index in 0..worker_count {
            let worker_rx = work_rx.clone();
            let worker_completion = completion_tx.clone();
            let (done_tx, done) = bounded(1);
            let handle = match std::thread::Builder::new()
                .name(format!("bowline-rpc-connection-{index}"))
                .spawn(move || {
                    run_connection_worker(worker_rx, worker_completion);
                    let _receiver_gone = done_tx.send(());
                }) {
                Ok(handle) => handle,
                Err(error) => {
                    drop(work_tx);
                    for worker in workers {
                        let _finished = worker.done.recv();
                        let _join_result = worker.handle.join();
                    }
                    return Err(error);
                }
            };
            workers.push(ConnectionWorker { handle, done });
        }
        drop(completion_tx);
        Ok(Self {
            work: Some(work_tx),
            completions,
            workers,
        })
    }

    pub(super) fn try_submit(
        &self,
        stream: UnixStream,
        state: Arc<DaemonServerState>,
        socket_owner_uid: Option<u32>,
        rpc_executor: Arc<super::super::protocol_v2::RpcExecutor>,
        acceptor_wake: AcceptorWake,
    ) -> Result<(), UnixStream> {
        let task = ConnectionTask {
            stream,
            state,
            socket_owner_uid,
            rpc_executor,
            acceptor_wake,
        };
        let Some(work) = self.work.as_ref() else {
            return Err(task.stream);
        };
        work.try_send(task)
            .map_err(|error| error.into_inner().stream)
    }

    pub(super) fn completions(&self) -> &Receiver<()> {
        &self.completions
    }

    pub(super) fn shutdown_and_join(mut self, grace: Duration) -> io::Result<ThreadJoinReport> {
        self.work.take();
        let deadline = Instant::now() + grace;
        let expected = self.workers.len();
        let mut joined = 0;
        let mut forced_recovery = false;
        for worker in self.workers.drain(..) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match worker.done.recv_timeout(remaining) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    forced_recovery = true;
                    super::handle_shutdown_grace_expiry("RPC connection worker");
                    let _finished_or_panicked = worker.done.recv();
                }
            }
            worker
                .handle
                .join()
                .map_err(|_| io::Error::other("bowline RPC connection worker panicked"))?;
            joined += 1;
        }
        Ok(ThreadJoinReport {
            expected,
            joined,
            forced_recovery,
        })
    }
}

impl Drop for ConnectionExecutor {
    fn drop(&mut self) {
        self.work.take();
        for worker in self.workers.drain(..) {
            let _finished_or_panicked = worker.done.recv();
            if worker.handle.join().is_err() {
                eprintln!("bowline-daemon RPC connection worker panicked during ownership drop");
            }
        }
    }
}

fn run_connection_worker(work: Receiver<ConnectionTask>, completions: Sender<()>) {
    while let Ok(task) = work.recv() {
        let state = Arc::clone(&task.state);
        let wake = task.acceptor_wake.clone();
        let _connection = ConnectionGuard {
            state: Arc::clone(&state),
            acceptor_wake: wake.clone(),
        };
        if !state.should_stop_connections() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_connection(
                    task.stream,
                    &state,
                    task.socket_owner_uid,
                    task.rpc_executor,
                )
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("bowline-daemon ignored client error: {error}"),
                Err(_) => eprintln!("bowline-daemon isolated a panicked connection handler"),
            }
        }
        let _receiver_gone = completions.send(());
    }
}
