//! Watcher implementation for the inotify Linux API
//!
//! The inotify API provides a mechanism for monitoring filesystem events.  Inotify can be used to
//! monitor individual files, or to monitor directories.  When a directory is monitored, inotify
//! will return events for the directory itself, and for files inside the directory.

use super::event::*;
use super::{Config, Error, ErrorKind, EventHandler, RecursiveMode, Result, Watcher};
use crate::{bounded, unbounded, BoundSender, Receiver, Sender};
use inotify as inotify_sys;
use inotify_sys::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs::metadata;
use std::num::NonZeroU64;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

const INOTIFY: mio::Token = mio::Token(0);
const MESSAGE: mio::Token = mio::Token(1);
const DRAIN_INTERRUPT_BATCH: usize = 64;

/// A caller-provided identifier for one inotify control acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct INotifyCoverageToken(NonZeroU64);

impl INotifyCoverageToken {
    /// Construct a nonzero control token.
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Return the integer representation of this token.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A same-event-loop inotify coverage acknowledgement.
#[derive(Debug)]
pub enum INotifyCoverageSignal {
    /// Recursive watches are installed and pending callbacks were drained.
    Ready(INotifyCoverageToken),
    /// The requested watch or boundary could not be established.
    Failed(INotifyCoverageToken, Error),
    /// The inotify event loop stopped and will emit no further events.
    Stopped,
}

/// Receives inotify control acknowledgements separately from translated events.
pub trait INotifyCoverageHandler: Send + 'static {
    /// Handle one native watcher-control signal.
    fn handle_coverage(&mut self, signal: INotifyCoverageSignal);
}

impl<F> INotifyCoverageHandler for F
where
    F: FnMut(INotifyCoverageSignal) + Send + 'static,
{
    fn handle_coverage(&mut self, signal: INotifyCoverageSignal) {
        (self)(signal);
    }
}

// The EventLoop will set up a mio::Poll and use it to wait for the following:
//
// -  messages telling it what to do
//
// -  events telling it that something has happened on one of the watched files.

struct EventLoop {
    running: bool,
    poll: mio::Poll,
    event_loop_waker: Arc<mio::Waker>,
    event_loop_tx: Sender<EventLoopMsg>,
    event_loop_rx: Receiver<EventLoopMsg>,
    inotify: Option<Inotify>,
    event_handler: Box<dyn EventHandler>,
    coverage_handler: Option<Box<dyn INotifyCoverageHandler>>,
    /// PathBuf -> (WatchDescriptor, WatchMask, is_recursive, is_dir)
    watches: HashMap<PathBuf, (WatchDescriptor, WatchMask, bool, bool)>,
    paths: HashMap<WatchDescriptor, PathBuf>,
    rename_event: Option<Event>,
    follow_links: bool,
    shutdown_requested: Arc<AtomicBool>,
}

/// Watcher implementation based on inotify
#[derive(Debug)]
pub struct INotifyWatcher {
    channel: Sender<EventLoopMsg>,
    waker: Arc<mio::Waker>,
    worker: Option<thread::JoinHandle<()>>,
    shutdown_requested: Arc<AtomicBool>,
}

enum EventLoopMsg {
    AddWatch(PathBuf, RecursiveMode, Sender<Result<()>>),
    AddWatchReady(PathBuf, RecursiveMode, INotifyCoverageToken),
    CoverageBoundary(PathBuf, INotifyCoverageToken),
    RemoveWatch(PathBuf, Sender<Result<()>>),
    Shutdown,
    Configure(Config, BoundSender<Result<bool>>),
}

#[inline]
fn add_watch_by_event(
    path: &PathBuf,
    event: &inotify_sys::Event<&OsStr>,
    watches: &HashMap<PathBuf, (WatchDescriptor, WatchMask, bool, bool)>,
    add_watches: &mut Vec<PathBuf>,
) {
    if event.mask.contains(EventMask::ISDIR) {
        if let Some(parent_path) = path.parent() {
            if let Some(&(_, _, is_recursive, _)) = watches.get(parent_path) {
                if is_recursive {
                    add_watches.push(path.to_owned());
                }
            }
        }
    }
}

#[inline]
fn remove_watch_by_event(
    path: &PathBuf,
    watches: &HashMap<PathBuf, (WatchDescriptor, WatchMask, bool, bool)>,
    remove_watches: &mut Vec<PathBuf>,
) {
    if watches.contains_key(path) {
        remove_watches.push(path.to_owned());
    }
}

impl EventLoop {
    pub fn new(
        inotify: Inotify,
        event_handler: Box<dyn EventHandler>,
        coverage_handler: Option<Box<dyn INotifyCoverageHandler>>,
        follow_links: bool,
    ) -> Result<Self> {
        let (event_loop_tx, event_loop_rx) = unbounded::<EventLoopMsg>();
        let poll = mio::Poll::new()?;

        let event_loop_waker = Arc::new(mio::Waker::new(poll.registry(), MESSAGE)?);

        let inotify_fd = inotify.as_raw_fd();
        let mut evented_inotify = mio::unix::SourceFd(&inotify_fd);
        poll.registry()
            .register(&mut evented_inotify, INOTIFY, mio::Interest::READABLE)?;

        let event_loop = EventLoop {
            running: true,
            poll,
            event_loop_waker,
            event_loop_tx,
            event_loop_rx,
            inotify: Some(inotify),
            event_handler,
            coverage_handler,
            watches: HashMap::new(),
            paths: HashMap::new(),
            rename_event: None,
            follow_links,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
        };
        Ok(event_loop)
    }

    // Run the event loop.
    pub fn run(self) -> Result<thread::JoinHandle<()>> {
        thread::Builder::new()
            .name("notify-rs inotify loop".to_string())
            .spawn(|| self.event_loop_thread())
            .map_err(Error::io)
    }

    fn event_loop_thread(mut self) {
        let mut events = mio::Events::with_capacity(16);
        loop {
            // Wait for something to happen.
            match self.poll.poll(&mut events, None) {
                Err(ref e) if matches!(e.kind(), std::io::ErrorKind::Interrupted) => {
                    // System call was interrupted, we will retry
                    // TODO: Not covered by tests (to reproduce likely need to setup signal handlers)
                }
                Err(e) => panic!("poll failed: {}", e),
                Ok(()) => {}
            }

            // Process whatever happened.
            for event in &events {
                self.handle_event(event);
            }

            // Stop, if we're done.
            if !self.running {
                break;
            }
        }
    }

    // Handle a single event.
    fn handle_event(&mut self, event: &mio::event::Event) {
        match event.token() {
            MESSAGE => {
                // The channel is readable - handle messages.
                self.handle_messages()
            }
            INOTIFY => {
                // inotify has something to tell us.
                let _ = self.drain_inotify_until_stable(false);
            }
            _ => unreachable!(),
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.event_loop_rx.try_recv() {
            match msg {
                EventLoopMsg::AddWatch(path, recursive_mode, tx) => {
                    let _ = tx.send(self.add_watch(path, recursive_mode.is_recursive(), true));
                }
                EventLoopMsg::AddWatchReady(path, recursive_mode, token) => {
                    let result = self
                        .add_watch_strict(path, recursive_mode.is_recursive(), true)
                        .and_then(|()| self.drain_inotify_until_stable(true));
                    self.finish_coverage_request(token, result);
                }
                EventLoopMsg::CoverageBoundary(path, token) => {
                    // The message, native reads, recursive walk, and final
                    // acknowledgement all execute serially on this event loop.
                    // A successful marker therefore follows an actual
                    // WouldBlock after the last watch mutation.
                    let result = self
                        .drain_inotify_until_stable(true)
                        .and_then(|()| self.add_watch_strict(path, true, true))
                        .and_then(|()| self.drain_inotify_until_stable(true));
                    self.finish_coverage_request(token, result);
                }
                EventLoopMsg::RemoveWatch(path, tx) => {
                    let _ = tx.send(self.remove_watch(path, false));
                }
                EventLoopMsg::Shutdown => {
                    let _ = self.remove_all_watches();
                    if let Some(inotify) = self.inotify.take() {
                        let _ = inotify.close();
                    }
                    if let Some(handler) = &mut self.coverage_handler {
                        handler.handle_coverage(INotifyCoverageSignal::Stopped);
                    }
                    self.running = false;
                    break;
                }
                EventLoopMsg::Configure(config, tx) => {
                    self.configure_raw_mode(config, tx);
                }
            }
        }
    }

    fn finish_coverage_request(&mut self, token: INotifyCoverageToken, result: Result<()>) {
        let Some(handler) = &mut self.coverage_handler else {
            return;
        };
        match result {
            Ok(()) => handler.handle_coverage(INotifyCoverageSignal::Ready(token)),
            Err(error) => handler.handle_coverage(INotifyCoverageSignal::Failed(token, error)),
        }
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: BoundSender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnected");
    }

    fn drain_inotify_until_stable(&mut self, strict: bool) -> Result<()> {
        loop {
            self.check_drain_interruption()?;
            let watch_graph_changed = self.drain_inotify_once(strict)?;
            if !watch_graph_changed {
                return Ok(());
            }
        }
    }

    fn drain_inotify_once(&mut self, strict: bool) -> Result<bool> {
        let mut add_watches = Vec::new();
        let mut remove_watches = Vec::new();
        let mut overflowed = false;

        if let Some(ref mut inotify) = self.inotify {
            let mut buffer = [0; 1024];
            let mut events_since_interrupt_check = 0;
            // Read all buffers available.
            loop {
                if self.shutdown_requested.load(Ordering::Acquire) {
                    return Err(Error::generic(
                        "inotify callback drain interrupted by shutdown",
                    ));
                }
                match inotify.read_events(&mut buffer) {
                    Ok(events) => {
                        let mut num_events = 0;
                        for event in events {
                            events_since_interrupt_check += 1;
                            if events_since_interrupt_check == DRAIN_INTERRUPT_BATCH {
                                events_since_interrupt_check = 0;
                                if self.shutdown_requested.load(Ordering::Acquire) {
                                    return Err(Error::generic(
                                        "inotify callback drain interrupted by shutdown",
                                    ));
                                }
                            }
                            log::trace!("inotify event: {event:?}");

                            num_events += 1;
                            if event.mask.contains(EventMask::Q_OVERFLOW) {
                                overflowed = true;
                                let ev = Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan));
                                self.event_handler.handle_event(ev);
                            }

                            let path = match event.name {
                                Some(name) => self.paths.get(&event.wd).map(|root| root.join(name)),
                                None => self.paths.get(&event.wd).cloned(),
                            };

                            let path = match path {
                                Some(path) => path,
                                None => {
                                    log::debug!("inotify event with unknown descriptor: {event:?}");
                                    continue;
                                }
                            };

                            let mut evs = Vec::new();

                            if event.mask.contains(EventMask::MOVED_FROM) {
                                remove_watch_by_event(&path, &self.watches, &mut remove_watches);

                                let event = Event::new(EventKind::Modify(ModifyKind::Name(
                                    RenameMode::From,
                                )))
                                .add_path(path.clone())
                                .set_tracker(event.cookie as usize);

                                self.rename_event = Some(event.clone());

                                evs.push(event);
                            } else if event.mask.contains(EventMask::MOVED_TO) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
                                        .set_tracker(event.cookie as usize)
                                        .add_path(path.clone()),
                                );

                                let trackers_match =
                                    self.rename_event.as_ref().and_then(|e| e.tracker())
                                        == Some(event.cookie as usize);

                                if trackers_match {
                                    let rename_event = self.rename_event.take().unwrap(); // unwrap is safe because `rename_event` must be set at this point
                                    evs.push(
                                        Event::new(EventKind::Modify(ModifyKind::Name(
                                            RenameMode::Both,
                                        )))
                                        .set_tracker(event.cookie as usize)
                                        .add_some_path(rename_event.paths.first().cloned())
                                        .add_path(path.clone()),
                                    );
                                }
                                add_watch_by_event(&path, &event, &self.watches, &mut add_watches);
                            }
                            if event.mask.contains(EventMask::MOVE_SELF) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Name(
                                        RenameMode::From,
                                    )))
                                    .add_path(path.clone()),
                                );
                                // TODO stat the path and get to new path
                                // - emit To and Both events
                                // - change prefix for further events
                            }
                            if event.mask.contains(EventMask::CREATE) {
                                evs.push(
                                    Event::new(EventKind::Create(
                                        if event.mask.contains(EventMask::ISDIR) {
                                            CreateKind::Folder
                                        } else {
                                            CreateKind::File
                                        },
                                    ))
                                    .add_path(path.clone()),
                                );
                                add_watch_by_event(&path, &event, &self.watches, &mut add_watches);
                            }
                            if event.mask.contains(EventMask::DELETE) {
                                evs.push(
                                    Event::new(EventKind::Remove(
                                        if event.mask.contains(EventMask::ISDIR) {
                                            RemoveKind::Folder
                                        } else {
                                            RemoveKind::File
                                        },
                                    ))
                                    .add_path(path.clone()),
                                );
                                remove_watch_by_event(&path, &self.watches, &mut remove_watches);
                            }
                            if event.mask.contains(EventMask::DELETE_SELF) {
                                let remove_kind = match self.watches.get(&path) {
                                    Some(&(_, _, _, true)) => RemoveKind::Folder,
                                    Some(&(_, _, _, false)) => RemoveKind::File,
                                    None => RemoveKind::Other,
                                };
                                evs.push(
                                    Event::new(EventKind::Remove(remove_kind))
                                        .add_path(path.clone()),
                                );
                                remove_watch_by_event(&path, &self.watches, &mut remove_watches);
                            }
                            if event.mask.contains(EventMask::MODIFY) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Data(
                                        DataChange::Any,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::CLOSE_WRITE) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Close(
                                        AccessMode::Write,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::CLOSE_NOWRITE) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Close(
                                        AccessMode::Read,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::ATTRIB) {
                                evs.push(
                                    Event::new(EventKind::Modify(ModifyKind::Metadata(
                                        MetadataKind::Any,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }
                            if event.mask.contains(EventMask::OPEN) {
                                evs.push(
                                    Event::new(EventKind::Access(AccessKind::Open(
                                        AccessMode::Any,
                                    )))
                                    .add_path(path.clone()),
                                );
                            }

                            for ev in evs {
                                self.event_handler.handle_event(Ok(ev));
                            }
                        }

                        // All events read. Break out.
                        if num_events == 0 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // No events read. Break out.
                        break;
                    }
                    Err(e) => {
                        self.event_handler.handle_event(Err(Error::io(e)));
                        return Err(Error::generic("inotify callback drain failed"));
                    }
                }
            }
        }

        let watch_graph_changed = !add_watches.is_empty() || !remove_watches.is_empty();
        for path in remove_watches {
            if let Err(error) = self.remove_watch(path, true) {
                if strict {
                    return Err(error);
                }
            }
        }

        for path in add_watches {
            let add_result = if strict {
                self.add_watch_strict(path, true, false)
            } else {
                self.add_watch(path, true, false)
            };
            if let Err(add_watch_error) = add_result {
                // The handler should be notified if we have reached the limit.
                // Otherwise, the user might expect that a recursive watch
                // is continuing to work correctly, but it's not.
                if matches!(&add_watch_error.kind, ErrorKind::MaxFilesWatch) {
                    if strict {
                        return Err(add_watch_error);
                    }
                    self.event_handler.handle_event(Err(add_watch_error));

                    // After that kind of a error we should stop adding watches,
                    // because the limit has already reached and all next calls
                    // will return us only the same error.
                    break;
                }
                if strict {
                    return Err(add_watch_error);
                }
            }
        }

        if overflowed {
            return Err(Error::generic(
                "inotify queue overflowed while establishing coverage",
            ));
        }
        Ok(watch_graph_changed)
    }

    fn check_drain_interruption(&self) -> Result<()> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(Error::generic(
                "inotify callback drain interrupted by shutdown",
            ));
        }
        Ok(())
    }

    fn add_watch(&mut self, path: PathBuf, is_recursive: bool, mut watch_self: bool) -> Result<()> {
        // If the watch is not recursive, or if we determine (by stat'ing the path to get its
        // metadata) that the watched path is not a directory, add a single path watch.
        if !is_recursive || !metadata(&path).map_err(Error::io_watch)?.is_dir() {
            return self.add_single_watch(path, false, true);
        }

        for entry in WalkDir::new(path)
            .follow_links(self.follow_links)
            .into_iter()
            .filter_map(filter_dir)
        {
            self.add_single_watch(entry.into_path(), is_recursive, watch_self)?;
            watch_self = false;
        }

        Ok(())
    }

    fn add_watch_strict(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        mut watch_self: bool,
    ) -> Result<()> {
        if !is_recursive || !metadata(&path).map_err(Error::io_watch)?.is_dir() {
            return self.add_single_watch(path, false, true);
        }

        for entry in WalkDir::new(path)
            .follow_links(self.follow_links)
            .into_iter()
        {
            let entry = entry.map_err(walk_error)?;
            if !entry.metadata().map_err(walk_error)?.is_dir() {
                continue;
            }
            self.add_single_watch(entry.into_path(), is_recursive, watch_self)?;
            watch_self = false;
        }

        Ok(())
    }

    fn add_single_watch(
        &mut self,
        path: PathBuf,
        is_recursive: bool,
        watch_self: bool,
    ) -> Result<()> {
        let mut watchmask = WatchMask::ATTRIB
            | WatchMask::CREATE
            | WatchMask::OPEN
            | WatchMask::DELETE
            | WatchMask::CLOSE_WRITE
            | WatchMask::MODIFY
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO;

        if watch_self {
            watchmask.insert(WatchMask::DELETE_SELF);
            watchmask.insert(WatchMask::MOVE_SELF);
        }

        if let Some(&(_, old_watchmask, _, _)) = self.watches.get(&path) {
            watchmask.insert(old_watchmask);
            watchmask.insert(WatchMask::MASK_ADD);
        }

        if let Some(ref mut inotify) = self.inotify {
            log::trace!("adding inotify watch: {}", path.display());

            match inotify.watches().add(&path, watchmask) {
                Err(e) => {
                    Err(if e.raw_os_error() == Some(libc::ENOSPC) {
                        // do not report inotify limits as "no more space" on linux #266
                        Error::new(ErrorKind::MaxFilesWatch)
                    } else if e.kind() == std::io::ErrorKind::NotFound {
                        Error::new(ErrorKind::PathNotFound)
                    } else {
                        Error::io(e)
                    }
                    .add_path(path))
                }
                Ok(w) => {
                    watchmask.remove(WatchMask::MASK_ADD);
                    let is_dir = metadata(&path).map_err(Error::io)?.is_dir();
                    self.watches
                        .insert(path.clone(), (w.clone(), watchmask, is_recursive, is_dir));
                    self.paths.insert(w, path);
                    Ok(())
                }
            }
        } else {
            Ok(())
        }
    }

    fn remove_watch(&mut self, path: PathBuf, remove_recursive: bool) -> Result<()> {
        match self.watches.remove(&path) {
            None => return Err(Error::watch_not_found().add_path(path)),
            Some((w, _, is_recursive, _)) => {
                if let Some(ref mut inotify) = self.inotify {
                    let mut inotify_watches = inotify.watches();
                    log::trace!("removing inotify watch: {}", path.display());

                    inotify_watches
                        .remove(w.clone())
                        .map_err(|e| Error::io(e).add_path(path.clone()))?;
                    self.paths.remove(&w);

                    if is_recursive || remove_recursive {
                        let mut remove_list = Vec::new();
                        for (w, p) in &self.paths {
                            if p.starts_with(&path) {
                                inotify_watches
                                    .remove(w.clone())
                                    .map_err(|e| Error::io(e).add_path(p.into()))?;
                                self.watches.remove(p);
                                remove_list.push(w.clone());
                            }
                        }
                        for w in remove_list {
                            self.paths.remove(&w);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_all_watches(&mut self) -> Result<()> {
        if let Some(ref mut inotify) = self.inotify {
            let mut inotify_watches = inotify.watches();
            for (w, p) in &self.paths {
                inotify_watches
                    .remove(w.clone())
                    .map_err(|e| Error::io(e).add_path(p.into()))?;
            }
            self.watches.clear();
            self.paths.clear();
        }
        Ok(())
    }
}

/// return `DirEntry` when it is a directory
fn filter_dir(e: walkdir::Result<walkdir::DirEntry>) -> Option<walkdir::DirEntry> {
    if let Ok(e) = e {
        if let Ok(metadata) = e.metadata() {
            if metadata.is_dir() {
                return Some(e);
            }
        }
    }
    None
}

fn walk_error(error: walkdir::Error) -> Error {
    let path = error.path().map(Path::to_path_buf);
    let mut error = error.into_io_error().map_or_else(
        || Error::generic("recursive inotify walk failed"),
        Error::io_watch,
    );
    if let Some(path) = path {
        error = error.add_path(path);
    }
    error
}

impl INotifyWatcher {
    fn from_event_handler(
        event_handler: Box<dyn EventHandler>,
        coverage_handler: Option<Box<dyn INotifyCoverageHandler>>,
        follow_links: bool,
    ) -> Result<Self> {
        let inotify = Inotify::init()?;
        let event_loop = EventLoop::new(inotify, event_handler, coverage_handler, follow_links)?;
        let channel = event_loop.event_loop_tx.clone();
        let waker = event_loop.event_loop_waker.clone();
        let shutdown_requested = Arc::clone(&event_loop.shutdown_requested);
        let worker = event_loop.run()?;
        Ok(INotifyWatcher {
            channel,
            waker,
            worker: Some(worker),
            shutdown_requested,
        })
    }

    /// Create an inotify watcher with a separate native coverage-control lane.
    pub fn new_with_coverage<F, C>(
        event_handler: F,
        coverage_handler: C,
        config: Config,
    ) -> Result<Self>
    where
        F: EventHandler,
        C: INotifyCoverageHandler,
    {
        Self::from_event_handler(
            Box::new(event_handler),
            Some(Box::new(coverage_handler)),
            config.follow_symlinks(),
        )
    }

    /// Request recursive watch installation followed by a same-loop callback
    /// drain. Completion is reported with `token` on the coverage-control lane.
    pub fn request_watch_ready(
        &mut self,
        path: &Path,
        recursive_mode: RecursiveMode,
        token: INotifyCoverageToken,
    ) -> Result<()> {
        let path = self.absolute_path(path)?;
        self.send_control(EventLoopMsg::AddWatchReady(path, recursive_mode, token))
    }

    /// Request a recursive re-walk followed by a same-loop callback drain.
    /// Completion is reported with `token` on the coverage-control lane.
    pub fn request_coverage_boundary(
        &mut self,
        path: &Path,
        token: INotifyCoverageToken,
    ) -> Result<()> {
        let path = self.absolute_path(path)?;
        self.send_control(EventLoopMsg::CoverageBoundary(path, token))
    }

    /// Whether the owned inotify event-loop worker has exited.
    pub fn worker_is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .map_or(true, thread::JoinHandle::is_finished)
    }

    /// Stop the inotify event loop and join its worker.
    pub fn shutdown(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        self.shutdown_requested.store(true, Ordering::Release);
        let send_result = self.channel.send(EventLoopMsg::Shutdown);
        let wake_result = self.waker.wake();
        let join_result = worker.join();
        send_result?;
        wake_result?;
        join_result.map_err(|_| Error::generic("inotify worker panicked during shutdown"))
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            Ok(env::current_dir().map_err(Error::io)?.join(path))
        }
    }

    fn send_control(&self, message: EventLoopMsg) -> Result<()> {
        self.channel.send(message)?;
        self.waker.wake()?;
        Ok(())
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        let pb = self.absolute_path(path)?;
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::AddWatch(pb, recursive_mode, tx);

        // we expect the event loop to live and reply => unwraps must not panic
        self.channel.send(msg).unwrap();
        self.waker.wake().unwrap();
        rx.recv().unwrap()
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        let pb = self.absolute_path(path)?;
        let (tx, rx) = unbounded();
        let msg = EventLoopMsg::RemoveWatch(pb, tx);

        // we expect the event loop to live and reply => unwraps must not panic
        self.channel.send(msg).unwrap();
        self.waker.wake().unwrap();
        rx.recv().unwrap()
    }
}

impl Watcher for INotifyWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, config: Config) -> Result<Self> {
        Self::from_event_handler(Box::new(event_handler), None, config.follow_symlinks())
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = bounded(1);
        self.channel.send(EventLoopMsg::Configure(config, tx))?;
        self.waker.wake()?;
        rx.recv()?
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Inotify
    }
}

impl Drop for INotifyWatcher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests;
