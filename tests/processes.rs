//! Process isolation is required: fork fixtures run on the main thread of a
//! separate executable, never on a multithreaded libtest worker.
#[cfg(unix)]
mod suite {
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Output, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};
    use zprs::{ForkResult, NameSize, Node, Options};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            for attempt in 0..1000 {
                let path =
                    std::env::temp_dir().join(format!("zprs-{}-{attempt}", std::process::id()));
                match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("cannot create test directory: {e}"),
                }
            }
            panic!("cannot find an unused test directory");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn options(name: &str) -> Options {
        Options {
            refresh_interval: None,
            ..Options::new(name, 0)
        }
    }
    fn worker_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--worker").arg(mode);
        command
    }
    fn pipe() -> (File, File) {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
        }
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }
    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        // SAFETY: the pipe owns this descriptor for both calls.
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain(pipe: Option<&mut impl Read>, data: &mut Vec<u8>) -> io::Result<bool> {
        let Some(pipe) = pipe else {
            return Ok(true);
        };
        let mut buffer = [0; 4096];
        // Check the deadline even when a worker writes without stopping.
        for _ in 0..16 {
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(n) => data.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    // Callers start each fixture in its own process group. Keep the deadline
    // active until all pipes reach EOF, including after the main worker exits.
    fn collect(
        mut child: Child,
        mut reader: Option<&mut File>,
        timeout: Duration,
    ) -> io::Result<(Vec<u8>, Output)> {
        let result = (|| {
            let deadline = Instant::now() + timeout;
            if let Some(pipe) = &child.stdout {
                nonblocking(pipe)?;
            }
            if let Some(pipe) = &child.stderr {
                nonblocking(pipe)?;
            }
            let mut data = Vec::new();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            loop {
                let progress_done = drain(reader.as_deref_mut(), &mut data)?;
                let stdout_done = drain(child.stdout.as_mut(), &mut stdout)?;
                let stderr_done = drain(child.stderr.as_mut(), &mut stderr)?;
                // Keep the leader unreaped until EOF. Its PID then cannot be
                // reused for another process group before timeout cleanup.
                if progress_done && stdout_done && stderr_done {
                    if let Some(status) = child.try_wait()? {
                        return Ok((
                            data,
                            Output {
                                status,
                                stdout,
                                stderr,
                            },
                        ));
                    }
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "process timed out"));
                }
                thread::sleep(Duration::from_millis(2));
            }
        })();
        if result.is_err() {
            // SAFETY: this group belongs to the fixture. Stop descendants too,
            // since one can hold a pipe open after the group leader exits.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = child.wait();
        }
        result
    }

    fn run(command: &mut Command) -> io::Result<Output> {
        let child = command.process_group(0).spawn()?;
        collect(child, None, Duration::from_secs(10)).map(|(_, output)| output)
    }

    fn wait(command: &mut Command) -> Output {
        run(command).unwrap()
    }
    fn capture(mode: &str) -> (Vec<u8>, Output) {
        let (mut reader, writer) = pipe();
        let fd = writer.as_raw_fd();
        let mut command = worker_command(mode);
        command
            .env("ZIG_PROGRESS", fd.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.process_group(0).spawn().unwrap();
        drop(writer);
        let (data, output) = collect(child, Some(&mut reader), Duration::from_secs(10)).unwrap();
        assert!(
            output.status.success(),
            "mode {mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (data, output)
    }

    #[derive(Debug)]
    struct Record {
        name: String,
        completed: u32,
        total: u32,
        parent: u8,
    }
    // Deliberately independent decoder for the public wire contract.
    fn frames(data: &[u8], size: usize) -> Vec<Vec<Record>> {
        let mut cursor = 0;
        let mut frames = Vec::new();
        while cursor < data.len() {
            let n = usize::from(data[cursor]);
            assert!((1..=253).contains(&n));
            let length = 1 + n * (size + 9);
            assert!(cursor + length <= data.len(), "truncated frame");
            let mut frame = Vec::new();
            for i in 0..n {
                let start = cursor + 1 + i * (size + 8);
                let record = &data[start..start + size + 8];
                let parent = data[cursor + 1 + n * (size + 8) + i];
                assert!(if i == 0 {
                    parent == 255
                } else {
                    usize::from(parent) < n
                });
                let total = u32::from_le_bytes(record[4..8].try_into().unwrap());
                assert_ne!(total, u32::MAX);
                frame.push(Record {
                    name: String::from_utf8(
                        record[8..].split(|b| *b == 0).next().unwrap().to_vec(),
                    )
                    .unwrap(),
                    completed: u32::from_le_bytes(record[..4].try_into().unwrap()),
                    total,
                    parent,
                });
            }
            for i in 1..n {
                let mut index = i;
                for _ in 0..n {
                    if index == 0 {
                        break;
                    }
                    index = usize::from(frame[index].parent);
                }
                assert_eq!(index, 0, "cycle");
            }
            frames.push(frame);
            cursor += length;
        }
        assert!(!frames.is_empty(), "no progress received");
        frames
    }

    fn reap(pid: libc::pid_t) {
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
    }
    fn fork_worker(task: &Node, name: &str, nested: bool) {
        // This fixture is single-threaded except for this crate's update threads.
        match unsafe { task.fork() }.unwrap() {
            ForkResult::Parent { pid } => reap(pid),
            ForkResult::Child { channel } => {
                assert!(!task.enabled());
                task.complete_one();
                task.end();
                assert!(std::env::var_os("ZIG_PROGRESS").is_none());
                let p = match channel.as_ref() {
                    Some(fd) => options(name).build_with_fd(fd.as_fd()).unwrap(),
                    None => Options {
                        disabled: true,
                        ..options(name)
                    }
                    .build()
                    .unwrap(),
                };
                drop(channel);
                p.root().set_completed_items(6);
                p.root().set_estimated_total_items(10);
                if nested {
                    let leaf = p.root().start("nested fork", 0);
                    fork_worker(&leaf, "fork leaf", false);
                    let spawned = p.root().start("nested spawn", 0);
                    assert!(spawned
                        .spawn(worker_command("leaf"))
                        .unwrap()
                        .wait()
                        .unwrap()
                        .success());
                    p.flush().unwrap();
                    // Retain the tree in the final report.
                    p.close().unwrap();
                } else {
                    p.close().unwrap();
                }
                unsafe {
                    libc::_exit(0);
                }
            }
        }
    }

    fn worker(mode: &str) {
        match mode {
            "leaf" => {
                let p = options("leaf").build().unwrap();
                p.root().set_completed_items(3);
                p.root().set_estimated_total_items(8);
                p.close().unwrap();
            }
            "nested" => {
                let p = options("nested").build().unwrap();
                let task = p.root().start("leaf placeholder", 0);
                assert!(task
                    .spawn(worker_command("leaf"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
                p.close().unwrap();
            }
            "basic" | "legacy" => {
                let mut opts = options("root");
                opts.estimated_total_items = 2;
                if mode == "legacy" {
                    opts.name_size = NameSize::Legacy;
                }
                let p = opts.build().unwrap();
                let a = p.root().start("assets", 10);
                let b = p.root().start("code", 0);
                let c = b.start("compile", 4);
                a.add(3);
                c.complete_one();
                assert!(p.flush().unwrap());
                a.end();
                c.set_name("link");
                assert!(p.flush().unwrap());
                c.end();
                b.end();
                p.close().unwrap();
                println!("stdout unchanged");
                eprintln!("stderr unchanged");
            }
            "spawn" => {
                let p = options("parent").build().unwrap();
                let a = p.root().start("first", 0);
                let b = p.root().start("second", 0);
                let mut first = a.spawn(worker_command("leaf")).unwrap();
                let mut second = b.spawn(worker_command("nested")).unwrap();
                assert!(first.wait().unwrap().success());
                assert!(second.wait().unwrap().success());
                assert!(p.flush().unwrap());
                a.end();
                b.end();
                p.close().unwrap();
            }
            "spawn-options" => {
                let p = options("parent").build().unwrap();
                let task = p.root().start("child", 0);
                let original_env = std::env::var_os("ZIG_PROGRESS");
                let mut command = worker_command("check-options");
                command
                    .env_clear()
                    .env("CUSTOM", "kept")
                    .env("EXPECTED_DIR", std::env::temp_dir())
                    .env("ZIG_PROGRESS", "invalid")
                    .current_dir(std::env::temp_dir())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = task.spawn(command).unwrap();
                use std::io::Write;
                child.stdin.take().unwrap().write_all(b"input").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(output.stdout, b"output\n");
                assert_eq!(output.stderr, b"error\n");
                assert_eq!(std::env::var_os("ZIG_PROGRESS"), original_env);
                p.close().unwrap();
            }
            "check-options" => {
                assert_eq!(std::env::var("CUSTOM").unwrap(), "kept");
                assert_eq!(
                    std::env::current_dir().unwrap(),
                    std::path::PathBuf::from(std::env::var_os("EXPECTED_DIR").unwrap())
                );
                let mut input = String::new();
                io::stdin().read_to_string(&mut input).unwrap();
                assert_eq!(input, "input");
                let p = options("options preserved").build().unwrap();
                assert!(p.enabled());
                println!("output");
                eprintln!("error");
            }
            "spawn-failures" => {
                let p = options("parent").build().unwrap();
                let a = p.root().start("child", 0);
                assert!(a.spawn(Command::new("/no/such/zprs-program")).is_err());
                assert!(a
                    .spawn(worker_command("leaf"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
                assert_eq!(
                    a.spawn(worker_command("leaf")).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput
                );
                p.close().unwrap();
                assert!(a
                    .spawn(worker_command("no-channel"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
            }
            "no-channel" => {
                assert!(std::env::var_os("ZIG_PROGRESS").is_none());
                assert!(!options("disabled").build().unwrap().enabled());
            }
            "disabled-spawn" => {
                let p = Options {
                    disabled: true,
                    ..options("disabled")
                }
                .build()
                .unwrap();
                let mut cmd = worker_command("no-channel");
                cmd.env("ZIG_PROGRESS", "3");
                assert!(p.root().spawn(cmd).unwrap().wait().unwrap().success());
                assert!(Node::default()
                    .spawn(worker_command("no-channel"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
            }
            "ended-spawn" => {
                let p = options("parent").build().unwrap();
                let a = p.root().start("ended", 0);
                a.end();
                assert!(a
                    .spawn(worker_command("no-channel"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
            }
            "fork" | "auto-fork" => {
                let p = if mode == "auto-fork" {
                    Options {
                        initial_delay: Duration::ZERO,
                        refresh_interval: Some(Duration::from_millis(5)),
                        ..options("parent")
                    }
                    .build()
                    .unwrap()
                } else {
                    options("parent").build().unwrap()
                };
                // An unrelated automatic reporter must also be quiesced.
                let (_r, w) = pipe();
                let unrelated = Options::new("unrelated", 0)
                    .build_with_fd(w.as_fd())
                    .unwrap();
                let task = p.root().start("fork placeholder", 0);
                let env = std::env::var_os("ZIG_PROGRESS");
                fork_worker(&task, "forked", true);
                assert_eq!(std::env::var_os("ZIG_PROGRESS"), env);
                assert!(unrelated.enabled());
                assert!(p.flush().unwrap());
                if mode == "auto-fork" {
                    p.root().set_name("resumed");
                    thread::sleep(Duration::from_millis(40));
                }
                assert!(
                    matches!(unsafe { task.fork() }, Err(e) if e.kind() == io::ErrorKind::InvalidInput)
                );
                p.close().unwrap();
            }
            "disabled-fork" => {
                let p = Options {
                    disabled: true,
                    ..options("disabled")
                }
                .build()
                .unwrap();
                fork_worker(&p.root(), "disabled child", false);
                fork_worker(&Node::default(), "disabled child", false);
            }
            "raw-fork" => {
                let p = options("parent").build().unwrap();
                let root = p.root();
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0);
                if pid == 0 {
                    assert!(!p.enabled());
                    assert!(!root.enabled());
                    root.add(100);
                    root.end();
                    assert!(!p.flush().unwrap());
                    p.close().unwrap();
                    unsafe {
                        libc::_exit(0);
                    }
                }
                reap(pid);
                p.root().add(1);
                p.close().unwrap();
            }
            "max" => {
                let p = options("capacity").build().unwrap();
                let tasks: Vec<_> = (0..252)
                    .map(|i| p.root().start(&format!("task-{i}"), 1))
                    .collect();
                assert!(!p.root().start("overflow", 0).enabled());
                while !p.flush().unwrap() {
                    thread::sleep(Duration::from_millis(2));
                }
                drop(tasks);
                let replacement = p.root().start("replacement", 2);
                replacement.complete_one();
                p.flush().unwrap();
                replacement.end();
                p.close().unwrap();
            }
            "auto" => {
                let p = Options {
                    initial_delay: Duration::ZERO,
                    refresh_interval: Some(Duration::from_millis(5)),
                    ..options("automatic")
                }
                .build()
                .unwrap();
                p.root().add(1);
                thread::sleep(Duration::from_millis(40));
                p.root().add(1);
            }
            "native" => {
                let p = options("native parent").build().unwrap();
                let task = p.root().start("native placeholder", 0);
                let mut cmd = Command::new("zpg");
                cmd.args(["--report", "2", "5", "--name", "native leaf"]);
                assert!(task.spawn(cmd).unwrap().wait().unwrap().success());
                p.close().unwrap();
            }
            "invalid-env" => {
                assert!(!options("disabled").build().unwrap().enabled());
            }
            "cloexec" => {
                let p = options("parent").build().unwrap();
                assert!(p.enabled());
                let fd: i32 = std::env::var("ZIG_PROGRESS").unwrap().parse().unwrap();
                assert_ne!(
                    unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
                let mut command = worker_command("check-closed");
                command.env("CHECK_FD", fd.to_string());
                assert!(command.status().unwrap().success());
            }
            "check-closed" => {
                let fd: i32 = std::env::var("CHECK_FD").unwrap().parse().unwrap();
                assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
                assert!(!options("disabled").build().unwrap().enabled());
            }
            "sigpipe" | "sigpipe-ignored" | "sigpipe-pending" | "sigpipe-blocked" => {
                let (reader, writer) = pipe();
                drop(reader);
                let p = options("broken").build_with_fd(writer.as_fd()).unwrap();
                unsafe {
                    let handler = if mode == "sigpipe-ignored" {
                        libc::SIG_IGN
                    } else {
                        libc::SIG_DFL
                    };
                    assert_ne!(libc::signal(libc::SIGPIPE, handler), libc::SIG_ERR);
                    let mut set = std::mem::zeroed::<libc::sigset_t>();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGPIPE);
                    let blocked = matches!(mode, "sigpipe-pending" | "sigpipe-blocked");
                    assert_eq!(
                        libc::pthread_sigmask(
                            if blocked {
                                libc::SIG_BLOCK
                            } else {
                                libc::SIG_UNBLOCK
                            },
                            &set,
                            std::ptr::null_mut(),
                        ),
                        0
                    );
                    if mode == "sigpipe-pending" {
                        assert_eq!(libc::raise(libc::SIGPIPE), 0);
                    }
                    assert_eq!(p.flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
                    let mut pending = std::mem::zeroed::<libc::sigset_t>();
                    assert_eq!(libc::sigpending(&mut pending), 0);
                    assert_eq!(
                        libc::sigismember(&pending, libc::SIGPIPE),
                        i32::from(mode == "sigpipe-pending")
                    );
                    let mut mask = std::mem::zeroed::<libc::sigset_t>();
                    assert_eq!(
                        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask),
                        0
                    );
                    assert_eq!(libc::sigismember(&mask, libc::SIGPIPE), i32::from(blocked));
                    if mode == "sigpipe-pending" {
                        let mut signal = 0;
                        assert_eq!(libc::sigwait(&set, &mut signal), 0);
                        assert_eq!(signal, libc::SIGPIPE);
                    }
                    assert_eq!(libc::signal(libc::SIGPIPE, handler), handler);
                }
                assert!(!p.enabled());
            }
            "closed-stdio" => {
                let p = options("parent").build().unwrap();
                for fd in 0..=2 {
                    unsafe {
                        libc::close(fd);
                    }
                }
                let task = p.root().start("child", 0);
                assert!(task
                    .spawn(worker_command("leaf"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
                for fd in 0..=2 {
                    assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
                }
                p.close().unwrap();
            }
            "resources" => {
                let p = options("parent").build().unwrap();
                let count_fds = || {
                    (0..1024)
                        .filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0)
                        .count()
                };
                let before = count_fds();
                for _ in 0..20 {
                    let task = p.root().start("child", 0);
                    assert!(task.spawn(Command::new("/no/such/zprs-program")).is_err());
                    assert!(task
                        .spawn(worker_command("leaf"))
                        .unwrap()
                        .wait()
                        .unwrap()
                        .success());
                }
                assert_eq!(count_fds(), before);
                let task = p.root().start("status", 0);
                let status = task
                    .spawn(worker_command("exit-23"))
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(status.code(), Some(23));
                task.end();
                let task = p.root().start("still running", 0);
                let mut child = task.spawn(worker_command("sleep")).unwrap();
                task.end();
                p.close().unwrap();
                assert!(child.try_wait().unwrap().is_none());
                child.kill().unwrap();
                assert!(!child.wait().unwrap().success());
            }
            "pipe-failures" => {
                let p = Options {
                    initial_delay: Duration::ZERO,
                    refresh_interval: Some(Duration::from_millis(5)),
                    ..options("before pipe failure")
                }
                .build()
                .unwrap();
                let task = p.root().start("child", 0);
                // Limit this fixture only. Existing descriptors stay open, but
                // pipe creation must fail. Restore the limit before assertions.
                let mut original = unsafe { std::mem::zeroed::<libc::rlimit>() };
                assert_eq!(
                    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
                    0
                );
                let limited = libc::rlimit {
                    rlim_cur: 0,
                    ..original
                };
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
                let spawn = task.spawn(worker_command("leaf"));
                // No application threads exist; fork must restart the reporter
                // thread after the pipe error without creating a process.
                let fork = unsafe { task.fork() };
                assert_eq!(
                    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) },
                    0
                );
                assert_eq!(spawn.unwrap_err().raw_os_error(), Some(libc::EMFILE));
                assert_eq!(fork.unwrap_err().raw_os_error(), Some(libc::EMFILE));
                p.root().set_name("after pipe failure");
                thread::sleep(Duration::from_millis(40));
                assert!(p.enabled());
                assert!(p.error().is_none());
                assert!(task
                    .spawn(worker_command("leaf"))
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success());
                p.close().unwrap();
            }
            "large-output" => {
                use std::io::Write;
                let p = options("large output").build().unwrap();
                io::stdout().write_all(&vec![b'o'; 256 * 1024]).unwrap();
                io::stderr().write_all(&vec![b'e'; 256 * 1024]).unwrap();
                p.close().unwrap();
            }
            "exit-23" => std::process::exit(23),
            "sleep" => thread::sleep(Duration::from_secs(30)),
            other => panic!("unknown worker {other}"),
        }
    }

    fn harness() {
        // Output must be read while the worker runs, or a full pipe prevents exit.
        let (data, output) = capture("large-output");
        assert_eq!(frames(&data, 120)[0][0].name, "large output");
        assert_eq!(output.stdout, vec![b'o'; 256 * 1024]);
        assert_eq!(output.stderr, vec![b'e'; 256 * 1024]);

        // Keep an extra writer open, as a descendant could do after its parent
        // exits. Output collection must still stop at the deadline.
        let (reader, writer) = pipe();
        let mut child = worker_command("exit-23")
            .process_group(0)
            .stdout(Stdio::from(writer.try_clone().unwrap()))
            .spawn()
            .unwrap();
        child.stdout = Some(std::os::fd::OwnedFd::from(reader).into());
        let start = Instant::now();
        let error = collect(child, None, Duration::from_millis(25)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(writer);

        // A timeout must terminate and reap a worker which is still running.
        let child = worker_command("sleep").process_group(0).spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        let error = collect(child, None, Duration::from_millis(25)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    fn basic() {
        for (mode, size) in [("basic", 120), ("legacy", 40)] {
            let (data, output) = capture(mode);
            let reports = frames(&data, size);
            assert_eq!(reports.len(), 3);
            assert_eq!(reports[0][1].name, "assets");
            assert_eq!(reports[0][1].completed, 3);
            assert_eq!(reports[0][1].total, 10);
            assert_eq!(reports[0][3].parent, 2);
            assert_eq!(reports[1][2].name, "link");
            assert_eq!(reports[1][2].parent, 1);
            assert_eq!(reports[2][0].completed, 2);
            assert_eq!(output.stdout, b"stdout unchanged\n");
            assert_eq!(output.stderr, b"stderr unchanged\n");
        }
    }
    fn spawn() {
        let (data, _) = capture("spawn");
        let reports = frames(&data, 120);
        assert_eq!(
            reports[0]
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["parent", "leaf", "nested", "leaf"]
        );
        assert_eq!(reports[0][3].parent, 2);
        assert_eq!(reports.last().unwrap()[0].completed, 2);
        for mode in ["spawn-options", "spawn-failures", "ended-spawn", "cloexec"] {
            frames(&capture(mode).0, 120);
        }
        assert!(capture("disabled-spawn").0.is_empty());
    }
    fn forks() {
        for mode in ["fork", "auto-fork"] {
            let reports = frames(&capture(mode).0, 120);
            let frame = reports
                .iter()
                .find(|f| f.iter().any(|r| r.name == "fork leaf"))
                .expect("nested fork absent");
            assert!(frame
                .iter()
                .any(|r| r.name == "forked" && r.completed == 6 && r.total == 10));
            assert!(frame.iter().any(|r| r.name == "leaf" && r.completed == 3));
            if mode == "auto-fork" {
                assert!(reports.iter().filter(|f| f[0].name == "resumed").count() >= 2);
            }
        }
        assert!(capture("disabled-fork").0.is_empty());
        let reports = frames(&capture("raw-fork").0, 120);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0][0].completed, 1);
    }
    fn limits_and_updates() {
        let reports = frames(&capture("max").0, 120);
        assert_eq!(reports[0].len(), 253);
        assert_eq!(reports[0][252].name, "task-251");
        assert_eq!(reports.last().unwrap()[0].completed, 253);
        let reports = frames(&capture("auto").0, 120);
        assert!(reports.len() > 2);
        assert_eq!(reports.last().unwrap()[0].completed, 2);
    }

    fn resources() {
        for mode in ["resources", "closed-stdio", "pipe-failures"] {
            let reports = frames(&capture(mode).0, 120);
            if mode == "closed-stdio" {
                assert_eq!(reports[0][1].name, "leaf");
            } else if mode == "pipe-failures" {
                assert!(
                    reports
                        .iter()
                        .filter(|f| f[0].name == "after pipe failure")
                        .count()
                        >= 2
                );
                assert_eq!(reports.last().unwrap()[1].name, "leaf");
            }
        }
    }
    fn invalid_environment() {
        for value in [
            "",
            "-1",
            "+3",
            " 3",
            "3 ",
            "65536",
            "99999999999999999",
            "invalid",
            "65535",
        ] {
            let output = wait(
                worker_command("invalid-env")
                    .env("ZIG_PROGRESS", value)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            );
            assert!(
                output.status.success(),
                "{value}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = wait(worker_command("invalid-env").env_remove("ZIG_PROGRESS"));
        assert!(output.status.success());
        let file = File::open("/dev/null").unwrap();
        let fd = file.as_raw_fd();
        let mut command = worker_command("invalid-env");
        command.env("ZIG_PROGRESS", fd.to_string());
        unsafe {
            command.pre_exec(move || {
                libc::fcntl(fd, libc::F_SETFD, 0);
                Ok(())
            });
        }
        assert!(wait(&mut command).status.success());
        for mode in [
            "sigpipe",
            "sigpipe-ignored",
            "sigpipe-pending",
            "sigpipe-blocked",
        ] {
            assert!(wait(&mut worker_command(mode)).status.success(), "{mode}");
        }
    }
    fn zpg() {
        let probe = run(Command::new("zpg")
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null()));
        if let Err(error) = &probe {
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "zpg --help: {error}");
            assert!(
                std::env::var_os("ZPRS_REQUIRE_ZPG").is_none(),
                "zpg required but unavailable"
            );
            println!(
                "SKIP zpg compatibility: zpg is not on PATH (set ZPRS_REQUIRE_ZPG=1 to require it)"
            );
            return;
        }
        assert!(probe.unwrap().status.success(), "zpg --help failed");
        let directory = TempDir::new();
        for mode in [
            "basic",
            "spawn",
            "fork",
            "auto-fork",
            "native",
            "max",
            "auto",
        ] {
            let log_path = directory.0.join(format!("{mode}.log"));
            let mut command = Command::new("zpg");
            command
                .args(["--test", "--test-output"])
                .arg(&log_path)
                .arg(std::env::current_exe().unwrap())
                .args(["--worker", mode])
                .env_remove("ZIG_PROGRESS")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let output = wait(&mut command);
            let log = std::fs::read_to_string(&log_path).unwrap();
            let _ = std::fs::remove_file(log_path);
            assert!(
                output.status.success(),
                "{mode}: {}\n{log}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(log.contains("snapshot="), "{mode}: {log}");
            assert!(
                !log.contains("error:") && !log.contains("no progress received"),
                "{mode}: {log}"
            );
            match mode {
                "basic" => {
                    assert!(log.contains("completed=2 total=2 name=\"root\""));
                    assert!(log.contains("name=\"link\""));
                }
                "spawn" => {
                    assert!(log.contains("node=3 parent=2 completed=3 total=8 name=\"leaf\""))
                }
                "fork" | "auto-fork" => {
                    assert!(log.contains("name=\"fork leaf\""));
                    assert!(log.contains("name=\"leaf\""));
                }
                "native" => assert!(log.contains("completed=2 total=5 name=\"native leaf\"")),
                "max" => {
                    assert!(log.contains("nodes=253"));
                    assert!(log.contains("name=\"replacement\""));
                }
                "auto" => assert!(log.contains("completed=2 total=0 name=\"automatic\"")),
                _ => unreachable!(),
            }
            println!("test zpg::{mode} ... ok");
        }
    }
    pub fn main() {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|s| s == "--worker") {
            worker(&args[2]);
            return;
        }
        for (name, test) in [
            ("process test harness", harness as fn()),
            ("wire formats", basic),
            ("spawn", spawn),
            ("fork", forks),
            ("limits and automatic updates", limits_and_updates),
            ("descriptor and process lifetimes", resources),
            ("environment and signals", invalid_environment),
            ("zpg compatibility", zpg),
        ] {
            test();
            println!("test {name} ... ok");
        }
    }
}

fn main() {
    #[cfg(unix)]
    suite::main();
    #[cfg(not(unix))]
    println!("SKIP Unix process tests");
}
