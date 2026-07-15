use super::*;

use crossbeam_channel::{Receiver, bounded};

#[derive(Clone)]
pub(in crate::daemon) struct AcceptorWake {
    socket: Arc<PathBuf>,
    stopping: Arc<AtomicBool>,
}

impl AcceptorWake {
    fn wake_socket(&self) -> io::Result<()> {
        UnixStream::connect(self.socket.as_path()).map(drop)
    }

    pub(in crate::daemon) fn stop(&self) -> io::Result<()> {
        if self.stopping.swap(true, Ordering::AcqRel) {
            Ok(())
        } else {
            match self.wake_socket() {
                Ok(()) => Ok(()),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    // The worker can observe the stop flag and close the
                    // listener before this self-connect races into accept.
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }
}

pub(super) enum AcceptorEvent {
    Accepted(UnixStream),
    Failed(io::Error),
    Stopped,
}

pub(super) struct BlockingAcceptor {
    events: Receiver<AcceptorEvent>,
    wake: AcceptorWake,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl BlockingAcceptor {
    pub(super) fn start(
        listener: UnixListener,
        socket: &Path,
        state: Arc<DaemonServerState>,
    ) -> io::Result<Self> {
        let (events_tx, events) = bounded(1);
        let wake = AcceptorWake {
            socket: Arc::new(socket.to_path_buf()),
            stopping: Arc::new(AtomicBool::new(false)),
        };
        let worker_wake = wake.clone();
        let worker = std::thread::Builder::new()
            .name("bowline-rpc-acceptor".to_string())
            .spawn(move || {
                loop {
                    if worker_wake.is_stopping() || state.shutting_down.load(Ordering::Acquire) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if worker_wake.is_stopping()
                                || state.shutting_down.load(Ordering::Acquire)
                            {
                                break;
                            }
                            if events_tx.send(AcceptorEvent::Accepted(stream)).is_err() {
                                return;
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            let _receiver_gone = events_tx.send(AcceptorEvent::Failed(error));
                            return;
                        }
                    }
                }
                let _receiver_gone = events_tx.send(AcceptorEvent::Stopped);
            })?;
        Ok(Self {
            events,
            wake,
            worker: Some(worker),
        })
    }

    pub(super) fn events(&self) -> &Receiver<AcceptorEvent> {
        &self.events
    }

    pub(super) fn wake(&self) -> AcceptorWake {
        self.wake.clone()
    }

    pub(super) fn stop(&self) -> io::Result<()> {
        self.wake.stop()
    }

    pub(super) fn ensure_stopped(&self) -> io::Result<()> {
        if self
            .worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            Ok(())
        } else {
            self.stop()
        }
    }

    pub(super) fn join(mut self) -> io::Result<()> {
        join_worker(&mut self.worker)
    }
}

impl Drop for BlockingAcceptor {
    fn drop(&mut self) {
        if self.worker.is_some() {
            if let Err(error) = self.ensure_stopped() {
                eprintln!("bowline-daemon could not wake RPC acceptor during drop: {error}");
            }
            let _join_result = join_worker(&mut self.worker);
        }
    }
}

fn join_worker(worker: &mut Option<std::thread::JoinHandle<()>>) -> io::Result<()> {
    worker
        .take()
        .expect("acceptor worker is owned until join")
        .join()
        .map_err(|_| io::Error::other("bowline RPC acceptor panicked"))
}
