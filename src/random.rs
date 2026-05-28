//! OS-backed random byte helpers.

use std::io;

/// Fill a byte slice from the operating system CSPRNG.
pub fn fill_bytes(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(io::Error::other)
}
