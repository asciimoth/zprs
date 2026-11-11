# zprs

[![Crates.io](https://img.shields.io/crates/v/zprs.svg)](https://crates.io/crates/zprs)
[![Documentation](https://img.shields.io/docsrs/zprs/latest)](https://docs.rs/zprs/)

`zprs` is a Rust library that reports task progress through the Zig progress
protocol. Use it to report tasks, subtasks, and progress from child processes.

`zprs` requires Rust 1.75 or later. Pipe reporting and fork support are available
on Unix. On other platforms, reporting is disabled and child processes still
run. On Unix, the only runtime dependency is `libc`.

## Installation

Add `zprs` to the dependencies in your `Cargo.toml`:

```toml
[dependencies]
zprs = "0.1"
```

## Zig progress protocol

The Zig progress protocol sends a task tree from a child process to its
parent through a pipe. Tasks have names, completed item counts, and estimated
totals. The `ZIG_PROGRESS` environment variable gives the pipe descriptor.
A parent can combine task trees from its children into one progress display.

The protocol also works outside the Zig toolchain. For example,
[`zpg`][zpg] can run your Rust program and display its progress in a terminal.
Use `zprs` to send reports to `zpg` or another compatible receiver.

See Andrew Kelley's [protocol specification][spec] for the protocol design.
For clients in other languages, see [`zppy`][zppy] for Python,
[`zpgo`][zpgo] for Go, and [`zpjs`][zpjs] for JavaScript.

![zpg displaying a task tree](https://vhs.charm.sh/vhs-5OKsqixxKTapPmjdIv2Qhf.gif)

## Library usage

If you use this library, [open an issue][issues] or [send a pull request][pulls]
with a link to your project. This helps us track use of the Zig progress
protocol outside the Zig toolchain.

Add the [`zig-progress-protocol` topic][topic] to your GitHub repository. Use
the same tag on other platforms so people can find related projects.

### Report tasks

Save this example as `src/main.rs` in your Cargo project:

```rust
use std::thread;
use std::time::Duration;
use zprs::Progress;

fn main() -> std::io::Result<()> {
    let progress = Progress::new("build", 2)?;
    for name in ["assets", "code"] {
        let task = progress.root().start(name, 10);
        for _ in 0..10 {
            // Replace this delay with one item of work.
            thread::sleep(Duration::from_millis(100));
            task.complete_one();
        }
    } // Each task ends when its last handle is dropped.
    progress.close()
}
```

With `zpg` on `PATH`, build and run your program. Replace `your-program` with
your Cargo binary name:

```sh
cargo build
zpg target/debug/your-program
```

If no usable `ZIG_PROGRESS` channel is available, reporting is disabled.
Task updates then have no effect. The library does not write to standard
output or standard error. It does not draw its own terminal display.

A task can start child tasks. A `Node` is a shared task handle. Clone the handle
to share a task between threads.
When the last handle is dropped, the task and its descendants are removed.
Calling `end()` also removes the task and its descendants. Either action
completes one item in the parent task. Do not also increment the parent count
for that task. Repeated `end()` calls and updates on ended tasks have no effect.
Ending a task does not indicate success or failure.

Dropping a root handle has no effect. Ending the root closes the reporter.
Dropping `Progress` also closes the reporter, even if task handles remain.

### Run the combined example

From the root directory of this repository, run:

```sh
cargo build --example demo
zpg target/debug/examples/demo
```

The [combined example][demo] starts a `zpg --mock` child process and
forks a Rust worker. The parent combines both task trees under `Rust demo`.
Both workers also write messages to standard output and standard error. This
example requires Unix and `zpg` on `PATH`.

### Child processes

Configure a standard `Command`, then pass it to `Node::spawn`:

```rust
use std::process::{Command, Stdio};
use zprs::Progress;

fn run() -> std::io::Result<()> {
    let progress = Progress::new("build", 1)?;
    let task = progress.root().start("compile", 0);
    let mut command = Command::new("worker");
    command.arg("--input").arg("source").stdout(Stdio::piped());
    let child = task.spawn(command)?;
    let output = child.wait_with_output()?;
    task.end();
    progress.close()?;
    assert!(output.status.success());
    Ok(())
}
```

Replace `worker` and its arguments with your command. In a Rust worker, create
a new reporter with `Progress::new`. Workers in other languages can use any
client with the same protocol format.

`spawn` returns a `std::process::Child`. The method preserves the command's
arguments, standard streams, working directory, and other environment settings,
including `env_clear()`. It sets `ZIG_PROGRESS` in the child environment to
identify the private progress pipe. The parent environment stays unchanged.
The method consumes the command. Custom exec hooks must not call this library
or change the private progress pipe descriptor.

The child process's root replaces the task in each report. The root's
descendants appear below that task. If the child root name is empty, the task
keeps its local name. Before the first complete child report, the task keeps
its local values. After the pipe reaches end-of-file (EOF) or receives an
invalid child report, the last valid child tree remains until the task ends.

Use one task per process. A second attachment to the same active task returns
`InvalidInput`, even after the first process exits. If `spawn` fails, the task
remains available. Disabled, ended, or closed tasks still start child processes.
For these tasks, `ZIG_PROGRESS` is removed from the child environment.

Wait for the child process, then end its task. Ending a task closes the task's
progress pipes. Closing a reporter closes the reporter's progress pipes.
Neither action waits for or terminates the process. Child processes can start
their own child processes. Use the same `name_size` for the receiver and all
reporters in the process tree.

### Forked processes on Unix

`unsafe Node::fork()` returns `ForkResult::Parent { pid }` in the parent and
`ForkResult::Child { channel }` in the child. See the complete
[fork example][fork].

The method stops and joins all of this library's update threads before the fork
and restarts them in the parent. In the child, it disables inherited reporters,
closes the pipes they own, and removes `ZIG_PROGRESS`. Create a new reporter
with `Options::build_with_fd(channel.as_fd())`. The caller owns the returned
channel. Close the channel after the new reporter duplicates it. If the source
task was disabled or ended, `channel` is `None`. In that case, create a disabled
reporter.

The caller must ensure that no other application threads exist at the fork.
Signal handlers and at-fork hooks must not call this library. Other libraries
must permit execution after fork. Use `spawn` in multithreaded applications.
The caller must manage caller-owned descriptors in both processes.
Close the child's reporter before calling `_exit`, which skips destructors.
The parent must reap the PID with `waitpid`, then end the task.

A direct `libc::fork()` does not attach a private progress channel. Inherited
reporter handles detect the changed PID and do not report or join old threads.
After a direct `libc::fork()`, do not create a new reporter from the inherited
upstream channel. Two writers can corrupt the stream.

### Settings and API

Use `Options` to configure a reporter:

```rust
use zprs::Options;

fn main() -> std::io::Result<()> {
    let progress = Options {
        refresh_interval: None,
        ..Options::new("build", 10)
    }
    .build()?;

    // Do work and call progress.flush() regularly to send updates.
    progress.flush()?;
    progress.close()
}
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `root_name` | Empty | Root name |
| `estimated_total_items` | `0` | Root estimate; zero means unknown |
| `refresh_interval` | `Some(80 ms)` | Update period; `None` for manual use |
| `initial_delay` | `200 ms` | Delay before the first automatic report |
| `disabled` | `false` | Disable reporting |
| `name_size` | `NameSize::Current` | 120-byte name field |

A zero update interval or a duration that cannot fit an `Instant` is invalid.
If you use manual updates, call `flush()` regularly to collect child reports.

| Method | Action |
| --- | --- |
| `Progress::root()` | Get a root handle |
| `Progress::enabled()` | Check whether the channel is open |
| `Progress::flush()` | Collect child reports and try to send the current tree |
| `Progress::error()` | Get the first permanent output error |
| `Progress::close()` | Stop updates, try a final report, and close pipes |
| `Node::start(name, total)` | Start a child task |
| `Node::enabled()` | Check whether the task is active |
| `Node::complete_one()` / `add(count)` | Add completed items |
| `Node::set_completed_items(count)` | Set the completed count |
| `Node::set_estimated_total_items(count)` | Set the estimate |
| `Node::increase_estimated_total_items(count)` | Add to the estimate |
| `Node::set_name(name)` | Change the task name |
| `Node::end()` | Remove this branch and complete one parent item |
| `Node::spawn(command)` | Start a child process with a private channel |
| `unsafe Node::fork()` | Fork with a private channel on Unix |

Task operations are thread-safe. For full signatures, see the
[API documentation](https://docs.rs/zprs).

## Technical details

### Protocol compatibility and limits

Names use 120 bytes by default. Select `NameSize::Legacy` for the 40-byte
format in the [published specification][spec]. The two formats are not wire
compatible. The protocol does not negotiate the format. The receiver and every
reporter in a process tree must use the same setting.

Use one reporter per output pipe. Each report replaces the previous task tree.
Multiple writers can replace each other's progress or mix packet bytes. Use
`Node::spawn()` or `Node::fork()` to combine progress from separate processes
through private pipes.

Completed counts are limited to `u32::MAX`. Totals are limited to
`u32::MAX - 1`. The library reduces larger values to these limits. Names stop
at the first NUL. The library shortens names at a UTF-8 character boundary and
pads them with zero bytes. A tree has at most 253 nodes, including the root.
When the tree reaches this limit, `start()` returns a disabled handle. Ending
tasks makes space for new tasks. Local tasks have priority when combined child
trees exceed this limit.

### Platforms and pipe handling

Pipe reporting and `Node::fork()` are available on Unix. On other platforms,
reporting is disabled and `Node::spawn()` still starts processes.

On Unix, `Options::build_with_fd(BorrowedFd)` accepts an explicit writable pipe.
It returns an error for an unusable descriptor unless reporting is disabled.
The caller keeps the original descriptor. The reporter owns a duplicate.
Both descriptors share nonblocking mode. If the channel specified by
`ZIG_PROGRESS` is invalid, reporting is disabled without an error.

`flush()` returns one of these results:

- `Ok(true)` if the current snapshot was sent in full.
- `Ok(false)` if reporting is disabled or blocked.
- An error if a permanent output failure occurs.

Writes do not wait for pipe space. The reporter completes a partially written
packet before it sends a newer packet. When the reporter closes, it tries to
complete the last packet. If the pipe stays full, that packet can remain
incomplete. Closing the reporter does not set counts to their estimates.
The reporter handles a closed receiver without changing the process-wide
`SIGPIPE` handler.

[spec]: https://andrewkelley.me/post/zig-new-cli-progress-bar-explained.html#the-zig-progress-protocol-specification
[zpg]: https://github.com/asciimoth/zpg
[zppy]: https://github.com/asciimoth/zppy
[zpgo]: https://github.com/asciimoth/zpgo
[zpjs]: https://github.com/asciimoth/zpjs
[issues]: https://github.com/asciimoth/zprs/issues
[pulls]: https://github.com/asciimoth/zprs/pulls
[topic]: https://github.com/topics/zig-progress-protocol
[demo]: https://github.com/asciimoth/zprs/blob/master/examples/demo.rs
[fork]: https://github.com/asciimoth/zprs/blob/master/examples/fork.rs

## License

Files in this repository are distributed under the CC0 license.

<p xmlns:dct="http://purl.org/dc/terms/">
  <a rel="license"
     href="https://creativecommons.org/publicdomain/zero/1.0/">
    <img src="https://i.creativecommons.org/p/zero/1.0/88x31.png"
         style="border-style: none;" alt="CC0" />
  </a>
  <br />
  To the extent possible under law,
  <a rel="dct:publisher"
     href="https://github.com/asciimoth">
    <span property="dct:title">ASCIIMoth</span></a>
  has waived all copyright and related or neighboring rights to
  <span property="dct:title">zprs</span>.
</p>
