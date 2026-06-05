//! Persist the Unifying pairing profile (address, AES key, channel) and the
//! rolling AES counter into the `STORAGE` flash partition so pairing survives
//! resets and OTA updates.
//!
//! The AES counter is the security-critical field: the receiver rejects a
//! keystroke whose counter it has already seen, so after a reset we must resume
//! from a value strictly greater than anything used before. We therefore save
//! the counter after every typing burst.
//!
//! To avoid wearing out the flash (a NOR page tolerates a limited number of
//! erase cycles), records are *appended* into the first page of `STORAGE`:
//! each save writes a new 32-byte slot, and the page is only erased once all
//! 128 slots are used. On load we scan for the most recent valid slot.
//!
//! Record layout (32 bytes, 4-byte aligned):
//! ```text
//! [0..4]   magic  = 0x5546_4B31 ("UFK1")
//! [4..9]   address[5]
//! [9]      channel
//! [10..12] reserved (0)
//! [12..28] aes_key[16]
//! [28..32] aes_counter (u32 little-endian)
//! ```

use core::cell::RefCell;

use embassy_nrf::nvmc::Nvmc;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use rust_unifying::constants::{ADDRESS_LEN, AES_BLOCK_LEN};

/// Absolute base address of the `STORAGE` partition (see `app/memory.x`).
const STORAGE_BASE: u32 = 0x000F_8000;
/// NVMC erase granularity.
const PAGE_SIZE: u32 = 4096;
/// Fixed record size in bytes.
const RECORD_SIZE: usize = 32;
/// Records per page.
const SLOTS: u32 = PAGE_SIZE / RECORD_SIZE as u32;
/// Record validity marker. Erased flash reads as `0xFFFF_FFFF`, which is
/// distinct from this, so empty slots are easy to detect.
const MAGIC: u32 = 0x5546_4B31;

/// Shared flash handle, identical to the one the firmware updater borrows.
pub type SharedFlash<'d> = Mutex<NoopRawMutex, RefCell<Nvmc<'d>>>;

/// A persisted pairing profile.
#[derive(Clone, Copy)]
pub struct Profile {
    pub address: [u8; ADDRESS_LEN],
    pub aes_key: [u8; AES_BLOCK_LEN],
    pub aes_counter: u32,
    pub channel: u8,
}

fn encode(profile: &Profile) -> [u8; RECORD_SIZE] {
    let mut rec = [0u8; RECORD_SIZE];
    rec[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    rec[4..9].copy_from_slice(&profile.address);
    rec[9] = profile.channel;
    rec[12..28].copy_from_slice(&profile.aes_key);
    rec[28..32].copy_from_slice(&profile.aes_counter.to_le_bytes());
    rec
}

fn decode(rec: &[u8; RECORD_SIZE]) -> Option<Profile> {
    let magic = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
    if magic != MAGIC {
        return None;
    }
    let mut address = [0u8; ADDRESS_LEN];
    address.copy_from_slice(&rec[4..9]);
    let mut aes_key = [0u8; AES_BLOCK_LEN];
    aes_key.copy_from_slice(&rec[12..28]);
    let aes_counter = u32::from_le_bytes([rec[28], rec[29], rec[30], rec[31]]);
    Some(Profile {
        address,
        aes_key,
        aes_counter,
        channel: rec[9],
    })
}

/// Load the most recent valid profile, if any.
pub fn load(flash: &SharedFlash) -> Option<Profile> {
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        let mut latest = None;
        let mut rec = [0u8; RECORD_SIZE];
        for slot in 0..SLOTS {
            let offset = STORAGE_BASE + slot * RECORD_SIZE as u32;
            if nvmc.read(offset, &mut rec).is_err() {
                break;
            }
            match decode(&rec) {
                Some(profile) => latest = Some(profile),
                // First erased slot ends the sequential log.
                None => break,
            }
        }
        latest
    })
}

/// Append `profile` as a new record. Erases the page first if it is full.
pub fn save(flash: &SharedFlash, profile: &Profile) {
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();

        // Find the first erased slot (= one past the last written record).
        let mut next = SLOTS;
        let mut rec = [0u8; RECORD_SIZE];
        for slot in 0..SLOTS {
            let offset = STORAGE_BASE + slot * RECORD_SIZE as u32;
            if nvmc.read(offset, &mut rec).is_err() {
                return;
            }
            if decode(&rec).is_none() {
                next = slot;
                break;
            }
        }

        // Page full: wipe and restart at slot 0.
        if next >= SLOTS {
            if nvmc
                .erase(STORAGE_BASE, STORAGE_BASE + PAGE_SIZE)
                .is_err()
            {
                return;
            }
            next = 0;
        }

        let offset = STORAGE_BASE + next * RECORD_SIZE as u32;
        let _ = nvmc.write(offset, &encode(profile));
    });
}

/// Erase the STORAGE page, wiping all saved pairings.
pub fn clear(flash: &SharedFlash) {
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        let _ = nvmc.erase(STORAGE_BASE, STORAGE_BASE + PAGE_SIZE);
    });
}
