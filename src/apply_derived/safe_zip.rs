//! An in-memory reader for the zip archives GitHub serves for workflow
//! artifacts, strict enough for attacker-influenced input.
//!
//! It reads the central directory, cross-checks each local header, and
//! inflates each entry with its uncompressed size capped while it inflates.
//! It verifies every CRC-32. It accepts only what an artifact upload
//! produces: stored or deflated regular files, and empty directory entries
//! (skipped).
//!
//! It refuses:
//! - encryption;
//! - zip64 and multi-disk archives;
//! - names that are not UTF-8, or not ASCII without the UTF-8 flag;
//! - symlinks and any other non-regular Unix file type;
//! - setuid, setgid and sticky bits;
//! - an entry written by a host other than MS-DOS or Unix;
//! - duplicate names (compared case-insensitively, for case-insensitive
//!   filesystems);
//! - entries whose data overlap.
//!
//! It never extracts anything to disk: the caller gets names and bytes.

use std::io::Read;

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const EOCD_LEN: usize = 22;
const CENTRAL_LEN: usize = 46;
const LOCAL_LEN: usize = 30;
const FLAG_ENCRYPTED: u16 = 1;
const FLAG_STRONG_ENCRYPTION: u16 = 1 << 6;
const FLAG_UTF8: u16 = 1 << 11;
const FLAG_MASKED_HEADER: u16 = 1 << 13;
const HOST_DOS: u16 = 0;
const HOST_UNIX: u16 = 3;
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const SPECIAL_BITS: u32 = 0o7000;
const DOS_DIRECTORY: u32 = 0x10;

/// Size limits for one archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// The archive's own size.
    pub max_archive_bytes: usize,
    /// File entries.
    pub max_entries: usize,
    /// One entry, uncompressed.
    pub max_entry_bytes: u64,
    /// Every entry together, uncompressed.
    pub max_total_bytes: u64,
}

impl Limits {
    /// The defaults for a derived-files artifact.
    pub const DEFAULT: Self = Self {
        max_archive_bytes: 16 * 1024 * 1024,
        max_entries: 4096,
        max_entry_bytes: 2 * 1024 * 1024,
        max_total_bytes: 64 * 1024 * 1024,
    };
}

/// One regular-file entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name as stored (not yet normalized).
    pub name: String,
    /// Its uncompressed bytes.
    pub data: Vec<u8>,
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| "truncated archive".to_owned())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| "truncated archive".to_owned())
}

fn usize_of(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The end-of-central-directory record's offset.
fn find_eocd(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < EOCD_LEN {
        return Err("not a zip archive (too short)".to_owned());
    }
    let lowest = bytes.len().saturating_sub(EOCD_LEN + usize::from(u16::MAX));
    let mut at = bytes.len() - EOCD_LEN;
    loop {
        if u32_at(bytes, at)? == EOCD_SIGNATURE
            && at + EOCD_LEN + usize::from(u16_at(bytes, at + 20)?) == bytes.len()
        {
            return Ok(at);
        }
        if at == lowest {
            return Err("not a zip archive (no end of central directory)".to_owned());
        }
        at -= 1;
    }
}

/// Parse `bytes` into its regular-file entries.
#[allow(
    clippy::too_many_lines,
    reason = "one pass over the central directory; every refusal sits next to the field it reads"
)]
pub fn read(bytes: &[u8], limits: &Limits) -> Result<Vec<Entry>, String> {
    if bytes.len() > limits.max_archive_bytes {
        return Err(format!(
            "archive is {} bytes, over the {} byte limit",
            bytes.len(),
            limits.max_archive_bytes
        ));
    }
    let eocd = find_eocd(bytes)?;
    if eocd >= 20 && u32_at(bytes, eocd - 20)? == ZIP64_LOCATOR_SIGNATURE {
        return Err("zip64 archives are not accepted".to_owned());
    }
    let disk = u16_at(bytes, eocd + 4)?;
    let cd_disk = u16_at(bytes, eocd + 6)?;
    let disk_entries = u16_at(bytes, eocd + 8)?;
    let entries = u16_at(bytes, eocd + 10)?;
    let cd_size = u32_at(bytes, eocd + 12)?;
    let cd_offset = u32_at(bytes, eocd + 16)?;
    if entries == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err("zip64 archives are not accepted".to_owned());
    }
    if disk != 0 || cd_disk != 0 || disk_entries != entries {
        return Err("multi-disk archives are not accepted".to_owned());
    }
    let cd_start = usize_of(cd_offset);
    let cd_end = cd_start
        .checked_add(usize_of(cd_size))
        .filter(|end| *end <= eocd)
        .ok_or("central directory lies outside the archive")?;
    if usize::from(entries) > limits.max_entries {
        return Err(format!(
            "archive has {entries} entries, over the {} entry limit",
            limits.max_entries
        ));
    }

    let mut out: Vec<Entry> = Vec::new();
    let mut folded: Vec<String> = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut total: u64 = 0;
    let mut at = cd_start;
    for _ in 0..entries {
        if at + CENTRAL_LEN > cd_end || u32_at(bytes, at)? != CENTRAL_SIGNATURE {
            return Err("malformed central directory".to_owned());
        }
        let made_by = u16_at(bytes, at + 4)?;
        let flags = u16_at(bytes, at + 8)?;
        let method = u16_at(bytes, at + 10)?;
        let crc = u32_at(bytes, at + 16)?;
        let compressed = u32_at(bytes, at + 20)?;
        let size = u32_at(bytes, at + 24)?;
        let name_len = usize::from(u16_at(bytes, at + 28)?);
        let extra_len = usize::from(u16_at(bytes, at + 30)?);
        let comment_len = usize::from(u16_at(bytes, at + 32)?);
        let start_disk = u16_at(bytes, at + 34)?;
        let external = u32_at(bytes, at + 38)?;
        let local = usize_of(u32_at(bytes, at + 42)?);
        let name_bytes = bytes
            .get(at + CENTRAL_LEN..at + CENTRAL_LEN + name_len)
            .ok_or("truncated central directory")?;
        let next = at + CENTRAL_LEN + name_len + extra_len + comment_len;
        if next > cd_end {
            return Err("truncated central directory".to_owned());
        }
        at = next;

        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| "entry name is not UTF-8".to_owned())?
            .to_owned();
        if flags & FLAG_UTF8 == 0 && !name.is_ascii() {
            return Err(format!(
                "entry `{name}`: non-ASCII name without the UTF-8 flag"
            ));
        }
        if flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION | FLAG_MASKED_HEADER) != 0 {
            return Err(format!("entry `{name}` is encrypted"));
        }
        if compressed == u32::MAX || size == u32::MAX || local == usize::MAX {
            return Err(format!("entry `{name}`: zip64 entries are not accepted"));
        }
        if start_disk != 0 {
            return Err(format!(
                "entry `{name}`: multi-disk entries are not accepted"
            ));
        }
        let host = made_by >> 8;
        let is_dir = match host {
            HOST_UNIX => {
                let mode = external >> 16;
                if mode & SPECIAL_BITS != 0 {
                    return Err(format!(
                        "entry `{name}` sets setuid, setgid or sticky bits ({mode:o})"
                    ));
                }
                match mode & S_IFMT {
                    0 => name.ends_with('/'),
                    S_IFREG => false,
                    S_IFDIR => true,
                    _ => {
                        return Err(format!(
                            "entry `{name}` is not a regular file (mode {mode:o}); symlinks \
                             and special files are refused"
                        ));
                    }
                }
            }
            HOST_DOS => external & DOS_DIRECTORY != 0 || name.ends_with('/'),
            other => {
                return Err(format!("entry `{name}`: unsupported host system {other}"));
            }
        };
        if is_dir != name.ends_with('/') {
            return Err(format!("entry `{name}`: directory flag and name disagree"));
        }
        if is_dir {
            if size != 0 || compressed != 0 {
                return Err(format!("directory entry `{name}` carries data"));
            }
            continue;
        }
        let key = name.to_lowercase();
        if folded.contains(&key) {
            return Err(format!("duplicate entry `{name}`"));
        }
        folded.push(key);
        if out.len() >= limits.max_entries {
            return Err(format!("over the {} entry limit", limits.max_entries));
        }

        if local + LOCAL_LEN > cd_start || u32_at(bytes, local)? != LOCAL_SIGNATURE {
            return Err(format!("entry `{name}`: bad local header"));
        }
        let local_method = u16_at(bytes, local + 8)?;
        let local_name_len = usize::from(u16_at(bytes, local + 26)?);
        let local_extra_len = usize::from(u16_at(bytes, local + 28)?);
        let local_name = bytes
            .get(local + LOCAL_LEN..local + LOCAL_LEN + local_name_len)
            .ok_or("truncated local header")?;
        if local_name != name_bytes || local_method != method {
            return Err(format!(
                "entry `{name}`: local header disagrees with the directory"
            ));
        }
        let data_start = local + LOCAL_LEN + local_name_len + local_extra_len;
        let data_end = data_start
            .checked_add(usize_of(compressed))
            .filter(|end| *end <= cd_start)
            .ok_or_else(|| format!("entry `{name}`: data lies outside the archive"))?;
        if ranges
            .iter()
            .any(|(start, end)| local < *end && *start < data_end)
        {
            return Err(format!("entry `{name}` overlaps another entry"));
        }
        ranges.push((local, data_end));
        let raw = &bytes[data_start..data_end];

        if u64::from(size) > limits.max_entry_bytes {
            return Err(format!(
                "entry `{name}` is {size} bytes, over the {} byte limit",
                limits.max_entry_bytes
            ));
        }
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut data = Vec::new();
                flate2::read::DeflateDecoder::new(raw)
                    .take(limits.max_entry_bytes + 1)
                    .read_to_end(&mut data)
                    .map_err(|error| format!("entry `{name}` does not inflate: {error}"))?;
                data
            }
            other => {
                return Err(format!(
                    "entry `{name}`: compression method {other} is refused"
                ));
            }
        };
        if data.len() as u64 > limits.max_entry_bytes {
            return Err(format!(
                "entry `{name}` inflates past the {} byte limit",
                limits.max_entry_bytes
            ));
        }
        if data.len() != usize_of(size) {
            return Err(format!(
                "entry `{name}`: {} bytes, the directory says {size}",
                data.len()
            ));
        }
        total += data.len() as u64;
        if total > limits.max_total_bytes {
            return Err(format!(
                "entries total over the {} byte limit",
                limits.max_total_bytes
            ));
        }
        let mut sum = flate2::Crc::new();
        sum.update(&data);
        if sum.sum() != crc {
            return Err(format!("entry `{name}`: CRC-32 mismatch"));
        }
        out.push(Entry { name, data });
    }
    Ok(out)
}
