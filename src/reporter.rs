use crate::protocol::{self, Record};
use crate::{NameSize, MAX_NODES, MAX_TOTAL};
use std::fs::File;
use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::unix;

/// Reporter settings. Use `None` for `refresh_interval` to send reports manually.
#[derive(Clone, Debug)]
pub struct Options {
    /// Root task label.
    pub root_name: String,
    /// Estimated root item count. Zero means unknown.
    pub estimated_total_items: u64,
    /// Time between automatic updates. Default: 80 ms.
    pub refresh_interval: Option<Duration>,
    /// Delay before the first automatic update. Default: 200 ms.
    pub initial_delay: Duration,
    /// Disable all reporting, including when a channel is present.
    pub disabled: bool,
    /// Wire format. All processes in a tree must use the same value.
    pub name_size: NameSize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            root_name: String::new(),
            estimated_total_items: 0,
            refresh_interval: Some(Duration::from_millis(80)),
            initial_delay: Duration::from_millis(200),
            disabled: false,
            name_size: NameSize::Current,
        }
    }
}

impl Options {
    /// Create settings with a root name and item estimate.
    pub fn new(name: impl Into<String>, estimated_total_items: u64) -> Self {
        Self {
            root_name: name.into(),
            estimated_total_items,
            ..Self::default()
        }
    }

    /// Create a reporter using `ZIG_PROGRESS`. An unusable channel disables it.
    pub fn build(self) -> io::Result<Progress> {
        self.validate()?;
        #[cfg(unix)]
        {
            let mut registry = unix::registry();
            let output = if self.disabled {
                None
            } else {
                unix::environment_channel()
            };
            let progress = self.create(output)?;
            registry.retain(|item| item.strong_count() != 0);
            registry.push(Arc::downgrade(&progress.inner));
            Ok(progress)
        }
        #[cfg(not(unix))]
        self.create(None)
    }

    /// Create a reporter from a writable pipe. The descriptor is duplicated;
    /// the caller retains ownership. Both descriptors share nonblocking mode.
    /// An invalid explicit channel returns an error, unless reporting is disabled.
    #[cfg(unix)]
    pub fn build_with_fd(self, fd: std::os::fd::BorrowedFd<'_>) -> io::Result<Progress> {
        use std::os::fd::AsRawFd;
        self.validate()?;
        let mut registry = unix::registry();
        let output = if self.disabled {
            None
        } else {
            Some(unix::duplicate(fd.as_raw_fd(), false)?)
        };
        let progress = self.create(output)?;
        registry.retain(|item| item.strong_count() != 0);
        registry.push(Arc::downgrade(&progress.inner));
        Ok(progress)
    }

    fn validate(&self) -> io::Result<()> {
        let now = Instant::now();
        if self
            .refresh_interval
            .is_some_and(|d| d.is_zero() || now.checked_add(d).is_none())
            || now.checked_add(self.initial_delay).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid update duration",
            ));
        }
        Ok(())
    }

    fn create(self, output: Option<File>) -> io::Result<Progress> {
        let root = Entry {
            id: 0,
            parent: None,
            record: Record {
                completed: 0,
                total: total(self.estimated_total_items),
                name: protocol::name(&self.root_name, self.name_size),
                parent: 255,
            },
            child: None,
        };
        let inner = Arc::new(Inner {
            pid: std::process::id(),
            interval: self.refresh_interval,
            initial_delay: self.initial_delay,
            state: Mutex::new(State {
                output,
                nodes: vec![root],
                next_id: 1,
                size: self.name_size,
                pending: Vec::new(),
                offset: 0,
                error: None,
                paused: false,
            }),
            wake: Condvar::new(),
            worker: Mutex::new(None),
        });
        let progress = Progress { inner };
        let mut worker = lock(&progress.inner.worker);
        *worker = progress.inner.start_worker(true)?;
        drop(worker);
        Ok(progress)
    }
}

/// Owns a reporter and its update thread. Drop sends a final report and closes
/// its pipes. It does not wait for or terminate child processes.
#[derive(Debug)]
pub struct Progress {
    pub(crate) inner: Arc<Inner>,
}

impl Progress {
    /// Create a reporter with default settings and a channel from `ZIG_PROGRESS`.
    pub fn new(name: impl Into<String>, estimated_total_items: u64) -> io::Result<Self> {
        Options::new(name, estimated_total_items).build()
    }

    /// Get a handle to the root. Dropping a root handle does not close the reporter.
    pub fn root(&self) -> Node {
        Node {
            handle: Some(Arc::new(Handle {
                inner: self.inner.clone(),
                id: 0,
            })),
        }
    }

    /// Return true while this process has an open reporting channel.
    pub fn enabled(&self) -> bool {
        self.inner.local() && lock(&self.inner.state).output.is_some()
    }

    /// Collect child reports and try to send a current snapshot without waiting
    /// for pipe space. `false` means disabled or incomplete. Partial packets are
    /// finished before a newer packet is sent. Permanent I/O errors are returned.
    pub fn flush(&self) -> io::Result<bool> {
        self.inner.flush()
    }

    /// Return the first permanent output error, if any.
    pub fn error(&self) -> Option<io::Error> {
        if !self.inner.local() {
            return None;
        }
        lock(&self.inner.state).error.as_ref().map(copy_error)
    }

    /// Stop updates, try to send the final state, and close all owned pipes.
    /// This is idempotent. Final delivery is best effort if the pipe is full.
    pub fn close(&self) -> io::Result<()> {
        self.inner.close()
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// A task handle. Clones refer to the same task. Dropping the last handle ends
/// a non-root task. Explicit `end()` affects all clones and is idempotent.
/// Operations on disabled or ended tasks have no effect, except process creation.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub(crate) handle: Option<Arc<Handle>>,
}

#[derive(Debug)]
pub(crate) struct Handle {
    pub inner: Arc<Inner>,
    pub id: u64,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.id != 0 {
            self.inner.end(self.id);
        }
    }
}

impl Node {
    /// Start a child task. At the 253-node limit, return a disabled handle.
    #[must_use = "dropping the handle ends the task"]
    pub fn start(&self, name: &str, estimated_total_items: u64) -> Self {
        let Some(handle) = self.local_handle() else {
            return Self::default();
        };
        let mut state = lock(&handle.inner.state);
        if !state.active(handle.id) || state.nodes.len() == MAX_NODES || state.next_id == u64::MAX {
            return Self::default();
        }
        let id = state.next_id;
        state.next_id += 1;
        let record = Record {
            completed: 0,
            total: total(estimated_total_items),
            name: protocol::name(name, state.size),
            parent: 0,
        };
        state.nodes.push(Entry {
            id,
            parent: Some(handle.id),
            record,
            child: None,
        });
        Self {
            handle: Some(Arc::new(Handle {
                inner: handle.inner.clone(),
                id,
            })),
        }
    }

    /// Return true if the task is active and its channel is open.
    pub fn enabled(&self) -> bool {
        self.local_handle()
            .is_some_and(|h| lock(&h.inner.state).active(h.id))
    }

    /// Complete one item. Counts saturate at `u32::MAX`.
    pub fn complete_one(&self) {
        self.add(1);
    }

    /// Add completed items. Counts saturate at `u32::MAX`.
    pub fn add(&self, count: u64) {
        self.update(|r, _| r.completed = r.completed.saturating_add(completed(count)));
    }

    /// Set the completed item count. It can exceed the estimate.
    pub fn set_completed_items(&self, count: u64) {
        self.update(|r, _| r.completed = completed(count));
    }

    /// Set the estimated total, capped at [`MAX_TOTAL`]. Zero means unknown.
    pub fn set_estimated_total_items(&self, count: u64) {
        self.update(|r, _| r.total = total(count));
    }

    /// Increase the estimated total, capped at [`MAX_TOTAL`].
    pub fn increase_estimated_total_items(&self, count: u64) {
        self.update(|r, _| r.total = r.total.saturating_add(total(count)).min(MAX_TOTAL));
    }

    /// Set a UTF-8 label. Stop at the first NUL and truncate at a character boundary.
    pub fn set_name(&self, name: &str) {
        self.update(|r, size| r.name = protocol::name(name, size));
    }

    /// Remove this branch and complete one item in its parent. Ending the root
    /// closes the reporter. This method does not indicate success or failure.
    pub fn end(&self) {
        if let Some(h) = self.local_handle() {
            h.inner.end(h.id);
        }
    }

    /// Start a standard command with a private progress pipe. The child's root
    /// replaces this task in reports; its final tree remains until `end()`.
    /// Standard streams, arguments, directory, and other environment values are
    /// preserved. The command is consumed because its exec hook owns a descriptor.
    /// Returns a normal `Child`; the caller must wait for it.
    ///
    /// One process can attach to each active task. Failed spawns leave it reusable.
    /// Disabled tasks still spawn, with `ZIG_PROGRESS` removed from the child.
    /// Exec hooks must not call this library or change its private descriptor.
    pub fn spawn(&self, command: Command) -> io::Result<Child> {
        #[cfg(unix)]
        {
            unix::spawn(self, command)
        }
        #[cfg(not(unix))]
        {
            let mut command = command;
            command.env_remove("ZIG_PROGRESS").spawn()
        }
    }

    pub(crate) fn local_handle(&self) -> Option<&Handle> {
        self.handle.as_deref().filter(|h| h.inner.local())
    }

    fn update(&self, update: impl FnOnce(&mut Record, NameSize)) {
        if let Some(h) = self.local_handle() {
            let mut state = lock(&h.inner.state);
            let size = state.size;
            if state.output.is_some() {
                if let Some(entry) = state.nodes.iter_mut().find(|e| e.id == h.id) {
                    update(&mut entry.record, size);
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub pid: u32,
    pub interval: Option<Duration>,
    pub initial_delay: Duration,
    pub state: Mutex<State>,
    pub wake: Condvar,
    pub worker: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    pub fn local(&self) -> bool {
        self.pid == std::process::id()
    }

    pub fn start_worker(self: &Arc<Self>, initial: bool) -> io::Result<Option<JoinHandle<()>>> {
        let Some(interval) = self.interval else {
            return Ok(None);
        };
        if lock(&self.state).output.is_none() {
            return Ok(None);
        }
        let inner = self.clone();
        let delay = if initial {
            self.initial_delay
        } else {
            interval
        };
        thread::Builder::new()
            .name("zig-progress".into())
            .spawn(move || {
                let mut state = lock(&inner.state);
                let mut delay = delay;
                loop {
                    let (next, _) = inner
                        .wake
                        .wait_timeout_while(state, delay, |s| s.output.is_some() && !s.paused)
                        .unwrap_or_else(|e| e.into_inner());
                    state = next;
                    if state.output.is_none() || state.paused {
                        break;
                    }
                    let _ = state.flush();
                    delay = interval;
                }
            })
            .map(Some)
    }

    fn flush(&self) -> io::Result<bool> {
        if !self.local() {
            return Ok(false);
        }
        lock(&self.state).flush()
    }

    fn close(&self) -> io::Result<()> {
        if !self.local() {
            return Ok(());
        }
        let mut worker = lock(&self.worker);
        let result = {
            let mut state = lock(&self.state);
            let result = state.flush().map(|_| ());
            state.disconnect();
            self.wake.notify_all();
            result
        };
        if let Some(worker) = worker.take() {
            let _ = worker.join();
        }
        result
    }

    fn end(&self, id: u64) {
        if !self.local() {
            return;
        }
        if id == 0 {
            let _ = self.close();
            return;
        }
        let mut state = lock(&self.state);
        if !state.active(id) {
            return;
        }
        let parent = state
            .nodes
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.parent);
        let mut removed = vec![id];
        // Nodes are allocated after their parents, so one pass removes a branch.
        state.nodes.retain(|entry| {
            if entry.id == id || entry.parent.is_some_and(|p| removed.contains(&p)) {
                removed.push(entry.id);
                false
            } else {
                true
            }
        });
        if let Some(entry) = state.nodes.iter_mut().find(|e| Some(e.id) == parent) {
            entry.record.completed = entry.record.completed.saturating_add(1);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Entry {
    pub id: u64,
    pub parent: Option<u64>,
    pub record: Record,
    pub child: Option<ChildPipe>,
}

#[derive(Debug)]
pub(crate) struct State {
    pub output: Option<File>,
    pub nodes: Vec<Entry>,
    next_id: u64,
    pub size: NameSize,
    pending: Vec<u8>,
    offset: usize,
    pub error: Option<io::Error>,
    pub paused: bool,
}

impl State {
    pub fn active(&self, id: u64) -> bool {
        self.output.is_some() && self.nodes.iter().any(|entry| entry.id == id)
    }

    pub fn disconnect(&mut self) {
        self.output = None;
        self.pending.clear();
        self.offset = 0;
        for entry in &mut self.nodes {
            entry.child = None;
        }
    }

    fn snapshot(&mut self) -> Vec<u8> {
        let mut records: Vec<_> = self
            .nodes
            .iter()
            .map(|e| {
                let mut r = e.record.clone();
                r.parent = e.parent.map_or(255, |p| {
                    self.nodes.iter().position(|e| e.id == p).unwrap() as u8
                });
                r
            })
            .collect();
        for (i, entry) in self.nodes.iter_mut().enumerate() {
            let Some(child) = entry.child.as_mut() else {
                continue;
            };
            child.read(self.size);
            let Some(root) = child.snapshot.first() else {
                continue;
            };
            let mut root = root.clone();
            root.parent = records[i].parent;
            if root.name.is_empty() {
                root.name.clone_from(&records[i].name);
            }
            records[i] = root;
            let mut mapped = vec![0; child.snapshot.len()];
            mapped[0] = i as u8;
            for (j, record) in child.snapshot.iter().enumerate().skip(1) {
                if records.len() == MAX_NODES {
                    break;
                }
                mapped[j] = records.len() as u8;
                let mut record = record.clone();
                record.parent = mapped[usize::from(record.parent)];
                records.push(record);
            }
        }
        protocol::encode(&records, self.size)
    }

    fn flush(&mut self) -> io::Result<bool> {
        if let Some(error) = &self.error {
            return Err(copy_error(error));
        }
        if self.output.is_none() {
            return Ok(false);
        }
        let current = self.snapshot();
        if !self.pending.is_empty() {
            let same = self.pending == current;
            if !self.write_pending()? {
                return Ok(false);
            }
            if same {
                return Ok(true);
            }
        }
        self.pending = current;
        self.offset = 0;
        self.write_pending()
    }

    fn write_pending(&mut self) -> io::Result<bool> {
        let output = self.output.as_ref().unwrap();
        match write_pipe(output, &self.pending[self.offset..]) {
            Ok(0) => self.fail(io::Error::new(
                io::ErrorKind::WriteZero,
                "progress pipe wrote zero bytes",
            )),
            Ok(n) => {
                self.offset += n;
                if self.offset == self.pending.len() {
                    self.pending.clear();
                    self.offset = 0;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                // No bytes sent: a later snapshot can supersede this one.
                if self.offset == 0 {
                    self.pending.clear();
                }
                Ok(false)
            }
            Err(e) => self.fail(e),
        }
    }

    fn fail(&mut self, error: io::Error) -> io::Result<bool> {
        self.disconnect();
        self.error = Some(copy_error(&error));
        Err(error)
    }
}

#[derive(Debug)]
pub(crate) struct ChildPipe {
    pipe: Option<File>,
    pending: Vec<u8>,
    snapshot: Vec<Record>,
}

impl ChildPipe {
    pub fn new(pipe: File) -> Self {
        Self {
            pipe: Some(pipe),
            pending: Vec::new(),
            snapshot: Vec::new(),
        }
    }

    fn close(&mut self) {
        self.pipe = None;
        self.pending.clear();
    }

    fn read(&mut self, size: NameSize) {
        let mut buffer = [0; 4096];
        // Bound work per child so a noisy writer cannot block other tasks.
        for _ in 0..16 {
            let Some(pipe) = &self.pipe else {
                break;
            };
            match read_pipe(pipe, &mut buffer) {
                Ok(0) => {
                    self.close();
                    break;
                }
                Ok(n) => {
                    self.pending.extend_from_slice(&buffer[..n]);
                    while let Some(&count) = self.pending.first() {
                        if !(1..=MAX_NODES as u8).contains(&count) {
                            self.close();
                            return;
                        }
                        let length = 1 + usize::from(count) * (9 + size.bytes());
                        if self.pending.len() < length {
                            break;
                        }
                        let Some(records) = protocol::decode(&self.pending[..length], size) else {
                            self.close();
                            return;
                        };
                        self.snapshot = records;
                        self.pending.drain(..length);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break
                }
                Err(_) => {
                    self.close();
                    break;
                }
            }
        }
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn total(count: u64) -> u32 {
    count.min(u64::from(MAX_TOTAL)) as u32
}
fn completed(count: u64) -> u32 {
    count.min(u64::from(u32::MAX)) as u32
}
fn copy_error(error: &io::Error) -> io::Error {
    error.raw_os_error().map_or_else(
        || io::Error::new(error.kind(), error.to_string()),
        io::Error::from_raw_os_error,
    )
}

#[cfg(unix)]
use unix::{read_pipe, write_pipe};
#[cfg(not(unix))]
fn read_pipe(_: &File, _: &mut [u8]) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}
#[cfg(not(unix))]
fn write_pipe(_: &File, _: &[u8]) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(all(test, unix))]
mod tests;
