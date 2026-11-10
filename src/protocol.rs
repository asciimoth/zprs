use crate::{NameSize, MAX_NODES, MAX_TOTAL};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub completed: u32,
    pub total: u32,
    pub name: Vec<u8>,
    pub parent: u8,
}

pub(crate) fn name(text: &str, size: NameSize) -> Vec<u8> {
    let text = text.split('\0').next().unwrap_or_default();
    let mut end = text.len().min(size.bytes());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.as_bytes()[..end].to_vec()
}

pub(crate) fn encode(records: &[Record], size: NameSize) -> Vec<u8> {
    let stride = size.bytes() + 8;
    let mut packet = vec![0; 1 + records.len() * (stride + 1)];
    packet[0] = records.len() as u8;
    for (i, record) in records.iter().enumerate() {
        let start = 1 + i * stride;
        packet[start..start + 4].copy_from_slice(&record.completed.to_le_bytes());
        packet[start + 4..start + 8].copy_from_slice(&record.total.to_le_bytes());
        packet[start + 8..start + 8 + record.name.len()].copy_from_slice(&record.name);
        packet[1 + records.len() * stride + i] = record.parent;
    }
    packet
}

// Forward parent references are valid. Sort parents before children for merging.
pub(crate) fn decode(packet: &[u8], size: NameSize) -> Option<Vec<Record>> {
    let count = usize::from(*packet.first()?);
    let stride = 8 + size.bytes();
    if !(1..=MAX_NODES).contains(&count) || packet.len() != 1 + count * (stride + 1) {
        return None;
    }
    let parents = &packet[1 + count * stride..];
    if parents[0] != 255 {
        return None;
    }
    let mut children = vec![Vec::new(); count];
    for (i, &parent) in parents.iter().enumerate().skip(1) {
        children.get_mut(usize::from(parent))?.push(i);
    }
    let mut order = vec![0];
    let mut cursor = 0;
    while cursor < order.len() {
        let node = order[cursor];
        order.extend_from_slice(&children[node]);
        cursor += 1;
    }
    if order.len() != count {
        return None;
    }
    let mut mapped = vec![0; count];
    for (i, &old) in order.iter().enumerate() {
        mapped[old] = i as u8;
    }
    let mut records = Vec::with_capacity(count);
    for old in order {
        let data = &packet[1 + old * stride..][..stride];
        let total = u32::from_le_bytes(data[4..8].try_into().ok()?);
        if total > MAX_TOTAL {
            return None;
        }
        let name = data[8..].split(|&byte| byte == 0).next()?.to_vec();
        records.push(Record {
            completed: u32::from_le_bytes(data[..4].try_into().ok()?),
            total,
            name,
            parent: if old == 0 {
                255
            } else {
                mapped[usize::from(parents[old])]
            },
        });
    }
    Some(records)
}

#[cfg(test)]
mod tests;
