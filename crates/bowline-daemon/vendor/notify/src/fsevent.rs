//! Watcher implementation for Darwin's FSEvents API
//!
//! The FSEvents API provides a mechanism to notify clients about directories they ought to re-scan
//! in order to keep their internal data structures up-to-date with respect to the true state of
//! the file system. (For example, when files or directories are created, modified, or removed.) It
//! sends these notifications "in bulk", possibly notifying the client of changes to several
//! directories in a single callback.
//!
//! For more information see the [FSEvents API reference][ref].
//!
//! TODO: document event translation
//!
//! [ref]: https://developer.apple.com/library/mac/documentation/Darwin/Reference/FSEvents_Ref/

#![allow(non_upper_case_globals, dead_code)]

use crate::event::*;
use crate::{
    unbounded, Config, Error, EventHandler, PathsMut, Receiver, RecursiveMode, Result, Sender,
    Watcher,
};
use fsevent_sys as fs;
use fsevent_sys::core_foundation as cf;
use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::os::raw;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

mod coverage;
pub use coverage::{
    FsEventCoverageEvent, FsEventCoverageFlags, FsEventCoverageHandler, FsEventCoverageSignal,
    FsEventCursor,
};

bitflags::bitflags! {
  #[repr(C)]
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  struct StreamFlags: u32 {
    const NONE = fs::kFSEventStreamEventFlagNone;
    const MUST_SCAN_SUBDIRS = fs::kFSEventStreamEventFlagMustScanSubDirs;
    const USER_DROPPED = fs::kFSEventStreamEventFlagUserDropped;
    const KERNEL_DROPPED = fs::kFSEventStreamEventFlagKernelDropped;
    const IDS_WRAPPED = fs::kFSEventStreamEventFlagEventIdsWrapped;
    const HISTORY_DONE = fs::kFSEventStreamEventFlagHistoryDone;
    const ROOT_CHANGED = fs::kFSEventStreamEventFlagRootChanged;
    const MOUNT = fs::kFSEventStreamEventFlagMount;
    const UNMOUNT = fs::kFSEventStreamEventFlagUnmount;
    const ITEM_CREATED = fs::kFSEventStreamEventFlagItemCreated;
    const ITEM_REMOVED = fs::kFSEventStreamEventFlagItemRemoved;
    const INODE_META_MOD = fs::kFSEventStreamEventFlagItemInodeMetaMod;
    const ITEM_RENAMED = fs::kFSEventStreamEventFlagItemRenamed;
    const ITEM_MODIFIED = fs::kFSEventStreamEventFlagItemModified;
    const FINDER_INFO_MOD = fs::kFSEventStreamEventFlagItemFinderInfoMod;
    const ITEM_CHANGE_OWNER = fs::kFSEventStreamEventFlagItemChangeOwner;
    const ITEM_XATTR_MOD = fs::kFSEventStreamEventFlagItemXattrMod;
    const IS_FILE = fs::kFSEventStreamEventFlagItemIsFile;
    const IS_DIR = fs::kFSEventStreamEventFlagItemIsDir;
    const IS_SYMLINK = fs::kFSEventStreamEventFlagItemIsSymlink;
    const OWN_EVENT = fs::kFSEventStreamEventFlagOwnEvent;
    const IS_HARDLINK = fs::kFSEventStreamEventFlagItemIsHardlink;
    const IS_LAST_HARDLINK = fs::kFSEventStreamEventFlagItemIsLastHardlink;
    const ITEM_CLONED = fs::kFSEventStreamEventFlagItemCloned;
  }
}

/// FSEvents-based `Watcher` implementation
pub struct FsEventWatcher {
    paths: cf::CFMutableArrayRef,
    since_when: fs::FSEventStreamEventId,
    latency: cf::CFTimeInterval,
    flags: fs::FSEventStreamCreateFlags,
    event_handler: Arc<Mutex<dyn EventHandler>>,
    coverage_handler: Option<Arc<Mutex<dyn FsEventCoverageHandler>>>,
    runloop: Option<(cf::CFRunLoopRef, thread::JoinHandle<()>, Arc<AtomicBool>)>,
    active_stream: Option<RetainedStreamRef>,
    worker_exit: Option<Receiver<()>>,
    recursive_info: HashMap<PathBuf, bool>,
}

struct RetainedStreamRef(fs::FSEventStreamRef);

unsafe impl Send for RetainedStreamRef {}
unsafe impl Sync for RetainedStreamRef {}

impl RetainedStreamRef {
    unsafe fn new(stream: fs::FSEventStreamRef) -> Self {
        fs::FSEventStreamRetain(stream);
        Self(stream)
    }
}

impl Drop for RetainedStreamRef {
    fn drop(&mut self) {
        unsafe {
            fs::FSEventStreamRelease(self.0);
        }
    }
}

impl fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FsEventWatcher")
            .field("paths", &self.paths)
            .field("since_when", &self.since_when)
            .field("latency", &self.latency)
            .field("flags", &self.flags)
            .field("event_handler", &Arc::as_ptr(&self.event_handler))
            .field(
                "coverage_handler",
                &self.coverage_handler.as_ref().map(Arc::as_ptr),
            )
            .field("runloop", &self.runloop)
            .field("recursive_info", &self.recursive_info)
            .finish()
    }
}

// CFMutableArrayRef is a type alias to *mut libc::c_void, so FsEventWatcher is not Send/Sync
// automatically. It's Send because the pointer is not used in other threads.
unsafe impl Send for FsEventWatcher {}

// It's Sync because all methods that change the mutable state use `&mut self`.
unsafe impl Sync for FsEventWatcher {}

fn translate_flags(flags: StreamFlags, precise: bool) -> Vec<Event> {
    let mut evs = Vec::new();

    // «Denotes a sentinel event sent to mark the end of the "historical" events
    // sent as a result of specifying a `sinceWhen` value in the FSEvents.Create
    // call that created this event stream. After invoking the client's callback
    // with all the "historical" events that occurred before now, the client's
    // callback will be invoked with an event where the HistoryDone flag is set.
    // The client should ignore the path supplied in this callback.»
    // — https://www.mbsplugins.eu/FSEventsNextEvent.shtml
    //
    // As a result, we just stop processing here and return an empty vec, which
    // will ignore this completely and not emit any Events whatsoever.
    if flags.contains(StreamFlags::HISTORY_DONE) {
        return evs;
    }

    // FSEvents provides two possible hints as to why events were dropped,
    // however documentation on what those mean is scant, so we just pass them
    // through in the info attr field. The intent is clear enough, and the
    // additional information is provided if the user wants it.
    if flags.contains(StreamFlags::MUST_SCAN_SUBDIRS) {
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        evs.push(if flags.contains(StreamFlags::USER_DROPPED) {
            e.set_info("rescan: user dropped")
        } else if flags.contains(StreamFlags::KERNEL_DROPPED) {
            e.set_info("rescan: kernel dropped")
        } else {
            e
        });
    }

    // In imprecise mode, let's not even bother parsing the kind of the event
    // except for the above very special events.
    if !precise {
        evs.push(Event::new(EventKind::Any));
        return evs;
    }

    // This is most likely a rename or a removal. We assume rename but may want
    // to figure out if it was a removal some way later (TODO). To denote the
    // special nature of the event, we add an info string.
    if flags.contains(StreamFlags::ROOT_CHANGED) {
        evs.push(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .set_info("root changed"),
        );
    }

    // A path was mounted at the event path; we treat that as a create.
    if flags.contains(StreamFlags::MOUNT) {
        evs.push(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }

    // A path was unmounted at the event path; we treat that as a remove.
    if flags.contains(StreamFlags::UNMOUNT) {
        evs.push(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("mount"));
    }

    if flags.contains(StreamFlags::ITEM_CREATED) {
        evs.push(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Create(CreateKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Create(CreateKind::File))
        } else {
            let e = Event::new(EventKind::Create(CreateKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Create(CreateKind::Any))
            }
        });
    }

    if flags.contains(StreamFlags::ITEM_REMOVED) {
        evs.push(if flags.contains(StreamFlags::IS_DIR) {
            Event::new(EventKind::Remove(RemoveKind::Folder))
        } else if flags.contains(StreamFlags::IS_FILE) {
            Event::new(EventKind::Remove(RemoveKind::File))
        } else {
            let e = Event::new(EventKind::Remove(RemoveKind::Other));
            if flags.contains(StreamFlags::IS_SYMLINK) {
                e.set_info("is: symlink")
            } else if flags.contains(StreamFlags::IS_HARDLINK) {
                e.set_info("is: hardlink")
            } else if flags.contains(StreamFlags::ITEM_CLONED) {
                e.set_info("is: clone")
            } else {
                Event::new(EventKind::Remove(RemoveKind::Any))
            }
        });
    }

    // FSEvents provides no mechanism to associate the old and new sides of a
    // rename event.
    if flags.contains(StreamFlags::ITEM_RENAMED) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }

    // This is only described as "metadata changed", but it may be that it's
    // only emitted for some more precise subset of events... if so, will need
    // amending, but for now we have an Any-shaped bucket to put it in.
    if flags.contains(StreamFlags::INODE_META_MOD) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }

    if flags.contains(StreamFlags::FINDER_INFO_MOD) {
        evs.push(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }

    if flags.contains(StreamFlags::ITEM_CHANGE_OWNER) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }

    if flags.contains(StreamFlags::ITEM_XATTR_MOD) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }

    // This is specifically described as a data change, which we take to mean
    // is a content change.
    if flags.contains(StreamFlags::ITEM_MODIFIED) {
        evs.push(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }

    if flags.contains(StreamFlags::OWN_EVENT) {
        for ev in &mut evs {
            *ev = std::mem::take(ev).set_process_id(std::process::id());
        }
    }

    evs
}

struct StreamContextInfo {
    event_handler: Arc<Mutex<dyn EventHandler>>,
    coverage_handler: Option<Arc<Mutex<dyn FsEventCoverageHandler>>>,
    recursive_info: HashMap<PathBuf, bool>,
}

// Free the context when the stream created by `FSEventStreamCreate` is released.
extern "C" fn release_context(info: *const libc::c_void) {
    // Safety:
    // - The [documentation] for `FSEventStreamContext` states that `release` is only
    //   called when the stream is deallocated, so it is safe to convert `info` back into a
    //   box and drop it.
    //
    // [docs]: https://developer.apple.com/documentation/coreservices/fseventstreamcontext?language=objc
    unsafe {
        drop(Box::from_raw(
            info as *const StreamContextInfo as *mut StreamContextInfo,
        ));
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopWakeUp(runloop: cf::CFRunLoopRef);
    fn CFRunLoopRunInMode(
        mode_name: cf::CFStringRef,
        seconds: cf::CFTimeInterval,
        return_after_source_handled: cf::Boolean,
    ) -> i32;
}

struct FsEventPathsMut<'a>(&'a mut FsEventWatcher);
impl<'a> FsEventPathsMut<'a> {
    fn new(watcher: &'a mut FsEventWatcher) -> Self {
        watcher.stop();
        Self(watcher)
    }
}
impl PathsMut for FsEventPathsMut<'_> {
    fn add(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.0.append_path(path, recursive_mode)
    }

    fn remove(&mut self, path: &Path) -> Result<()> {
        self.0.remove_path(path)
    }

    fn commit(self: Box<Self>) -> Result<()> {
        // ignore return error: may be empty path list
        let _ = self.0.run();
        Ok(())
    }
}

impl FsEventWatcher {
    fn from_event_handler(event_handler: Arc<Mutex<dyn EventHandler>>) -> Result<Self> {
        Ok(FsEventWatcher {
            paths: unsafe {
                cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks)
            },
            since_when: fs::kFSEventStreamEventIdSinceNow,
            latency: 0.0,
            flags: fs::kFSEventStreamCreateFlagFileEvents | fs::kFSEventStreamCreateFlagNoDefer,
            event_handler,
            coverage_handler: None,
            runloop: None,
            active_stream: None,
            worker_exit: None,
            recursive_info: HashMap::new(),
        })
    }

    /// Create an FSEvents watcher that starts after an explicit journal cursor
    /// and reports native coverage observations on a separate control lane.
    pub fn new_with_coverage<F, C>(
        event_handler: F,
        coverage_handler: C,
        cursor: FsEventCursor,
        _config: Config,
    ) -> Result<Self>
    where
        F: EventHandler,
        C: FsEventCoverageHandler,
    {
        Ok(FsEventWatcher {
            paths: unsafe {
                cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks)
            },
            since_when: cursor.get(),
            latency: 0.0,
            flags: fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot,
            event_handler: Arc::new(Mutex::new(event_handler)),
            coverage_handler: Some(Arc::new(Mutex::new(coverage_handler))),
            runloop: None,
            active_stream: None,
            worker_exit: None,
            recursive_info: HashMap::new(),
        })
    }

    fn watch_inner(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.stop();
        let result = self.append_path(path, recursive_mode);
        // ignore return error: may be empty path list
        let _ = self.run();
        result
    }

    fn unwatch_inner(&mut self, path: &Path) -> Result<()> {
        self.stop();
        let result = self.remove_path(path);
        // ignore return error: may be empty path list
        let _ = self.run();
        result
    }

    #[inline]
    fn is_running(&self) -> bool {
        self.runloop.is_some()
    }

    fn stop(&mut self) {
        if let Err(error) = self.stop_result() {
            log::error!("failed to stop FSEvents worker: {error}");
        }
    }

    fn stop_result(&mut self) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }

        if let Some((runloop, thread_handle, shutdown_requested)) = self.runloop.take() {
            shutdown_requested.store(true, Ordering::Release);
            if !thread_handle.is_finished() {
                unsafe {
                    let runloop = runloop as *mut raw::c_void;
                    cf::CFRunLoopStop(runloop);
                    CFRunLoopWakeUp(runloop);
                }
            }

            // Wait for the thread to shut down.
            thread_handle
                .join()
                .map_err(|_| Error::generic("FSEvents worker panicked during shutdown"))?;
        }
        self.active_stream = None;
        Ok(())
    }

    /// Stop the native stream and join its run-loop worker.
    pub fn shutdown(&mut self) -> Result<()> {
        self.stop_result()
    }

    /// Whether the owned FSEvents run-loop worker has exited.
    pub fn worker_is_finished(&self) -> bool {
        self.runloop
            .as_ref()
            .map_or(true, |(_, worker, _)| worker.is_finished())
    }

    /// Return a signal that fires whenever the owned run-loop worker exits,
    /// including unwinding before the ordinary lifecycle callback can run.
    pub fn take_worker_exit_receiver(&mut self) -> Option<Receiver<()>> {
        self.worker_exit.take()
    }

    /// Flush all native events generated before this call and wait until their
    /// callbacks have returned.
    pub fn flush_sync(&mut self) -> Result<()> {
        if self.worker_is_finished() {
            return Err(Error::generic("FSEvents worker is not running"));
        }
        let stream = self
            .active_stream
            .as_ref()
            .ok_or_else(|| Error::generic("FSEvents stream is not active"))?;
        unsafe {
            fs::FSEventStreamFlushSync(stream.0);
        }
        if self.worker_is_finished() {
            return Err(Error::generic("FSEvents worker stopped during flush"));
        }
        Ok(())
    }

    fn remove_path(&mut self, path: &Path) -> Result<()> {
        let str_path = path
            .to_str()
            .ok_or_else(|| Error::generic("FSEvents watch path is not valid UTF-8"))?;
        unsafe {
            let mut err: cf::CFErrorRef = ptr::null_mut();
            let cf_path = cf::str_path_to_cfstring_ref(str_path, &mut err);
            if cf_path.is_null() {
                cf::CFRelease(err as cf::CFRef);
                return Err(Error::watch_not_found().add_path(path.into()));
            }

            let mut to_remove = Vec::new();
            for idx in 0..cf::CFArrayGetCount(self.paths) {
                let item = cf::CFArrayGetValueAtIndex(self.paths, idx);
                if cf::CFStringCompare(item, cf_path, cf::kCFCompareCaseInsensitive)
                    == cf::kCFCompareEqualTo
                {
                    to_remove.push(idx);
                }
            }

            cf::CFRelease(cf_path);

            for idx in to_remove.iter().rev() {
                cf::CFArrayRemoveValueAtIndex(self.paths, *idx);
            }
        }
        let p = if let Ok(canonicalized_path) = path.canonicalize() {
            canonicalized_path
        } else {
            path.to_owned()
        };
        match self.recursive_info.remove(&p) {
            Some(_) => Ok(()),
            None => Err(Error::watch_not_found()),
        }
    }

    // https://github.com/thibaudgg/rb-fsevent/blob/master/ext/fsevent_watch/main.c
    fn append_path(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        if !path.exists() {
            return Err(Error::path_not_found().add_path(path.into()));
        }
        let canonical_path = path.to_path_buf().canonicalize()?;
        let str_path = path
            .to_str()
            .ok_or_else(|| Error::generic("FSEvents watch path is not valid UTF-8"))?;
        unsafe {
            let mut err: cf::CFErrorRef = ptr::null_mut();
            let cf_path = cf::str_path_to_cfstring_ref(str_path, &mut err);
            if cf_path.is_null() {
                // Most likely the directory was deleted, or permissions changed,
                // while the above code was running.
                cf::CFRelease(err as cf::CFRef);
                return Err(Error::path_not_found().add_path(path.into()));
            }
            cf::CFArrayAppendValue(self.paths, cf_path);
            cf::CFRelease(cf_path);
        }
        self.recursive_info
            .insert(canonical_path, recursive_mode.is_recursive());
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        if unsafe { cf::CFArrayGetCount(self.paths) } == 0 {
            // TODO: Reconstruct and add paths to error
            return Err(Error::path_not_found());
        }

        // We need to associate the stream context with our callback in order to propagate events
        // to the rest of the system. This will be owned by the stream, and will be freed when the
        // stream is closed. This means we will leak the context if we panic before reaching
        // `FSEventStreamRelease`.
        let context = Box::into_raw(Box::new(StreamContextInfo {
            event_handler: self.event_handler.clone(),
            coverage_handler: self.coverage_handler.clone(),
            recursive_info: self.recursive_info.clone(),
        }));

        let stream_context = fs::FSEventStreamContext {
            version: 0,
            info: context as *mut libc::c_void,
            retain: None,
            release: Some(release_context),
            copy_description: None,
        };

        let stream = unsafe {
            fs::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                callback,
                &stream_context,
                self.paths,
                self.since_when,
                self.latency,
                self.flags,
            )
        };

        if stream.is_null() {
            unsafe {
                drop(Box::from_raw(context));
            }
            return Err(Error::generic("FSEvents stream creation failed"));
        }
        let retained_stream = unsafe { RetainedStreamRef::new(stream) };

        // Wrapper to help send CFRef types across threads.
        struct CFSendWrapper(cf::CFRef);

        // Safety:
        // - According to the Apple documentation, it's safe to move `CFRef`s across threads.
        //   https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/ThreadSafetySummary/ThreadSafetySummary.html
        unsafe impl Send for CFSendWrapper {}

        struct CFStreamOwner(fs::FSEventStreamRef);

        unsafe impl Send for CFStreamOwner {}

        impl Drop for CFStreamOwner {
            fn drop(&mut self) {
                unsafe {
                    fs::FSEventStreamInvalidate(self.0);
                    fs::FSEventStreamRelease(self.0);
                }
            }
        }

        // move into thread
        let stream = CFStreamOwner(stream);
        let lifecycle_handler = self.coverage_handler.clone();

        // channel to pass runloop around
        let (rl_tx, rl_rx) = unbounded();
        let (worker_exit_tx, worker_exit_rx) = unbounded();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown_requested);
        self.worker_exit = Some(worker_exit_rx);

        let thread_handle = thread::Builder::new()
            .name("notify-rs fsevents loop".to_string())
            .spawn(move || {
                struct WorkerExitSignal(Sender<()>);

                impl Drop for WorkerExitSignal {
                    fn drop(&mut self) {
                        let _ = self.0.send(());
                    }
                }

                let _worker_exit = WorkerExitSignal(worker_exit_tx);
                let _ = &stream;
                let stream = stream.0;

                unsafe {
                    let cur_runloop = cf::CFRunLoopGetCurrent();

                    fs::FSEventStreamScheduleWithRunLoop(
                        stream,
                        cur_runloop,
                        cf::kCFRunLoopDefaultMode,
                    );
                    if fs::FSEventStreamStart(stream) == 0 {
                        let _ = rl_tx.send(Err(Error::generic("FSEvents stream start failed")));
                        return;
                    }

                    if let Some(handler) = &lifecycle_handler {
                        let mut handler = handler
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        handler.handle_coverage(FsEventCoverageSignal::Started);
                    }

                    // the calling to CFRunLoopRun will be terminated by CFRunLoopStop call in drop()
                    if rl_tx.send(Ok(CFSendWrapper(cur_runloop))).is_err() {
                        fs::FSEventStreamStop(stream);
                        return;
                    }

                    // A bounded run-loop turn closes the stop-before-run race:
                    // shutdown never depends on `CFRunLoopStop` being observed
                    // before this worker enters its first run-loop call.
                    while !worker_shutdown.load(Ordering::Acquire) {
                        CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.1, 0);
                    }
                    fs::FSEventStreamStop(stream);
                    if let Some(handler) = &lifecycle_handler {
                        let mut handler = handler
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        handler.handle_coverage(FsEventCoverageSignal::Stopped);
                    }
                }
            })?;
        match rl_rx.recv() {
            Ok(Ok(runloop)) => {
                self.runloop = Some((runloop.0, thread_handle, shutdown_requested));
                self.active_stream = Some(retained_stream);
            }
            Ok(Err(error)) => {
                thread_handle
                    .join()
                    .map_err(|_| Error::generic("FSEvents worker panicked during startup"))?;
                return Err(error);
            }
            Err(error) => {
                thread_handle
                    .join()
                    .map_err(|_| Error::generic("FSEvents worker panicked during startup"))?;
                return Err(error.into());
            }
        }

        Ok(())
    }

    fn configure_raw_mode(&mut self, _config: Config, tx: Sender<Result<bool>>) {
        tx.send(Ok(false))
            .expect("configuration channel disconnect");
    }
}

extern "C" fn callback(
    stream_ref: fs::FSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                        // size_t numEvents
    event_paths: *mut libc::c_void,                  // void *eventPaths
    event_flags: *const fs::FSEventStreamEventFlags, // const FSEventStreamEventFlags eventFlags[]
    event_ids: *const fs::FSEventStreamEventId,      // const FSEventStreamEventId eventIds[]
) {
    unsafe {
        callback_impl(
            stream_ref,
            info,
            num_events,
            event_paths,
            event_flags,
            event_ids,
        )
    }
}

unsafe fn callback_impl(
    _stream_ref: fs::FSEventStreamRef,
    info: *mut libc::c_void,
    num_events: libc::size_t,                        // size_t numEvents
    event_paths: *mut libc::c_void,                  // void *eventPaths
    event_flags: *const fs::FSEventStreamEventFlags, // const FSEventStreamEventFlags eventFlags[]
    event_ids: *const fs::FSEventStreamEventId,      // const FSEventStreamEventId eventIds[]
) {
    let event_paths = event_paths as *const *const libc::c_char;
    let info = info as *const StreamContextInfo;
    let event_handler = &(*info).event_handler;
    let coverage_handler = &(*info).coverage_handler;

    for p in 0..num_events {
        let raw_flag = *event_flags.add(p);
        let flag = StreamFlags::from_bits_retain(raw_flag);
        let coverage_event = FsEventCoverageEvent::new(
            FsEventCursor::from_raw(*event_ids.add(p)),
            FsEventCoverageFlags::from_stream_flags(flag),
        );
        if let Some(handler) = coverage_handler {
            let mut handler = handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handler.handle_coverage(FsEventCoverageSignal::Event(coverage_event));
        }
        let path = PathBuf::from(OsStr::from_bytes(
            CStr::from_ptr(*event_paths.add(p)).to_bytes(),
        ));

        let mut handle_event = false;
        for (p, r) in &(*info).recursive_info {
            if path.starts_with(p) {
                if *r || &path == p {
                    handle_event = true;
                    break;
                } else if let Some(parent_path) = path.parent() {
                    if parent_path == p {
                        handle_event = true;
                        break;
                    }
                }
            }
        }

        if !handle_event {
            continue;
        }

        log::trace!("FSEvent: path = `{}`, flag = {:?}", path.display(), flag);

        for ev in translate_flags(flag, true).into_iter() {
            // TODO: precise
            let ev = ev.add_path(path.clone());
            let mut event_handler = event_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            event_handler.handle_event(Ok(ev));
        }
    }
}

#[cfg(test)]
#[path = "fsevent/bowline_coverage_tests.rs"]
mod bowline_coverage_tests;

impl Watcher for FsEventWatcher {
    /// Create a new watcher.
    fn new<F: EventHandler>(event_handler: F, _config: Config) -> Result<Self> {
        Self::from_event_handler(Arc::new(Mutex::new(event_handler)))
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        self.watch_inner(path, recursive_mode)
    }

    fn paths_mut<'me>(&'me mut self) -> Box<dyn PathsMut + 'me> {
        Box::new(FsEventPathsMut::new(self))
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.unwatch_inner(path)
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        let (tx, rx) = unbounded();
        self.configure_raw_mode(config, tx);
        rx.recv()?
    }

    fn kind() -> crate::WatcherKind {
        crate::WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            cf::CFRelease(self.paths);
        }
    }
}

#[cfg(test)]
#[path = "fsevent/upstream_tests.rs"]
mod upstream_tests;
