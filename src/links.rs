use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn key() -> Option<Vec<u8>> {
    std::env::var("CRABGATE_MASTER_KEY")
        .ok()
        .and_then(|value| {
            STANDARD_NO_PAD
                .decode(&value)
                .or_else(|_| STANDARD.decode(value))
                .ok()
        })
        .filter(|bytes| bytes.len() == 32)
}

pub fn sign_order(organizer_id: &str, order_id: &str, expires_at: i64) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(&key()?).ok()?;
    mac.update(format!("order:{organizer_id}:{order_id}:{expires_at}").as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_order(
    organizer_id: &str,
    order_id: &str,
    expires_at: i64,
    signature: &str,
    now: i64,
) -> bool {
    if expires_at <= now {
        return false;
    }
    let Some(key) = key() else {
        return false;
    };
    let Ok(bytes) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&key) else {
        return false;
    };
    mac.update(format!("order:{organizer_id}:{order_id}:{expires_at}").as_bytes());
    mac.verify_slice(&bytes).is_ok()
}
