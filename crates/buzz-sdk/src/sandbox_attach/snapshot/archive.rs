//! Preflight from the official Git bundle/pack specifications. Git must still
//! verify integrity and inventory imported objects before any checkout.
use super::*;
use flate2::{Decompress, FlushDecompress, Status};
use std::time::{Duration, Instant};

/// Largest number of historical objects accepted in one snapshot.
pub const MAX_OBJECTS: u32 = 200000;
/// Bounded expansion inventory, calculated before Git imports or checks out bytes.
pub struct Inventory {
    /// Total number of packed objects.
    pub objects: u32,
    /// Sum of reconstructed object sizes, including delta targets.
    pub expanded_bytes: u64,
    /// Bundle references, as object-ID/ref-name pairs. Git validates ref syntax later.
    pub refs: Vec<(String, String)>,
    /// Object ID length, in bytes (20 for SHA-1, 32 for SHA-256).
    pub hash_bytes: usize,
}
fn take<'a>(bytes: &'a [u8], position: &mut usize, count: usize) -> Result<&'a [u8], SdkError> {
    let end = position
        .checked_add(count)
        .ok_or_else(|| invalid("snapshot bundle offset overflow"))?;
    let value = bytes
        .get(*position..end)
        .ok_or_else(|| invalid("truncated snapshot bundle"))?;
    *position = end;
    Ok(value)
}
fn byte(bytes: &[u8], position: &mut usize) -> Result<u8, SdkError> {
    Ok(take(bytes, position, 1)?[0])
}
fn varint(bytes: &[u8], position: &mut usize) -> Result<u64, SdkError> {
    let mut value = 0u64;
    for shift in (0..=63).step_by(7) {
        let part = byte(bytes, position)?;
        value = value
            .checked_add(
                u64::from(part & 127)
                    .checked_mul(1u64 << shift)
                    .ok_or_else(|| invalid("snapshot integer overflow"))?,
            )
            .ok_or_else(|| invalid("snapshot integer overflow"))?;
        if part & 128 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("snapshot integer overflow"))
}
fn size(bytes: &[u8], position: &mut usize) -> Result<(u8, u64), SdkError> {
    let mut part = byte(bytes, position)?;
    let kind = (part >> 4) & 7;
    let mut value = u64::from(part & 15);
    for shift in (4..=60).step_by(7) {
        if part & 128 == 0 {
            return Ok((kind, value));
        }
        part = byte(bytes, position)?;
        value = value
            .checked_add(
                u64::from(part & 127)
                    .checked_mul(1u64 << shift)
                    .ok_or_else(|| invalid("snapshot object size overflow"))?,
            )
            .ok_or_else(|| invalid("snapshot object size overflow"))?;
    }
    if part & 128 != 0 {
        return Err(invalid("snapshot object size overflow"));
    }
    Ok((kind, value))
}
fn bounded(value: u64, budget: u64) -> Result<(), SdkError> {
    if value > budget {
        return Err(invalid("snapshot Git expansion exceeds budget"));
    }
    Ok(())
}
struct Header {
    start: usize,
    hash_bytes: usize,
    refs: Vec<(String, String)>,
}
fn header(bytes: &[u8]) -> Result<Header, SdkError> {
    let end = bytes[..bytes.len().min(8 * 1024 * 1024)]
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .ok_or_else(|| invalid("invalid snapshot bundle header"))?
        + 2;
    let text = std::str::from_utf8(&bytes[..end])
        .map_err(|_| invalid("invalid snapshot bundle header"))?;
    let mut lines = text.lines();
    let signature = lines
        .next()
        .ok_or_else(|| invalid("missing snapshot bundle signature"))?;
    if !matches!(signature, "# v2 git bundle" | "# v3 git bundle") {
        return Err(invalid("unsupported snapshot bundle version"));
    }
    let mut hash_bytes = 20;
    let mut capability = false;
    let mut refs = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if line.starts_with('-') {
            return Err(invalid("snapshot bundle requires external history"));
        }
        if line.starts_with('@') {
            if signature != "# v3 git bundle" || capability || !refs.is_empty() {
                return Err(invalid("invalid snapshot bundle capabilities"));
            }
            hash_bytes = match line {
                "@object-format=sha1" => 20,
                "@object-format=sha256" => 32,
                _ => return Err(invalid("unsupported snapshot bundle capability")),
            };
            capability = true;
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| invalid("invalid snapshot bundle reference"))?;
        if oid.len() != hash_bytes * 2
            || !oid.bytes().all(|c| c.is_ascii_hexdigit())
            || name.len() > 1024
            || name.chars().any(|c| c.is_whitespace() || c.is_control())
            || !(name == "HEAD" || name.starts_with("refs/"))
            || name.starts_with("refs/replace/")
            || refs.len() >= MAX_FILES
        {
            return Err(invalid("invalid snapshot bundle reference"));
        }
        refs.push((oid.to_owned(), name.to_owned()));
    }
    if refs.is_empty() {
        return Err(invalid("snapshot bundle has no references"));
    }
    Ok(Header {
        start: end,
        hash_bytes,
        refs,
    })
}
/// Refuse declared object/delta expansion, aggregate expansion, count overflow,
/// incomplete packs and external prerequisites without allocating expanded objects.
/// `budget` may tighten, never raise, the 256 MiB source limit.
/// See https://git-scm.com/docs/gitformat-pack and gitformat-bundle.
pub fn inspect_bundle(bytes: &[u8], budget: u64) -> Result<Inventory, SdkError> {
    if bytes.len() > MAX_BLOB_BYTES || budget == 0 || budget > MAX_SOURCE_BYTES {
        return Err(invalid("invalid snapshot bundle budget"));
    }
    let Header {
        start,
        hash_bytes,
        refs,
    } = header(bytes)?;
    let pack = &bytes[start..];
    let mut position = 0;
    if take(pack, &mut position, 4)? != b"PACK" {
        return Err(invalid("snapshot bundle has no pack"));
    }
    let version = u32::from_be_bytes(
        take(pack, &mut position, 4)?
            .try_into()
            .map_err(|_| invalid("invalid pack version"))?,
    );
    let objects = u32::from_be_bytes(
        take(pack, &mut position, 4)?
            .try_into()
            .map_err(|_| invalid("invalid pack count"))?,
    );
    if !matches!(version, 2 | 3) || objects == 0 || objects > MAX_OBJECTS {
        return Err(invalid("snapshot Git object count exceeds budget"));
    }
    let started = Instant::now();
    let mut expanded_bytes = 0u64;
    let mut inflated_bytes = 0u64;
    let mut output = [0u8; 16384];
    for _ in 0..objects {
        let (kind, declared) = size(pack, &mut position)?;
        bounded(declared, budget)?;
        inflated_bytes = inflated_bytes
            .checked_add(declared)
            .ok_or_else(|| invalid("snapshot expansion overflow"))?;
        bounded(inflated_bytes, budget)?;
        match kind {
            1..=4 => {}
            6 => {
                let mut done = false;
                for _ in 0..10 {
                    if byte(pack, &mut position)? & 128 == 0 {
                        done = true;
                        break;
                    }
                }
                if !done {
                    return Err(invalid("snapshot delta offset overflow"));
                }
            }
            7 => {
                take(pack, &mut position, hash_bytes)?;
            }
            _ => return Err(invalid("unsupported snapshot Git object type")),
        }
        let mut inflater = Decompress::new(true);
        let mut prefix = Vec::with_capacity(32);
        let mut expanded = if kind <= 4 { Some(declared) } else { None };
        if let Some(value) = expanded {
            bounded(
                expanded_bytes
                    .checked_add(value)
                    .ok_or_else(|| invalid("snapshot expansion overflow"))?,
                budget,
            )?;
        }
        loop {
            if started.elapsed() > Duration::from_secs(30) {
                return Err(invalid("snapshot Git preflight timed out"));
            }
            let before_in = inflater.total_in();
            let before_out = inflater.total_out();
            let input = pack
                .get(position..position.saturating_add(65536).min(pack.len()))
                .ok_or_else(|| invalid("truncated snapshot Git object"))?;
            let status = inflater
                .decompress(input, &mut output, FlushDecompress::None)
                .map_err(|_| invalid("invalid compressed snapshot Git object"))?;
            let consumed = (inflater.total_in() - before_in) as usize;
            let produced = (inflater.total_out() - before_out) as usize;
            position = position
                .checked_add(consumed)
                .ok_or_else(|| invalid("snapshot bundle offset overflow"))?;
            if inflater.total_out() > declared {
                return Err(invalid("snapshot Git object exceeds declared size"));
            }
            if expanded.is_none() {
                let count = produced.min(32 - prefix.len());
                prefix.extend_from_slice(&output[..count]);
                if prefix.len() >= 20 || status == Status::StreamEnd {
                    let mut offset = 0;
                    let base = varint(&prefix, &mut offset)?;
                    bounded(base, budget)?;
                    let target = varint(&prefix, &mut offset)?;
                    bounded(target, budget)?;
                    bounded(
                        expanded_bytes
                            .checked_add(target)
                            .ok_or_else(|| invalid("snapshot expansion overflow"))?,
                        budget,
                    )?;
                    expanded = Some(target);
                }
            }
            if status == Status::StreamEnd {
                if inflater.total_out() != declared {
                    return Err(invalid("snapshot Git object size mismatch"));
                }
                break;
            }
            if consumed == 0 && produced == 0 {
                return Err(invalid("truncated snapshot Git object"));
            }
        }
        expanded_bytes = expanded_bytes
            .checked_add(expanded.ok_or_else(|| invalid("invalid snapshot delta"))?)
            .ok_or_else(|| invalid("snapshot expansion overflow"))?;
        bounded(expanded_bytes, budget)?;
    }
    if pack.len().saturating_sub(position) != hash_bytes {
        return Err(invalid("snapshot pack trailer is incomplete"));
    }
    Ok(Inventory {
        objects,
        expanded_bytes,
        refs,
        hash_bytes,
    })
}
#[cfg(test)]
mod tests;
