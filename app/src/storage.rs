//! Persist up to 4 Unifying pairing profiles and the active-slot selection into
//! the `STORAGE` flash partition so pairings survive resets and OTA updates.
//!
//! ## Flash layout (STORAGE = 0xF8000, 32K = 8 pages × 4K)
//!
//! | Page | Address     | Content |
//! |------|-------------|---------|
//! | 0    | 0xF8000     | Profile slot 0 (append-log, 128 records) |
//! | 1    | 0xF9000     | Profile slot 1 |
//! | 2    | 0xFA000     | Profile slot 2 |
//! | 3    | 0xFB000     | Profile slot 3 |
//! | 4    | 0xFC000     | Active-slot metadata (append-log, 1024 entries) |
//! | 5-7  | 0xFD000-    | Reserved |
//!
//! Each profile page uses the same append-style log as before: records are
//! appended one by one, and the page is only erased once all 128 slots are
//! full. This minimizes flash wear.
//!
//! The active-slot page stores 4-byte entries (just the active slot id as u32).
//! Erased flash reads 0xFFFFFFFF which is not a valid slot id (0..3), so empty
//! entries are easy to detect. 4K / 4 bytes = 1024 entries before needing an
//! erase — plenty for thousands of switches.
//!
//! ## Profile record format (32 bytes, 4-byte aligned)
//!
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
/// Fixed profile record size in bytes.
const RECORD_SIZE: usize = 32;
/// Profile records per page.
const SLOTS_PER_PAGE: u32 = PAGE_SIZE / RECORD_SIZE as u32; // 128
/// Maximum number of profile slots.
pub const MAX_PROFILES: u8 = 4;
/// Page used for active-slot metadata.
const ACTIVE_PAGE: u32 = STORAGE_BASE + PAGE_SIZE * MAX_PROFILES as u32; // page 4
/// Entries in the active-slot page (4 bytes each).
const ACTIVE_ENTRIES: u32 = PAGE_SIZE / 4;
/// Profile record validity marker.
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

fn page_base(slot: u8) -> u32 {
    STORAGE_BASE + PAGE_SIZE * slot as u32
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

/// Load the most recent valid profile from the given slot (0..MAX_PROFILES).
pub fn load(flash: &SharedFlash, slot: u8) -> Option<Profile> {
    if slot >= MAX_PROFILES {
        return None;
    }
    let base = page_base(slot);
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        let mut latest = None;
        let mut rec = [0u8; RECORD_SIZE];
        for i in 0..SLOTS_PER_PAGE {
            let offset = base + i * RECORD_SIZE as u32;
            if nvmc.read(offset, &mut rec).is_err() {
                break;
            }
            match decode(&rec) {
                Some(profile) => latest = Some(profile),
                None => break,
            }
        }
        latest
    })
}

/// Append `profile` into the given slot's page. Erases the page if full.
pub fn save(flash: &SharedFlash, slot: u8, profile: &Profile) {
    if slot >= MAX_PROFILES {
        return;
    }
    let base = page_base(slot);
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();

        let mut next = SLOTS_PER_PAGE;
        let mut rec = [0u8; RECORD_SIZE];
        for i in 0..SLOTS_PER_PAGE {
            let offset = base + i * RECORD_SIZE as u32;
            if nvmc.read(offset, &mut rec).is_err() {
                return;
            }
            if decode(&rec).is_none() {
                next = i;
                break;
            }
        }

        if next >= SLOTS_PER_PAGE {
            if nvmc.erase(base, base + PAGE_SIZE).is_err() {
                return;
            }
            next = 0;
        }

        let offset = base + next * RECORD_SIZE as u32;
        let _ = nvmc.write(offset, &encode(profile));
    });
}

/// Erase a specific profile slot.
pub fn clear_slot(flash: &SharedFlash, slot: u8) {
    if slot >= MAX_PROFILES {
        return;
    }
    let base = page_base(slot);
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        let _ = nvmc.erase(base, base + PAGE_SIZE);
    });
}

/// Erase all profile slots and the active-slot page.
pub fn clear_all(flash: &SharedFlash) {
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        // Erase pages 0..4 (profiles) + page 4 (active-slot).
        let end = ACTIVE_PAGE + PAGE_SIZE;
        let _ = nvmc.erase(STORAGE_BASE, end);
    });
}

/// Load the persisted active slot id (0..MAX_PROFILES), or None if unset.
pub fn load_active_slot(flash: &SharedFlash) -> Option<u8> {
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();
        let mut latest: Option<u8> = None;
        let mut buf = [0u8; 4];
        for i in 0..ACTIVE_ENTRIES {
            let offset = ACTIVE_PAGE + i * 4;
            if nvmc.read(offset, &mut buf).is_err() {
                break;
            }
            let val = u32::from_le_bytes(buf);
            if val < MAX_PROFILES as u32 {
                latest = Some(val as u8);
            } else {
                // Erased (0xFFFFFFFF) or invalid — end of log.
                break;
            }
        }
        latest
    })
}

/// Persist the active slot id. Appends to the active-slot page; erases if full.
pub fn save_active_slot(flash: &SharedFlash, slot: u8) {
    if slot >= MAX_PROFILES {
        return;
    }
    flash.lock(|cell| {
        let mut nvmc = cell.borrow_mut();

        let mut next = ACTIVE_ENTRIES;
        let mut buf = [0u8; 4];
        for i in 0..ACTIVE_ENTRIES {
            let offset = ACTIVE_PAGE + i * 4;
            if nvmc.read(offset, &mut buf).is_err() {
                return;
            }
            let val = u32::from_le_bytes(buf);
            if val >= MAX_PROFILES as u32 {
                // Erased or invalid — this is the next free entry.
                next = i;
                break;
            }
        }

        if next >= ACTIVE_ENTRIES {
            if nvmc.erase(ACTIVE_PAGE, ACTIVE_PAGE + PAGE_SIZE).is_err() {
                return;
            }
            next = 0;
        }

        let offset = ACTIVE_PAGE + next * 4;
        let _ = nvmc.write(offset, &(slot as u32).to_le_bytes());
    });
}
