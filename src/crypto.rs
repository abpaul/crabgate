use aes_gcm::{
    aead::{rand_core::RngCore, Aead, AeadCore, OsRng, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine,
};
use zeroize::Zeroizing;

const KEY_SIZE: usize = 32;
const NONCE_SIZE: usize = 12;
const VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("the master key must be 32 base64-encoded bytes")]
    InvalidMasterKey,
    #[error("invalid encrypted data")]
    InvalidData,
}

pub struct MasterKey(Zeroizing<[u8; KEY_SIZE]>);

pub struct OrganizerCipher {
    organizer_id: String,
    key: Zeroizing<[u8; KEY_SIZE]>,
}

impl MasterKey {
    pub fn from_base64(encoded: &str) -> Result<Self, CryptoError> {
        let bytes = STANDARD_NO_PAD
            .decode(encoded)
            .or_else(|_| STANDARD.decode(encoded))
            .map_err(|_| CryptoError::InvalidMasterKey)?;
        let key: [u8; KEY_SIZE] = bytes
            .try_into()
            .map_err(|_| CryptoError::InvalidMasterKey)?;
        Ok(Self(Zeroizing::new(key)))
    }

    pub fn generate_organizer_key(
        &self,
        organizer_id: &str,
    ) -> Result<(String, OrganizerCipher), CryptoError> {
        let mut bytes = Zeroizing::new([0_u8; KEY_SIZE]);
        OsRng.fill_bytes(&mut bytes[..]);
        let wrapped = seal(&self.0, organizer_id, "data_key", &bytes[..])?;
        Ok((
            wrapped,
            OrganizerCipher {
                organizer_id: organizer_id.to_owned(),
                key: bytes,
            },
        ))
    }

    pub fn open_organizer_key(
        &self,
        organizer_id: &str,
        wrapped: &str,
    ) -> Result<OrganizerCipher, CryptoError> {
        let bytes = Zeroizing::new(open(&self.0, organizer_id, "data_key", wrapped)?);
        let key: [u8; KEY_SIZE] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::InvalidData)?;
        Ok(OrganizerCipher {
            organizer_id: organizer_id.to_owned(),
            key: Zeroizing::new(key),
        })
    }
}

impl OrganizerCipher {
    pub fn encrypt(&self, column: &str, plaintext: &str) -> Result<String, CryptoError> {
        seal(&self.key, &self.organizer_id, column, plaintext.as_bytes())
    }

    pub fn decrypt(&self, column: &str, encoded: &str) -> Result<String, CryptoError> {
        let bytes = open(&self.key, &self.organizer_id, column, encoded)?;
        String::from_utf8(bytes).map_err(|_| CryptoError::InvalidData)
    }
}

fn seal(
    key: &[u8; KEY_SIZE],
    organizer_id: &str,
    column: &str,
    plaintext: &[u8],
) -> Result<String, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::InvalidData)?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let aad = associated_data(organizer_id, column);
    let encrypted = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| CryptoError::InvalidData)?;
    let mut result = Vec::with_capacity(1 + NONCE_SIZE + encrypted.len());
    result.push(VERSION);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&encrypted);
    Ok(STANDARD_NO_PAD.encode(result))
}

fn open(
    key: &[u8; KEY_SIZE],
    organizer_id: &str,
    column: &str,
    encoded: &str,
) -> Result<Vec<u8>, CryptoError> {
    let bytes = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| CryptoError::InvalidData)?;
    if bytes.len() < 1 + NONCE_SIZE + 16 || bytes[0] != VERSION {
        return Err(CryptoError::InvalidData);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::InvalidData)?;
    let mut nonce_bytes = [0_u8; NONCE_SIZE];
    nonce_bytes.copy_from_slice(&bytes[1..1 + NONCE_SIZE]);
    let nonce = Nonce::<aes_gcm::aead::consts::U12>::from(nonce_bytes);
    let aad = associated_data(organizer_id, column);
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &bytes[1 + NONCE_SIZE..],
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| CryptoError::InvalidData)
}

fn associated_data(organizer_id: &str, column: &str) -> String {
    format!("{organizer_id}:{column}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> MasterKey {
        MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap()
    }

    #[test]
    fn encrypts_each_field_for_one_organizer() {
        let (wrapped, cipher) = master().generate_organizer_key("org-1").unwrap();
        let encrypted = cipher.encrypt("buyer_email", "buyer@example.com").unwrap();
        assert!(!encrypted.contains("buyer@example.com"));
        assert_eq!(
            cipher.decrypt("buyer_email", &encrypted).unwrap(),
            "buyer@example.com"
        );
        assert!(cipher.decrypt("attendee_name", &encrypted).is_err());
        assert!(master().open_organizer_key("org-2", &wrapped).is_err());
        assert_eq!(
            master()
                .open_organizer_key("org-1", &wrapped)
                .unwrap()
                .decrypt("buyer_email", &encrypted)
                .unwrap(),
            "buyer@example.com"
        );
    }

    #[test]
    fn rejects_tampering_and_wrong_master_key() {
        let (wrapped, cipher) = master().generate_organizer_key("org-1").unwrap();
        let encrypted = cipher.encrypt("buyer_email", "buyer@example.com").unwrap();
        let mut bytes = STANDARD_NO_PAD.decode(encrypted).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        assert!(cipher
            .decrypt("buyer_email", &STANDARD_NO_PAD.encode(bytes))
            .is_err());
        let other = MasterKey::from_base64("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE").unwrap();
        assert!(other.open_organizer_key("org-1", &wrapped).is_err());
        assert!(MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").is_ok());
    }
}
