use super::*;

fn record(parent: u8, name: &[u8]) -> Record {
    Record {
        completed: 0x12345678,
        total: 0x01020304,
        name: name.to_vec(),
        parent,
    }
}

#[test]
fn exact_wire_layout_for_both_formats() {
    for size in [NameSize::Current, NameSize::Legacy] {
        let records = vec![record(255, b"root"), record(0, b"child")];
        let packet = encode(&records, size);
        let stride = 8 + size.bytes();
        assert_eq!(packet.len(), 1 + 2 * (stride + 1));
        assert_eq!(
            &packet[..13],
            &[2, 0x78, 0x56, 0x34, 0x12, 4, 3, 2, 1, b'r', b'o', b'o', b't']
        );
        assert!(packet[13..1 + stride].iter().all(|&b| b == 0));
        assert_eq!(&packet[packet.len() - 2..], &[255, 0]);
        assert_eq!(decode(&packet, size), Some(records));
    }
}

#[test]
fn names_are_bounded_utf8_and_nul_terminated() {
    for size in [NameSize::Current, NameSize::Legacy] {
        assert_eq!(name("a\0ignored", size), b"a");
        assert_eq!(name("", size), b"");
        assert_eq!(
            name(&"x".repeat(size.bytes() + 1), size).len(),
            size.bytes()
        );
        let text = format!("{}🦋", "a".repeat(size.bytes() - 1));
        assert_eq!(name(&text, size), vec![b'a'; size.bytes() - 1]);
        assert!(std::str::from_utf8(&name(&"é".repeat(100), size)).is_ok());
    }
}

#[test]
fn forward_references_are_sorted_and_remapped() {
    let packet = encode(
        &[
            record(255, b"root"),
            record(2, b"leaf"),
            record(0, b"branch"),
        ],
        NameSize::Current,
    );
    let records = decode(&packet, NameSize::Current).unwrap();
    assert_eq!(
        records.iter().map(|r| r.parent).collect::<Vec<_>>(),
        [255, 0, 1]
    );
    assert_eq!(records[1].name, b"branch");
    assert_eq!(records[2].name, b"leaf");
}

#[test]
fn rejects_bad_counts_lengths_and_trees() {
    for packet in [vec![], vec![0], vec![254], vec![255]] {
        assert!(decode(&packet, NameSize::Current).is_none());
    }
    for parents in [
        [0, 0, 1],
        [255, 1, 0],
        [255, 2, 1],
        [255, 254, 0],
        [255, 255, 0],
        [255, 3, 0],
    ] {
        let records: Vec<_> = parents.iter().map(|&p| record(p, b"node")).collect();
        assert!(
            decode(&encode(&records, NameSize::Current), NameSize::Current).is_none(),
            "{parents:?}"
        );
    }
    let packet = encode(&[record(255, b"root")], NameSize::Legacy);
    for length in 0..packet.len() {
        assert!(decode(&packet[..length], NameSize::Legacy).is_none());
    }
    let mut extra = packet;
    extra.push(0);
    assert!(decode(&extra, NameSize::Legacy).is_none());
}

#[test]
fn reserved_total_rejected_but_arbitrary_name_bytes_preserved() {
    let mut r = record(255, &[0xff, 0xfe, 0, b'x']);
    let records = decode(&encode(&[r.clone()], NameSize::Current), NameSize::Current).unwrap();
    assert_eq!(records[0].name, [0xff, 0xfe]);
    r.total = u32::MAX;
    assert!(decode(&encode(&[r], NameSize::Current), NameSize::Current).is_none());
}

#[test]
fn all_tree_sizes_round_trip() {
    for size in [NameSize::Legacy, NameSize::Current] {
        for count in 1..=MAX_NODES {
            let records: Vec<_> = (0..count)
                .map(|i| record(if i == 0 { 255 } else { ((i - 1) / 2) as u8 }, b"node"))
                .collect();
            assert_eq!(decode(&encode(&records, size), size), Some(records));
        }
    }
}

#[test]
fn arbitrary_packets_never_panic_or_produce_invalid_trees() {
    let mut seed = 0x12345678u32;
    for size in [NameSize::Legacy, NameSize::Current] {
        for count in 0..=255 {
            let mut packet = vec![0; 1 + count * (size.bytes() + 9)];
            for byte in &mut packet {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                *byte = (seed >> 16) as u8;
            }
            packet[0] = count as u8;
            if let Some(records) = decode(&packet, size) {
                assert_eq!(records[0].parent, 255);
                for (i, record) in records.iter().enumerate().skip(1) {
                    assert!(usize::from(record.parent) < i);
                }
            }
        }
    }
}
