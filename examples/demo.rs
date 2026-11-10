//! Run with: just run
//! Combine a zpg --mock subprocess and a forked Rust worker on Unix.
#[cfg(unix)]
mod demo {
    use std::io::{self, Write};
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus};
    use std::thread;
    use std::time::Duration;
    use zprs::{ForkResult, Options};

    fn worker(channel: Option<OwnedFd>) -> io::Result<()> {
        let options = Options {
            initial_delay: Duration::ZERO,
            ..Options::new("Rust fork worker", 2)
        };
        let progress = match channel.as_ref() {
            Some(fd) => options.build_with_fd(fd.as_fd())?,
            None => Options {
                disabled: true,
                ..options
            }
            .build()?,
        };
        // The new reporter owns a duplicate of the channel.
        drop(channel);
        for name in ["Rust assets", "Rust code"] {
            let task = progress.root().start(name, 10);
            for _ in 0..10 {
                thread::sleep(Duration::from_millis(60));
                task.complete_one();
            }
        }
        println!("Rust worker stdout: complete");
        eprintln!("Rust worker stderr: complete");
        progress.close()
    }

    fn wait_worker(pid: libc::pid_t, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        loop {
            let mut status = 0;
            // SAFETY: status is writable; pid identifies our child.
            let result = unsafe { libc::waitpid(pid, &mut status, flags) };
            if result > 0 {
                return Ok(Some(ExitStatus::from_raw(status)));
            }
            if result == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    pub fn run() -> io::Result<()> {
        let progress = Options {
            initial_delay: Duration::ZERO,
            ..Options::new("Rust demo", 2)
        }
        .build()?;
        let native_task = progress.root().start("zpg --mock", 0);
        let rust_task = progress.root().start("Rust fork worker", 0);
        // Do not copy buffered output into the forked child.
        io::stdout().flush()?;
        io::stderr().flush()?;
        let mut native_command = Command::new("zpg");
        native_command.arg("--mock");
        let mut native = native_task.spawn(native_command)?;

        // SAFETY: this program has no application threads. Node::fork stops
        // and joins the library's update threads before it calls fork.
        let pid = match unsafe { rust_task.fork() } {
            Ok(ForkResult::Parent { pid }) => pid,
            Ok(ForkResult::Child { channel }) => {
                let result = worker(channel);
                if let Err(error) = &result {
                    eprintln!("Rust worker: {error}");
                }
                let stdout = io::stdout().flush();
                let stderr = io::stderr().flush();
                let success = result.is_ok() && stdout.is_ok() && stderr.is_ok();
                // SAFETY: the child's reporter is closed. Skip the parent's
                // destructors and do not flush its copied standard streams.
                unsafe { libc::_exit(if success { 0 } else { 1 }) }
            }
            Err(error) => {
                let _ = native.kill();
                let _ = native.wait();
                return Err(error);
            }
        };

        let mut native_status = None;
        let mut rust_status = None;
        let result = (|| -> io::Result<()> {
            // Automatic updates collect both child trees and send them to zpg.
            // Remove each task as soon as its process exits.
            while native_status.is_none() || rust_status.is_none() {
                if native_status.is_none() {
                    native_status = native.try_wait()?;
                    if native_status.is_some() {
                        progress.flush()?;
                        native_task.end();
                    }
                }
                if rust_status.is_none() {
                    rust_status = wait_worker(pid, libc::WNOHANG)?;
                    if rust_status.is_some() {
                        progress.flush()?;
                        rust_task.end();
                    }
                }
                if native_status.is_none() || rust_status.is_none() {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            progress.close()?;
            if !native_status.unwrap().success() || !rust_status.unwrap().success() {
                return Err(io::Error::other("a worker failed"));
            }
            Ok(())
        })();

        // On an error, stop and reap any child that is still running.
        if rust_status.is_none() {
            // SAFETY: pid identifies our child, which has not been reaped.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let _ = wait_worker(pid, 0);
        }
        if native_status.is_none() {
            let _ = native.kill();
            let _ = native.wait();
        }
        result
    }
}

#[cfg(unix)]
fn main() -> std::io::Result<()> {
    demo::run()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("This example requires Unix.");
}
