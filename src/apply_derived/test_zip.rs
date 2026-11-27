//! A small zip writer for tests, able to build the archives a real writer
//! refuses to: symlinks, `..` names, setuid bits and lying sizes.

use std::io::Write;

/// One entry to write.
#[derive(Clone, Debug)]
pub(crate) struct TestEntry {
    pub name: String,
    pub data: Vec<u8>,
    /// Unix mode in the external attributes (with the file-type bits).
    pub mode: u32,
    pub deflate: bool,
    /// Override the stored uncompressed size.
    pub claimed_size: Option<u32>,
}

impl TestEntry {
    pub(crate) fn file(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.to_owned(),
            data: data.to_vec(),
            mode: 0o100_644,
            deflate: true,
            claimed_size: None,
        }
    }

    pub(crate) fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
}

fn crc(data: &[u8]) -> u32 {
    let mut sum = flate2::Crc::new();
    sum.update(data);
    sum.sum()
}

/// Build an archive the way `actions/upload-artifact` does: Unix host, data
/// descriptors (sizes zero in the local header), deflate.
pub(crate) fn build(entries: &[TestEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        let offset = u32::try_from(out.len()).unwrap();
        let body = if entry.deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&entry.data).unwrap();
            encoder.finish().unwrap()
        } else {
            entry.data.clone()
        };
        let method: u16 = if entry.deflate { 8 } else { 0 };
        let flags: u16 = 0x0008;
        let crc = crc(&entry.data);
        let size = entry
            .claimed_size
            .unwrap_or_else(|| u32::try_from(entry.data.len()).unwrap());
        let compressed = u32::try_from(body.len()).unwrap();
        let name = entry.name.as_bytes();
        let name_len = u16::try_from(name.len()).unwrap();
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20_u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(name);
        out.extend_from_slice(&body);
        out.extend_from_slice(&0x0807_4b50_u32.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&compressed.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());

        central.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        central.extend_from_slice(&0x032d_u16.to_le_bytes());
        central.extend_from_slice(&20_u16.to_le_bytes());
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&method.to_le_bytes());
        central.extend_from_slice(&[0; 4]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&compressed.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&[0; 8]);
        central.extend_from_slice(&((entry.mode << 16) | 0x20).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_offset = u32::try_from(out.len()).unwrap();
    let cd_size = u32::try_from(central.len()).unwrap();
    let count = u16::try_from(entries.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out
}
