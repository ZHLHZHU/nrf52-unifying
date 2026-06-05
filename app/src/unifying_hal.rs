//! Glue implementations of the `rust-unifying` hardware traits for the nRF52
//! app: a millisecond clock backed by `embassy-time` and an AES-128-CTR
//! encryptor backed by the `aes` + `ctr` crates.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use embassy_time::Instant;
use rust_unifying::constants::{AES_BLOCK_LEN, AES_DATA_LEN};
use rust_unifying::{AesEncryptor, Clock};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Millisecond clock built on the embassy monotonic timer.
pub struct EmbassyClock {
    start: Instant,
}

impl EmbassyClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for EmbassyClock {
    fn millis(&mut self) -> u32 {
        Instant::now()
            .duration_since(self.start)
            .as_millis() as u32
    }
}

/// AES-128 in counter mode, as required by the Unifying encrypted-keystroke
/// payloads. The 8 plaintext bytes are XORed with the AES-CTR keystream
/// generated from the per-keystroke IV.
#[derive(Default)]
pub struct SwAesCtr;

impl AesEncryptor for SwAesCtr {
    type Error = core::convert::Infallible;

    fn encrypt(
        &mut self,
        data: &mut [u8; AES_DATA_LEN],
        key: &[u8; AES_BLOCK_LEN],
        iv: &[u8; AES_BLOCK_LEN],
    ) -> Result<(), Self::Error> {
        let mut cipher = Aes128Ctr::new(key.into(), iv.into());
        cipher.apply_keystream(data);
        Ok(())
    }
}
