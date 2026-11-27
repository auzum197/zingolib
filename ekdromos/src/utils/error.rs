use std::fmt;

#[derive(thiserror::Error, Debug, PartialEq)]
pub enum ConversionError {
    DecodeHexFailed(hex::FromHexError),
    InvalidTxidLength(usize),
    InvalidAddress(#[from] zcash_address::ParseError),
    OutsideValidRange,
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConversionError::DecodeHexFailed(e) => write!(f, "failed to decode hex. {e}"),
            ConversionError::InvalidTxidLength(len) => {
                write!(f, "invalid txid length. should be 32 bytes. length: {len}")
            }
            ConversionError::InvalidAddress(e) => {
                write!(f, "invalid recipient address. {e}")
            }
            ConversionError::OutsideValidRange => {
                write!(f, "amount is outside the valid range of zatoshis")
            }
        }
    }
}
