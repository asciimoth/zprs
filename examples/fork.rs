//! Run with: cargo build --example fork && zpg target/debug/examples/fork
#[cfg(unix)]
fn main() -> std::io::Result<()> {
    use std::os::fd::AsFd;
    use std::thread;
    use std::time::Duration;
    use zprs::{ForkResult, Options, Progress};

    let progress = Progress::new("Rust fork demo", 1)?;
    let task = progress.root().start("Rust child", 0);
    // SAFETY: this program has no application threads. Node::fork stops and
    // joins the library's update threads before it calls fork.
    match unsafe { task.fork()? } {
        ForkResult::Parent { pid } => {
            let mut status = 0;
            loop {
                // SAFETY: status is writable; pid identifies our child.
                if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            task.end();
            progress.close()?;
            if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                return Err(std::io::Error::other("forked worker failed"));
            }
        }
        ForkResult::Child { channel } => {
            let result = (|| -> std::io::Result<()> {
                let options = Options::new("Rust fork worker", 10);
                let child = match channel.as_ref() {
                    Some(fd) => options.build_with_fd(fd.as_fd())?,
                    None => Options {
                        disabled: true,
                        ..options
                    }
                    .build()?,
                };
                drop(channel);
                for _ in 0..10 {
                    thread::sleep(Duration::from_millis(100));
                    child.root().complete_one();
                }
                child.close()
            })();
            // SAFETY: the child's reporter has been closed. Do not run parent
            // cleanup or flush copies of the parent's buffered standard streams.
            unsafe {
                libc::_exit(if result.is_ok() { 0 } else { 1 });
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn main() {
    eprintln!("This example requires Unix.");
}
