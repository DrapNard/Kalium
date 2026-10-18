//! Honest detection-only SPP inspection. This module DOES NOT parse Substance
//! projects, decode HBO, or claim Substance file compatibility.
use anyhow::{bail, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const HDF5_SIGNATURE: [u8; 8] = [0x89, b'H', b'D', b'F', b'\r', b'\n', 0x1a, b'\n'];

pub fn inspect(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut head = [0u8; 8];
    let mut found = None;
    for offset in std::iter::once(0u64).chain((9..=22).map(|n| 1u64 << n)) {
        if offset + 8 > len {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut head)?;
        if head == HDF5_SIGNATURE {
            found = Some(offset);
            break;
        }
    }
    match found {
        Some(offset)=>Ok(format!("HDF5 signature detected at offset {offset}. File: {} bytes.\n\nKalium currently supports identification only. Parsing HBO, layers, assets and SPP round-trip editing are NOT implemented.",len)),
        None=>bail!("No standard HDF5 signature detected. This file may use a different container or be invalid. SPP compatibility is not yet implemented."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hdf5_signature_is_correct() {
        assert_eq!(HDF5_SIGNATURE, [137, 72, 68, 70, 13, 10, 26, 10]);
    }
}
