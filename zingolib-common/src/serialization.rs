//! Versioned binary encoding shared by every type persisted in the wallet file.

use std::io::{self, Read, Write};

use byteorder::ReadBytesExt;
use tracing::{Level, event, instrument};

/// Binary encoding with a leading version byte.
///
/// Implementors write `VERSION` first and branch on the byte read back so older
/// layouts stay readable. `ReadInput` and `WriteInput` carry context such as
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
