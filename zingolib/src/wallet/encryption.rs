//! Passphrase-based at-rest encryption for the serialized wallet file.
//!
//! The entire serialized wallet (the buffer produced by [`crate::wallet::LightWallet::write`])
//! is wrapped in a single AEAD envelope:
//!
//! ```text
//! offset
//! 0   magic        8 bytes   = MAGIC (b"ZiLEnc01")
//! 8   format_ver   u8        = ENVELOPE_VERSION
//! 9   kdf_id       u8        = KDF_ARGON2ID
//! 10  m_cost       u32-LE    Argon2 memory cost (KiB)
//! 14  t_cost       u32-LE    Argon2 time cost (iterations)
//! 18  p_cost       u8        Argon2 parallelism (lanes)
//! 19  salt         16 bytes  random per wallet, fixed for the wallet's life
//! 35  nonce        24 bytes  random per save (XChaCha20 192-bit nonce)
//! 59  ciphertext   variable  XChaCha20Poly1305(key, nonce, aad = header[0..59], plaintext)
//!     tag          16 bytes  appended by the AEAD
//! ```
//!
//! The header (including the KDF parameters and salt) is stored in the clear so the file is
//! self-describing and the parameters can be upgraded in a future format version. The full
//! header is bound as AEAD associated data so it cannot be tampered with.
//!
//! ## Threat model
//! This protects the wallet file *at rest* (a stolen backup, a discarded disk, a synced
//! cloud backup). It does **not** protect a wallet that is currently unlocked: the keys must
//! live in plaintext in RAM to sync and sign, which is unavoidable for a hot wallet. The
//! [`zeroize`] usage here is defense-in-depth, not a guarantee.
//!
//! ## Why the magic can't collide with a plaintext wallet
//! [`crate::wallet::LightWallet::read`] reads the first 8 bytes as a little-endian `u64`
//! version number (currently small values like 39). `MAGIC` read as a `u64-LE` is
//! `0x31_30_63_6e_45_69_4c_5a`, far outside any real version, so an encrypted file can never
//! be mistaken for a plaintext one and vice versa.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::{RngCore, rngs::OsRng};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

/// 8-byte file magic identifying an encrypted wallet envelope.
pub(crate) const MAGIC: [u8; 8] = *b"ZiLEnc01";
/// The magic interpreted as a little-endian `u64`, matching how
/// [`crate::wallet::LightWallet::read`] reads the leading version field. Used to detect an
/// encrypted file from the version-reader path and return a clear error.
pub(crate) const MAGIC_AS_VERSION: u64 = u64::from_le_bytes(MAGIC);
/// Version of the *envelope* format (independent of the inner wallet serialization version).
const ENVELOPE_VERSION: u8 = 1;
/// KDF identifier stored in the header. Only Argon2id is currently supported.
const KDF_ARGON2ID: u8 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
/// Length of the cleartext header preceding the ciphertext.
const HEADER_LEN: usize = 8 + 1 + 1 + 4 + 4 + 1 + SALT_LEN + NONCE_LEN; // = 59

/// Errors that can occur while encrypting or decrypting a wallet file.
#[derive(Debug, thiserror::Error)]
pub enum WalletEncryptionError {
    /// The file is encrypted but no passphrase was supplied.
    #[error("wallet file is encrypted but no passphrase was provided")]
    PassphraseRequired,
    /// The header was truncated or did not begin with the expected magic.
    #[error("malformed encrypted wallet header")]
    MalformedHeader,
    /// The envelope format version is newer than this build understands.
    #[error("unsupported encrypted wallet format version {0}")]
    UnsupportedVersion(u8),
    /// The KDF identifier in the header is not one we support.
    #[error("unsupported key-derivation function id {0}")]
    UnsupportedKdf(u8),
    /// The Argon2 parameters in the header were invalid.
    #[error("invalid argon2 parameters: {0}")]
    InvalidParams(String),
    /// Key derivation failed.
    #[error("key derivation failed: {0}")]
    KeyDerivation(String),
    /// Decryption/authentication failed — almost always a wrong passphrase or a corrupt file.
    #[error("decryption failed (wrong passphrase or corrupt wallet file)")]
    DecryptionFailed,
}

/// Argon2id cost parameters.
///
/// Stored in the envelope header so a file is self-describing and parameters can be tuned per
/// platform or upgraded later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Params {
    /// Memory cost in KiB.
    pub m_cost: u32,
    /// Time cost (number of iterations).
    pub t_cost: u32,
    /// Degree of parallelism (lanes).
    pub p_cost: u8,
}

impl Argon2Params {
    /// Desktop preset: 64 MiB, 3 iterations, 1 lane (OWASP first recommendation).
    pub const fn desktop() -> Self {
        Self {
            m_cost: 64 * 1024,
            t_cost: 3,
            p_cost: 1,
        }
    }

    /// Mobile preset: 19 MiB, 2 iterations, 1 lane (OWASP second recommendation), suitable
    /// for memory-constrained devices.
    pub const fn mobile() -> Self {
        Self {
            m_cost: 19 * 1024,
            t_cost: 2,
            p_cost: 1,
        }
    }

    fn to_argon2(self) -> Result<Argon2<'static>, WalletEncryptionError> {
        let params = Params::new(
            self.m_cost,
            self.t_cost,
            self.p_cost as u32,
            Some(KEY_LEN),
        )
        .map_err(|e| WalletEncryptionError::InvalidParams(e.to_string()))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self::desktop()
    }
}

/// A cached encryption session: the derived symmetric key plus the parameters needed to
/// reproduce it.
///
/// The expensive Argon2id derivation is run **once** (at wallet creation, unlock, or
/// passphrase change) and the resulting key is held here so the per-second save loop only
/// pays for the (cheap) AEAD encryption. The key is zeroized on drop.
#[derive(Clone)]
pub struct EncryptionSession {
    key: Zeroizing<[u8; KEY_LEN]>,
    salt: [u8; SALT_LEN],
    params: Argon2Params,
}

impl std::fmt::Debug for EncryptionSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("EncryptionSession")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl EncryptionSession {
    /// Derive a fresh session from a passphrase, generating a new random salt.
    ///
    /// Runs Argon2id; call this only when (re)keying, not on every save.
    pub fn new(
        passphrase: &SecretString,
        params: Argon2Params,
    ) -> Result<Self, WalletEncryptionError> {
        let mut salt = [0u8; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        Self::derive(passphrase, salt, params)
    }

    /// Derive a session reusing a known salt and parameters (used on the decrypt path so the
    /// reloaded wallet can keep re-encrypting with the same key).
    fn derive(
        passphrase: &SecretString,
        salt: [u8; SALT_LEN],
        params: Argon2Params,
    ) -> Result<Self, WalletEncryptionError> {
        let argon2 = params.to_argon2()?;
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        argon2
            .hash_password_into(
                passphrase.expose_secret().as_bytes(),
                &salt,
                key.as_mut_slice(),
            )
            .map_err(|e| WalletEncryptionError::KeyDerivation(e.to_string()))?;
        Ok(Self { key, salt, params })
    }

    /// Encrypt a plaintext wallet buffer, returning the complete on-disk envelope.
    ///
    /// A fresh random nonce is generated for every call, so it is safe to reuse the same
    /// session across many saves.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, WalletEncryptionError> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        header.push(ENVELOPE_VERSION);
        header.push(KDF_ARGON2ID);
        header.extend_from_slice(&self.params.m_cost.to_le_bytes());
        header.extend_from_slice(&self.params.t_cost.to_le_bytes());
        header.push(self.params.p_cost);
        header.extend_from_slice(&self.salt);
        header.extend_from_slice(&nonce_bytes);
        debug_assert_eq!(header.len(), HEADER_LEN);

        let cipher = XChaCha20Poly1305::new(self.key.as_slice().into());
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: &header,
                },
            )
            .map_err(|_| WalletEncryptionError::DecryptionFailed)?;

        let mut out = header;
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }
}

/// Returns `true` if `bytes` begins with the encrypted-wallet magic.
pub fn is_encrypted(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] == MAGIC
}

/// Decrypt a wallet envelope, returning the plaintext buffer **and** an [`EncryptionSession`]
/// the caller should cache so subsequent saves re-encrypt with the same key.
pub fn decrypt(
    passphrase: &SecretString,
    envelope: &[u8],
) -> Result<(Zeroizing<Vec<u8>>, EncryptionSession), WalletEncryptionError> {
    if envelope.len() < HEADER_LEN || !is_encrypted(envelope) {
        return Err(WalletEncryptionError::MalformedHeader);
    }

    let format_ver = envelope[8];
    if format_ver != ENVELOPE_VERSION {
        return Err(WalletEncryptionError::UnsupportedVersion(format_ver));
    }
    let kdf_id = envelope[9];
    if kdf_id != KDF_ARGON2ID {
        return Err(WalletEncryptionError::UnsupportedKdf(kdf_id));
    }

    let m_cost = u32::from_le_bytes(envelope[10..14].try_into().unwrap());
    let t_cost = u32::from_le_bytes(envelope[14..18].try_into().unwrap());
    let p_cost = envelope[18];
    let params = Argon2Params {
        m_cost,
        t_cost,
        p_cost,
    };

    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&envelope[19..19 + SALT_LEN]);
    let nonce = &envelope[19 + SALT_LEN..HEADER_LEN];

    let session = EncryptionSession::derive(passphrase, salt, params)?;

    let cipher = XChaCha20Poly1305::new(session.key.as_slice().into());
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: &envelope[HEADER_LEN..],
                aad: &envelope[..HEADER_LEN],
            },
        )
        .map_err(|_| WalletEncryptionError::DecryptionFailed)?;

    Ok((Zeroizing::new(plaintext), session))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fast params keep the test suite quick; production uses Argon2Params::desktop().
    fn fast_params() -> Argon2Params {
        Argon2Params {
            m_cost: 8,
            t_cost: 1,
            p_cost: 1,
        }
    }

    fn pw(s: &str) -> SecretString {
        SecretString::new(s.to_string())
    }

    #[test]
    fn round_trip() {
        let session = EncryptionSession::new(&pw("correct horse"), fast_params()).unwrap();
        let plaintext = b"super secret wallet bytes".to_vec();
        let envelope = session.encrypt(&plaintext).unwrap();

        assert!(is_encrypted(&envelope));
        let (recovered, _) = decrypt(&pw("correct horse"), &envelope).unwrap();
        assert_eq!(recovered.as_slice(), plaintext.as_slice());
    }

    #[test]
    fn decrypt_yields_reusable_session() {
        let session = EncryptionSession::new(&pw("pw"), fast_params()).unwrap();
        let envelope = session.encrypt(b"v1").unwrap();
        let (_, recovered_session) = decrypt(&pw("pw"), &envelope).unwrap();
        // The session recovered on load must round-trip new saves.
        let envelope2 = recovered_session.encrypt(b"v2").unwrap();
        let (pt2, _) = decrypt(&pw("pw"), &envelope2).unwrap();
        assert_eq!(pt2.as_slice(), b"v2");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let session = EncryptionSession::new(&pw("right"), fast_params()).unwrap();
        let envelope = session.encrypt(b"data").unwrap();
        let err = decrypt(&pw("wrong"), &envelope).unwrap_err();
        assert!(matches!(err, WalletEncryptionError::DecryptionFailed));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let session = EncryptionSession::new(&pw("pw"), fast_params()).unwrap();
        let mut envelope = session.encrypt(b"data").unwrap();
        let last = envelope.len() - 1;
        envelope[last] ^= 0xff;
        assert!(matches!(
            decrypt(&pw("pw"), &envelope),
            Err(WalletEncryptionError::DecryptionFailed)
        ));
    }

    #[test]
    fn tampered_header_fails() {
        let session = EncryptionSession::new(&pw("pw"), fast_params()).unwrap();
        let mut envelope = session.encrypt(b"data").unwrap();
        // Flip a salt byte (within the header, which is bound as AAD).
        envelope[20] ^= 0xff;
        assert!(decrypt(&pw("pw"), &envelope).is_err());
    }

    #[test]
    fn nonce_is_fresh_per_encryption() {
        let session = EncryptionSession::new(&pw("pw"), fast_params()).unwrap();
        let a = session.encrypt(b"same").unwrap();
        let b = session.encrypt(b"same").unwrap();
        // Identical plaintext + same key must still produce different ciphertext (fresh nonce).
        assert_ne!(a, b);
    }

    #[test]
    fn magic_does_not_collide_with_plaintext_version() {
        // The first 8 bytes of any plaintext wallet are a small little-endian u64 version.
        // The magic interpreted the same way must be astronomically large, so the two
        // namespaces never overlap.
        let magic_as_version = u64::from_le_bytes(MAGIC);
        assert!(magic_as_version > 1_000_000);
        // And a plaintext v39 prefix is not mistaken for an envelope.
        let plaintext_prefix = 39u64.to_le_bytes();
        assert!(!is_encrypted(&plaintext_prefix));
    }

    #[test]
    fn short_input_is_rejected() {
        assert!(matches!(
            decrypt(&pw("pw"), b"too short"),
            Err(WalletEncryptionError::MalformedHeader)
        ));
    }
}
