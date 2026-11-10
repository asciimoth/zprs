use crate::reporter::{lock, ChildPipe, Inner};
use crate::Node;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

static REPORTERS: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

pub(crate) fn registry() -> MutexGuard<'static, Vec<Weak<Inner>>> {
    lock(&REPORTERS)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

pub(crate) fn environment_channel() -> Option<File> {
    let value = std::env::var("ZIG_PROGRESS").ok()?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let fd = value.parse::<u16>().ok()?;
    duplicate(i32::from(fd), true).ok()
}

pub(crate) fn duplicate(source: RawFd, inherited: bool) -> io::Result<File> {
    // SAFETY: fcntl and fstat borrow the descriptor. The new descriptor is owned
    // by File immediately, including on validation failures.
    unsafe {
        let fd = cvt(libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3))?;
        let file = File::from_raw_fd(fd);
        let mut stat = std::mem::zeroed::<libc::stat>();
        cvt(libc::fstat(fd, &mut stat))?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Err(invalid("progress channel must be a pipe"));
        }
        let flags = cvt(libc::fcntl(fd, libc::F_GETFL))?;
        if flags & libc::O_ACCMODE == libc::O_RDONLY {
            return Err(invalid("progress pipe must be writable"));
        }
        cvt(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        if inherited {
            let flags = cvt(libc::fcntl(source, libc::F_GETFD))?;
            cvt(libc::fcntl(source, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
        }
        Ok(file)
    }
}

pub(crate) fn pipe() -> io::Result<(File, File)> {
    let mut fds = [-1; 2];
    // SAFETY: fds has room for both results. Each successful descriptor is
    // immediately owned, and cleanup also covers errors during configuration.
    unsafe {
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        cvt(libc::pipe2(
            fds.as_mut_ptr(),
            libc::O_CLOEXEC | libc::O_NONBLOCK,
        ))?;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )))]
        cvt(libc::pipe(fds.as_mut_ptr()))?;
        let mut files = [File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])];
        for file in &mut files {
            if file.as_raw_fd() < 3 {
                let fd = cvt(libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3))?;
                *file = File::from_raw_fd(fd);
            }
            let fd = file.as_raw_fd();
            cvt(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
            let flags = cvt(libc::fcntl(fd, libc::F_GETFL))?;
            cvt(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        }
        let [reader, writer] = files;
        if writer.as_raw_fd() > 65535 {
            return Err(invalid("progress descriptor exceeds 65535"));
        }
        Ok((reader, writer))
    }
}

pub(crate) fn read_pipe(file: &File, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: buffer is writable for its length and file owns the descriptor.
    let n = unsafe { libc::read(file.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

pub(crate) fn write_pipe(file: &File, buffer: &[u8]) -> io::Result<usize> {
    // Do not depend on the application's SIGPIPE disposition. Block it only on
    // this thread, consume a new EPIPE signal, and restore the previous mask.
    // SAFETY: all signal sets are initialized, and pointers remain valid for
    // each syscall. No process-wide signal handler is changed.
    unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        let mut old = std::mem::zeroed::<libc::sigset_t>();
        let mut pending = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        libc::sigpending(&mut pending);
        let was_pending = libc::sigismember(&pending, libc::SIGPIPE) == 1;
        let n = libc::write(file.as_raw_fd(), buffer.as_ptr().cast(), buffer.len());
        let error = if n < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        if error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::EPIPE) && !was_pending {
            // sigwait is portable to macOS. Only wait when the signal is pending;
            // SIG_IGN can discard it even when blocked.
            libc::sigpending(&mut pending);
            if libc::sigismember(&pending, libc::SIGPIPE) == 1 {
                let mut signal = 0;
                libc::sigwait(&set, &mut signal);
            }
        }
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        match error {
            Some(e) => Err(e),
            None => Ok(n as usize),
        }
    }
}

pub(crate) fn spawn(node: &Node, mut command: Command) -> io::Result<Child> {
    command.env_remove("ZIG_PROGRESS");
    let Some(handle) = node.local_handle() else {
        return command.spawn();
    };
    let mut state = lock(&handle.inner.state);
    if !state.active(handle.id) {
        return command.spawn();
    }
    let entry = state.nodes.iter_mut().find(|e| e.id == handle.id).unwrap();
    if entry.child.is_some() {
        return Err(invalid("a process is already attached to this task"));
    }
    let (reader, writer) = pipe()?;
    command.env("ZIG_PROGRESS", writer.as_raw_fd().to_string());
    // SAFETY: this hook only calls async-signal-safe fcntl. The closure owns the
    // writer until spawn returns, so its descriptor cannot be closed or reused.
    unsafe {
        command.pre_exec(move || {
            cvt(libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0))?;
            Ok(())
        });
    }
    let child = command.spawn()?;
    entry.child = Some(ChildPipe::new(reader));
    Ok(child)
}

/// Result of [`Node::fork`]. The parent must reap the PID with `waitpid`.
#[derive(Debug)]
pub enum ForkResult {
    /// The parent branch, with the child's process ID.
    Parent {
        /// Child process ID, suitable for `libc::waitpid`.
        pid: libc::pid_t,
    },
    /// The child branch. Create a fresh reporter with `Options::build_with_fd`
    /// and this channel. `None` means that the source task was disabled.
    /// The child environment has no `ZIG_PROGRESS`; it cannot use the upstream
    /// channel by accident. The caller owns this descriptor and must close it.
    Child {
        /// Private pipe to the parent task, or `None` for a disabled task.
        channel: Option<OwnedFd>,
    },
}

impl Node {
    /// Fork with a private channel attached to this task. All library update
    /// threads stop before the fork and restart in the parent. Inherited
    /// reporters are disabled and their owned pipes are closed in the child.
    /// The parent environment is unchanged. A failed fork leaves the task free.
    ///
    /// # Safety
    /// The caller must ensure that no other application threads exist at the
    /// fork, and that signal handlers and at-fork hooks do not call this crate.
    /// This method stops this crate's threads only. Other libraries must permit
    /// continued execution after fork. Caller-owned descriptors remain the
    /// caller's responsibility. Close the child's reporter before `_exit`,
    /// which does not run destructors. Prefer `spawn` for multithreaded programs.
    pub unsafe fn fork(&self) -> io::Result<ForkResult> {
        let mut registry = registry();
        let reporters: Vec<_> = registry
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|p| p.local())
            .collect();
        // Lock order matches close(): worker, then state. Workers only use state.
        let mut workers: Vec<_> = reporters.iter().map(|p| lock(&p.worker)).collect();
        for reporter in &reporters {
            lock(&reporter.state).paused = true;
            reporter.wake.notify_all();
        }
        for worker in &mut workers {
            if let Some(thread) = worker.take() {
                let _ = thread.join();
            }
        }
        let mut states: Vec<_> = reporters.iter().map(|p| lock(&p.state)).collect();
        let target = self.local_handle().and_then(|h| {
            reporters
                .iter()
                .position(|p| Arc::ptr_eq(p, &h.inner))
                .map(|i| (i, h.id))
        });
        let attempt = (|| {
            let mut writer = None;
            if let Some((index, id)) = target {
                let state = &mut states[index];
                if state.active(id) {
                    let entry = state.nodes.iter_mut().find(|e| e.id == id).unwrap();
                    if entry.child.is_some() {
                        return Err(invalid("a process is already attached to this task"));
                    }
                    let (read, write) = pipe()?;
                    entry.child = Some(ChildPipe::new(read));
                    writer = Some(write);
                }
            }
            // SAFETY: the caller meets the application fork contract; all
            // reporter threads have been joined and all state is held here.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                let error = io::Error::last_os_error();
                if writer.is_some() {
                    let (index, id) = target.unwrap();
                    states[index]
                        .nodes
                        .iter_mut()
                        .find(|e| e.id == id)
                        .unwrap()
                        .child = None;
                }
                return Err(error);
            }
            Ok((pid, writer))
        })();
        if let Ok((0, writer)) = attempt {
            for state in &mut states {
                state.disconnect();
            }
            drop(states);
            drop(workers);
            registry.clear();
            drop(registry);
            // The safety contract ensures only this thread exists in the child.
            std::env::remove_var("ZIG_PROGRESS");
            return Ok(ForkResult::Child {
                channel: writer.map(Into::into),
            });
        }
        for state in &mut states {
            state.paused = false;
        }
        drop(states);
        for (reporter, worker) in reporters.iter().zip(workers.iter_mut()) {
            match reporter.start_worker(false) {
                Ok(thread) => **worker = thread,
                Err(error) => {
                    let mut state = lock(&reporter.state);
                    state.error = Some(error);
                    state.disconnect();
                }
            }
        }
        attempt.map(|(pid, _writer)| ForkResult::Parent { pid })
    }
}
