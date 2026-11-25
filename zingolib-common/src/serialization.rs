//! Versioned binary encoding shared by every type persisted in the wallet file.

use std::io::{self, Read, Write};

use byteorder::ReadBytesExt;
use tracing::{Level, event, instrument};

/// Binary encoding with a leading version byte.
///
/// Implementors that version their layout write `VERSION` first and check the byte
/// read back with `get_version`. `ReadInput` and `WriteInput` carry context such as
/// consensus parameters or the chain type.
pub trait ReadableWriteable<ReadInput = (), WriteInput = ()>: Sized {
    /// Layout version written by `write` and the newest one `read` accepts.
    const VERSION: u8;

    /// Decode from `reader`.
    fn read<R: Read>(reader: R, input: ReadInput) -> io::Result<Self>;

    /// Encode into `writer`.
    fn write<W: Write>(&self, writer: W, input: WriteInput) -> io::Result<()>;

    /// Reads the version byte, rejecting layouts newer than `VERSION`.
    #[instrument(level = "info", skip(reader))]
    fn get_version<R: Read>(mut reader: R) -> io::Result<u8> {
        let external_version = reader.read_u8()?;
        if external_version > Self::VERSION {
            event!(
                Level::ERROR,
                where = std::any::type_name::<Self>(),
                got_version = external_version,
                expected_version = Self::VERSION,
                kind = ?io::ErrorKind::InvalidData,
                msg = "Struct version is from a future version of zingo"
            );
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Struct version \"{external_version}\" is from future version of zingo",),
            ))
        } else {
            Ok(external_version)
        }
    }
}

/// An `InvalidData` error carrying `message`.
pub fn invalid_data(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Rejects a struct version older than the one the first layout 41 release wrote. Those layouts are
/// no longer readable, and decoding one with the current layout would misread its fields.
pub fn check_oldest_version(version: u8, oldest: u8, name: &str) -> io::Result<u8> {
    if version < oldest {
        Err(invalid_data(format!(
            "{name} version {version} predates the oldest readable version {oldest}"
        )))
    } else {
        Ok(version)
    }
}

/// Collects decoded entries into a map or set, rejecting repeats. Every writer iterates a map or
/// set, so a repeat is corruption, and collecting it would silently drop an entry.
pub fn collect_unique<T, C: FromIterator<T>>(
    entries: Vec<T>,
    len: fn(&C) -> usize,
    name: &str,
) -> io::Result<C> {
    let count = entries.len();
    let collected = entries.into_iter().collect();
    if len(&collected) == count {
        Ok(collected)
    } else {
        Err(invalid_data(format!("duplicate {name}")))
    }
}
