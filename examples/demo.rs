//! Run with: cargo build --example demo && zpg target/debug/examples/demo
use std::io;
use std::process::Command;
use std::thread;
use std::time::Duration;
use zprs::Progress;

fn worker() -> io::Result<()> {
    let progress = Progress::new("Rust worker", 2)?;
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

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--worker") {
        return worker();
    }
    let progress = Progress::new("Rust demo", 2)?;
    let native_task = progress.root().start("native worker", 0);
    let rust_task = progress.root().start("Rust worker", 0);
    let mut native_command = Command::new("zpg");
    native_command.arg("--mock");
    let mut native = native_task.spawn(native_command)?;
    let mut rust_command = Command::new(std::env::current_exe()?);
    rust_command.arg("--worker");
    let mut rust = rust_task.spawn(rust_command)?;
    let rust_status = rust.wait()?;
    rust_task.end();
    let native_status = native.wait()?;
    native_task.end();
    progress.close()?;
    if !rust_status.success() || !native_status.success() {
        return Err(io::Error::other("a worker failed"));
    }
    Ok(())
}
