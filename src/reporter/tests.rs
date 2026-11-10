use super::*;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd};

fn reporter(size: NameSize) -> (Progress, File) {
    let (reader, writer) = unix::pipe().unwrap();
    let p = Options {
        refresh_interval: None,
        name_size: size,
        ..Options::new("root", 0)
    }
    .build_with_fd(writer.as_fd())
    .unwrap();
    (p, reader)
}

fn drain(reader: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 65536];
        match unix::read_pipe(reader, &mut buffer) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
    }
    bytes
}

fn snapshot(p: &Progress) -> Vec<Record> {
    let mut state = lock(&p.inner.state);
    let size = state.size;
    protocol::decode(&state.snapshot(), size).unwrap()
}

#[test]
fn counters_names_and_parent_completion() {
    let (p, r) = reporter(NameSize::Current);
    let root = p.root();
    let a = root.start("a", u64::MAX);
    let b = a.start("b", 10);
    a.set_completed_items(u64::MAX);
    a.complete_one();
    a.increase_estimated_total_items(u64::MAX);
    b.add(3);
    b.set_name("renamed\0ignored");
    let records = snapshot(&p);
    assert_eq!(records[1].completed, u32::MAX);
    assert_eq!(records[1].total, MAX_TOTAL);
    assert_eq!(records[2].completed, 3);
    assert_eq!(records[2].name, b"renamed");
    assert_eq!(records[2].parent, 1);
    assert!(p.flush().unwrap());
    assert_eq!(
        protocol::decode(&drain(&r), NameSize::Current),
        Some(records)
    );
    a.end();
    a.end();
    b.end();
    assert_eq!(snapshot(&p)[0].completed, 1);
    assert!(!b.enabled());
    assert!(!b.start("ignored", 0).enabled());
}

#[test]
fn clone_lifetime_and_scope_cleanup() {
    let (p, _) = reporter(NameSize::Current);
    let a = p.root().start("a", 0);
    let b = a.clone();
    drop(a);
    assert_eq!(snapshot(&p).len(), 2);
    drop(b);
    assert_eq!(snapshot(&p).len(), 1);
    assert_eq!(snapshot(&p)[0].completed, 1);
    assert!(p.enabled()); // Root temporaries do not close the reporter.
}

#[test]
fn unwind_ends_task() {
    let (p, _r) = reporter(NameSize::Current);
    let _ = std::panic::catch_unwind(|| {
        let _task = p.root().start("panic", 0);
        panic!("test cleanup");
    });
    assert_eq!(snapshot(&p).len(), 1);
    assert_eq!(snapshot(&p)[0].completed, 1);
}

#[test]
fn subtree_removal_remaps_other_branches() {
    let (p, _) = reporter(NameSize::Current);
    let a = p.root().start("a", 0);
    let b = p.root().start("b", 0);
    let leaf = b.start("leaf", 0);
    a.end();
    let records = snapshot(&p);
    assert_eq!(records[1].name, b"b");
    assert_eq!(records[2].parent, 1);
    drop(leaf);
    assert_eq!(snapshot(&p)[1].completed, 1);
}

#[test]
fn capacity_reuse_and_stale_handles() {
    let (p, _) = reporter(NameSize::Current);
    let tasks: Vec<_> = (0..252).map(|_| p.root().start("task", 0)).collect();
    assert_eq!(snapshot(&p).len(), MAX_NODES);
    assert!(!p.root().start("overflow", 0).enabled());
    tasks[0].end();
    let replacement = p.root().start("replacement", 1);
    tasks[0].set_name("stale");
    tasks[0].end();
    assert!(replacement.enabled());
    assert_eq!(snapshot(&p).last().unwrap().name, b"replacement");
    assert_eq!(snapshot(&p)[0].completed, 1);
}

#[test]
fn concurrent_counts_and_node_lifetimes() {
    let (p, _) = reporter(NameSize::Current);
    thread::scope(|scope| {
        for _ in 0..8 {
            let root = p.root();
            scope.spawn(move || {
                for _ in 0..500 {
                    root.complete_one();
                    let _task = root.start("thread task", 1);
                }
            });
        }
    });
    assert_eq!(snapshot(&p).len(), 1);
    assert_eq!(snapshot(&p)[0].completed, 8000);
}

#[test]
fn disabled_and_closed_handles_are_inert() {
    let p = Options {
        disabled: true,
        ..Options::default()
    }
    .build()
    .unwrap();
    assert!(!p.enabled());
    assert!(!p.flush().unwrap());
    assert!(!p.root().start("disabled", 0).enabled());
    let node = Node::default();
    node.complete_one();
    node.end();
    assert!(!node.enabled());
    let (p, _reader) = reporter(NameSize::Current);
    let task = p.root().start("task", 0);
    p.root().end();
    assert!(!p.enabled());
    task.complete_one();
    task.end();
    p.close().unwrap();
    assert!(!task.enabled());
}

#[test]
fn drop_sends_final_state_and_closes_descriptors() {
    let (p, reader) = reporter(NameSize::Current);
    let root = p.root();
    root.add(7);
    drop(p);
    let records = protocol::decode(&drain(&reader), NameSize::Current).unwrap();
    assert_eq!(records[0].completed, 7);
    assert_eq!(unix::read_pipe(&reader, &mut [0; 1]).unwrap(), 0);
    assert!(!root.enabled());
}

#[test]
fn broken_pipe_is_reported_and_disables_reporter() {
    let (p, reader) = reporter(NameSize::Current);
    drop(reader);
    assert_eq!(p.flush().unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    assert!(!p.enabled());
    assert_eq!(p.error().unwrap().kind(), io::ErrorKind::BrokenPipe);
    assert!(p.close().is_err());
}

#[test]
fn rejects_invalid_explicit_channels_and_durations() {
    let file = File::open("/dev/null").unwrap();
    assert!(Options::default().build_with_fd(file.as_fd()).is_err());
    let (reader, _writer) = unix::pipe().unwrap();
    assert!(Options::default().build_with_fd(reader.as_fd()).is_err());
    assert!(Options {
        refresh_interval: Some(Duration::ZERO),
        disabled: true,
        ..Options::default()
    }
    .build()
    .is_err());
    assert!(Options {
        initial_delay: Duration::MAX,
        disabled: true,
        ..Options::default()
    }
    .build()
    .is_err());
    assert!(!Options {
        disabled: true,
        ..Options::default()
    }
    .build_with_fd(file.as_fd())
    .unwrap()
    .enabled());
}

#[test]
fn explicit_descriptor_is_borrowed_and_duplicate_is_cloexec() {
    let (_reader, writer) = unix::pipe().unwrap();
    let p = Options {
        refresh_interval: None,
        ..Options::default()
    }
    .build_with_fd(writer.as_fd())
    .unwrap();
    let fd = lock(&p.inner.state).output.as_ref().unwrap().as_raw_fd();
    assert_ne!(fd, writer.as_raw_fd());
    assert_ne!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    p.close().unwrap();
    assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFD) } >= 0);
}

#[test]
fn automatic_updates_and_prompt_close() {
    let (reader, writer) = unix::pipe().unwrap();
    let p = Options {
        initial_delay: Duration::ZERO,
        refresh_interval: Some(Duration::from_millis(5)),
        ..Options::new("auto", 2)
    }
    .build_with_fd(writer.as_fd())
    .unwrap();
    p.root().complete_one();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut data = Vec::new();
    while data.len() < 260 {
        assert!(Instant::now() < deadline);
        data.extend(drain(&reader));
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(data.len() % 130, 0);
    p.close().unwrap();
    let p = Options {
        initial_delay: Duration::from_secs(3600),
        ..Options::default()
    }
    .build_with_fd(writer.as_fd())
    .unwrap();
    let start = Instant::now();
    p.close().unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn backpressure_never_blocks_and_preserves_frame_boundaries() {
    for size in [NameSize::Current, NameSize::Legacy] {
        let (p, reader) = reporter(size);
        let tasks: Vec<_> = (0..252).map(|_| p.root().start("task", 1)).collect();
        let old = lock(&p.inner.state).snapshot();
        let start = Instant::now();
        let first = p.flush().unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        // A 253-node frame exceeds PIPE_BUF, and on Linux also pipe capacity.
        let mut data = drain(&reader);
        p.root().set_name("new state");
        let new = lock(&p.inner.state).snapshot();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let done = p.flush().unwrap();
            data.extend(drain(&reader));
            if done {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        let expected = [old.as_slice(), new.as_slice()].concat();
        assert_eq!(data, expected, "first flush complete: {first}");
        drop(tasks);
    }
}

#[test]
fn full_pipe_drops_unsent_stale_snapshot() {
    let (p, reader) = reporter(NameSize::Current);
    let output = lock(&p.inner.state)
        .output
        .as_ref()
        .unwrap()
        .try_clone()
        .unwrap();
    while unix::write_pipe(&output, &[0; 4096]).is_ok() {}
    p.root().set_name("stale");
    assert!(!p.flush().unwrap());
    p.root().set_name("latest");
    drain(&reader);
    assert!(p.flush().unwrap());
    assert_eq!(
        protocol::decode(&drain(&reader), NameSize::Current).unwrap()[0].name,
        b"latest"
    );
}

fn child_record(parent: u8, name: &[u8]) -> Record {
    Record {
        completed: 3,
        total: 8,
        name: name.to_vec(),
        parent,
    }
}

#[test]
fn child_fragmentation_latest_report_and_eof_retention() {
    for size in [NameSize::Current, NameSize::Legacy] {
        let (reader, mut writer) = unix::pipe().unwrap();
        let mut child = ChildPipe::new(reader);
        let packet = protocol::encode(
            &[child_record(255, b"child"), child_record(0, b"leaf")],
            size,
        );
        for byte in &packet[..packet.len() - 1] {
            writer.write_all(&[*byte]).unwrap();
            child.read(size);
            assert!(child.snapshot.is_empty());
        }
        writer.write_all(&packet[packet.len() - 1..]).unwrap();
        child.read(size);
        assert_eq!(child.snapshot.len(), 2);
        let latest = vec![child_record(255, b"latest")];
        writer.write_all(&protocol::encode(&latest, size)).unwrap();
        writer.write_all(&protocol::encode(&latest, size)).unwrap();
        drop(writer);
        child.read(size);
        assert_eq!(child.snapshot, latest);
        assert!(child.pipe.is_none());
    }
}

#[test]
fn malformed_or_truncated_child_preserves_last_good_tree() {
    for malformed in [
        vec![0],
        vec![254],
        vec![255],
        vec![1, 0, 0],
        protocol::encode(&[child_record(0, b"bad root")], NameSize::Current),
    ] {
        let (reader, mut writer) = unix::pipe().unwrap();
        let mut child = ChildPipe::new(reader);
        let good = vec![child_record(255, b"good")];
        writer
            .write_all(&protocol::encode(&good, NameSize::Current))
            .unwrap();
        writer.write_all(&malformed).unwrap();
        drop(writer);
        child.read(NameSize::Current);
        assert!(child.pipe.is_none());
        assert_eq!(child.snapshot, good);
    }
}

#[test]
fn merge_replaces_root_keeps_empty_name_and_remaps_children() {
    let (p, _) = reporter(NameSize::Current);
    let a = p.root().start("local", 1);
    let _b = p.root().start("other", 1);
    let (reader, mut writer) = unix::pipe().unwrap();
    lock(&p.inner.state).nodes[1].child = Some(ChildPipe::new(reader));
    assert_eq!(snapshot(&p)[1].total, 1);
    writer
        .write_all(&protocol::encode(
            &[
                child_record(255, b""),
                child_record(2, b"leaf"),
                child_record(0, b"branch"),
            ],
            NameSize::Current,
        ))
        .unwrap();
    let records = snapshot(&p);
    assert_eq!(records[1].name, b"local");
    assert_eq!(records[1].completed, 3);
    assert_eq!(records[3].name, b"branch");
    assert_eq!(records[3].parent, 1);
    assert_eq!(records[4].parent, 3);
    drop(writer);
    assert_eq!(snapshot(&p), records);
    a.end();
    assert_eq!(snapshot(&p).len(), 2);
    assert_eq!(snapshot(&p)[0].completed, 1);
}

#[test]
fn local_nodes_have_priority_over_large_child_tree() {
    let (p, _) = reporter(NameSize::Current);
    let _tasks: Vec<_> = (0..250).map(|_| p.root().start("local", 0)).collect();
    let (reader, _writer) = unix::pipe().unwrap();
    let mut child = ChildPipe::new(reader);
    child.snapshot = (0..253)
        .map(|i| child_record(if i == 0 { 255 } else { (i - 1) as u8 }, b"remote"))
        .collect();
    lock(&p.inner.state).nodes[1].child = Some(child);
    let records = snapshot(&p);
    assert_eq!(records.len(), MAX_NODES);
    assert_eq!(records[250].name, b"local");
    assert_eq!(records[251].parent, 1);
    assert_eq!(records[252].parent, 251);
}

#[test]
fn end_closes_child_reader() {
    let (p, _) = reporter(NameSize::Current);
    let a = p.root().start("child", 0);
    let (reader, writer) = unix::pipe().unwrap();
    lock(&p.inner.state).nodes[1].child = Some(ChildPipe::new(reader));
    a.end();
    assert_eq!(
        unix::write_pipe(&writer, b"x").unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
}
