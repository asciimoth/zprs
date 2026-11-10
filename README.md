# zprs

`zprs` is a Rust client for the Zig progress protocol. It reports tasks,
subtasks, and child process progress to a supervisor such as
[`zpg`](https://github.com/asciimoth/zpg). It does not draw a terminal display.

Requires Rust 1.75 or later. Pipe transport and fork support are available on
Unix. On other platforms, reporting is disabled and subprocesses still run.
The only runtime dependency is `libc`, on Unix.

## Report tasks

Add `zprs = "0.1"` to your dependencies, then create one reporter per upstream
channel:

```rust
use zprs::Progress;

fn build() -> std::io::Result<()> {
    let progress = Progress::new("build", 2)?;
    for name in ["assets", "code"] {
        let task = progress.root().start(name, 10);
        for _ in 0..10 {
            // Do one item of work.
            task.complete_one();
        }
    } // Each task ends when its last handle is dropped.
    progress.close()
}
```

Run your program with `zpg ./your-program`. Without a usable `ZIG_PROGRESS`
channel, task operations have no effect. The library does not write to standard
output or standard error.

A `Node` is a shared task handle. Clone it to share a task between threads.
Dropping its last handle, or calling `end()`, removes the task and its descendants
and completes **one** item in its parent. Do not also increment the parent for
that task. Repeated `end()` calls and updates on ended nodes have no effect.
Ending a task does not indicate success or failure.

Dropping a root handle has no effect. Ending the root closes the reporter.
Dropping `Progress` also closes the reporter, even if task handles remain.

## Subprocesses

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

`spawn` returns a normal `std::process::Child`. It preserves arguments, standard
streams, working directory, and other environment settings, including
`env_clear()`. It consumes the command because its exec hook owns the private
pipe descriptor. Custom exec hooks must not call this library or change that
private descriptor. The parent environment stays unchanged.

The child's root replaces the task in each report. Its descendants appear below
that task. An empty child root name keeps the local task name. Before the first
complete child report, the task keeps its local values. After EOF or an invalid
child report, the last valid child tree remains until the task ends.

Use one task per process. A second attachment to the same active task returns
`InvalidInput`, even after the first process exits. A failed spawn leaves the
task available. Disabled, ended, or closed tasks still spawn processes, with
`ZIG_PROGRESS` removed from the child environment.

Wait for the child, then end its task. Ending a task or closing a reporter closes
its progress pipes. Neither action waits for or terminates the process. Children
can use this library or any other client with the same wire format, and can
start their own children.

## Forks on Unix

`unsafe Node::fork()` returns `ForkResult::Parent { pid }` in the parent and
`ForkResult::Child { channel }` in the child. See the complete
[fork example](examples/fork.rs).

The method stops and joins all of this library's update threads before the fork
and restarts them in the parent. In the child, it disables inherited reporters,
closes their owned pipes, and removes `ZIG_PROGRESS`. Create a fresh reporter
with `Options::build_with_fd(channel.as_fd())`. The returned channel is owned by
the caller; close it after the new reporter duplicates it. If the source task
was disabled or ended, `channel` is `None`; create a disabled reporter.

The caller must ensure that no other application threads exist at the fork.
Signal handlers and at-fork hooks must not call this library. Other libraries
must permit execution after fork. Use `spawn` in multithreaded applications.
Caller-owned descriptors remain the caller's responsibility in both processes.
Close the child's reporter before calling `_exit`, which skips destructors.
The parent must reap the PID with `waitpid`, then end the task.

A direct `libc::fork()` does not attach a private progress channel. Inherited
reporter handles detect the changed PID and do not report or join old threads.
Do not create a new reporter from the inherited upstream channel after a raw
fork: two writers can corrupt the stream.

## Settings and API

Use `Options { ..Options::new("build", 10) }.build()` to configure a reporter.
On Unix, `build_with_fd(BorrowedFd)` accepts an explicit writable pipe. It returns
an error for an unusable descriptor. The caller keeps the original descriptor;
the reporter owns a duplicate. Nonblocking mode is shared by both descriptors.
An invalid environment channel instead disables reporting without an error.

| Setting | Default | Meaning |
| --- | --- | --- |
| `root_name` | Empty | Root label |
| `estimated_total_items` | `0` | Root estimate; zero means unknown |
| `refresh_interval` | `Some(80 ms)` | Update interval; `None` for manual |
| `initial_delay` | `200 ms` | Delay before the first automatic report |
| `disabled` | `false` | Disable reporting |
| `name_size` | `NameSize::Current` | 120-byte name field |

A zero update interval or a duration that cannot fit an `Instant` is invalid.
Manual reporters must call `flush()` regularly to collect child reports.

| Method | Action |
| --- | --- |
| `Progress::root()` | Get a root handle |
| `Progress::enabled()` | Check whether the channel is open |
| `Progress::flush()` | Collect children and try to send the current tree |
| `Progress::error()` | Get the first permanent output error |
| `Progress::close()` | Stop updates, try a final report, and close pipes |
| `Node::start(name, total)` | Start a child task |
| `Node::enabled()` | Check whether the task is active |
| `Node::complete_one()` / `add(count)` | Add completed items |
| `Node::set_completed_items(count)` | Set the completed count |
| `Node::set_estimated_total_items(count)` | Set the estimate |
| `Node::increase_estimated_total_items(count)` | Add to the estimate |
| `Node::set_name(name)` | Change the label |
| `Node::end()` | Remove this branch and complete one parent item |
| `Node::spawn(command)` | Start a subprocess with a private channel |
| `unsafe Node::fork()` | Fork with a private channel on Unix |

Task operations are thread-safe. Counters saturate at `u32::MAX` for completed
items and `u32::MAX - 1` for totals. Names stop at the first NUL. They are
shortened at a UTF-8 character boundary and padded with zero bytes. A tree has
at most 253 nodes, including the root. At capacity, `start()` returns a
disabled handle. Local tasks have priority when merged child trees exceed this
limit.

`flush()` returns `Ok(true)` when the current snapshot was sent in full,
`Ok(false)` when disabled or blocked, and an error for permanent output failure.
Writes do not wait for pipe space. A partially written packet is completed before
a newer packet is sent. Closing is best effort: if the pipe stays full, the last
packet can remain incomplete. Close does not set counts to their estimates.
The reporter handles a closed receiver without changing the process-wide
`SIGPIPE` handler.

## Wire compatibility

The [published protocol specification][spec] uses 40-byte task names.
The [current Zig standard library][zig] and current `zpg` use 120-byte names.
Select `NameSize::Legacy` for the published format and older Zig versions.
There is no format negotiation; the supervisor and every reporter in a process
tree must use the same setting.

Reports contain a node count, little-endian counters and fixed-size names, then
one parent byte per node. Child trees are checked for invalid parents, cycles,
reserved values, and truncated packets. The implementation also uses
[`zppy`](https://github.com/asciimoth/zppy) and
[`zpgo`](https://github.com/asciimoth/zpgo) as behavioral references.

[spec]: https://andrewkelley.me/post/zig-new-cli-progress-bar-explained.html#the-zig-progress-protocol-specification
[zig]: https://codeberg.org/ziglang/zig/src/branch/master/lib/std/Progress.zig

## Examples and tests

```sh
just run
just run fork
cargo build --examples
zpg target/debug/examples/demo
zpg target/debug/examples/fork
cargo test --frozen
cargo clippy --frozen --all-targets --all-features -- -D warnings
cargo doc --frozen --no-deps
```

`just run` builds the demo and runs it under the `zpg` tree renderer. On Unix,
the demo starts a `zpg --mock` subprocess and forks a Rust worker without exec.
The parent collects both progress trees and sends the merged tree to the
supervisor. Each task ends when its child exits. `just run fork` runs the
separate example with only a forked Rust worker.

Tests cover both wire formats, malformed child streams, partial writes,
backpressure, task lifetimes, capacity, concurrent updates, standard process
settings, descriptor ownership, forks, nested process trees, and signal handling.
Process fixtures run in separate executables so fork tests do not run inside
libtest's thread pool. Every process test has a timeout.

Compatibility tests run `zpg --test`, check its independent packet validator,
and inspect the merged tree. They run when `zpg` is on `PATH`; otherwise they
print a skip message. Set `ZPRS_REQUIRE_ZPG=1` to make its absence an error.
The Nix development shell provides `zpg`. Use `just test-min` to test Rust 1.75
and `just check-nix` for sandboxed checks with both Rust toolchains.

The project uses the [CC0-1.0 license](LICENSE).
