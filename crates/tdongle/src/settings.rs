//! Settings kept in the `settings` flash partition (see `partitions.csv`):
//! the WiFi network and password, set over the serial console, and the BLE
//! bonds made when pairing with lights.
//!
//! Each kind has a record in its own sector, so saving one never erases the
//! other: the WiFi record in sector 0, the bonds in sector 1, the automation
//! flag in sector 2, the schedule in sector 3. A record is a
//! magic value, the fields, and a CRC-32 over all of it. Saving erases the
//! sector and writes the record; both change rarely, so that's fine for wear.
//! A record cut short by a power loss fails the CRC and counts as nothing
//! stored.

use esp_bootloader_esp_idf::partitions::{
    Error, FlashStorage, PARTITION_TABLE_MAX_LEN, PartitionEntry, read_partition_table,
};
use heapless::{String, Vec};
use hue_ble::Bond;

use crate::schedule::{Entries, Entry, MAX_ENTRIES};

const LABEL: &str = "settings";
const MAGIC: &[u8; 4] = b"TDS1";
/// Record size read and written; enough for every field at its maximum.
const RECORD_LEN: usize = 128;
/// Sector of the partition holding the WiFi record.
const SETTINGS_SECTOR: u32 = 0;

const BONDS_MAGIC: &[u8; 4] = b"TDB1";
/// Most bonds kept.
pub const MAX_BONDS: usize = 8;
/// Bytes per bond: flags, address, LTK and IRK.
const BOND_LEN: usize = 1 + 6 + 16 + 16;
/// Magic, count, the bonds at their maximum, CRC; a multiple of 4 so the
/// flash can be written in whole words.
const BONDS_RECORD_LEN: usize = 324;
/// Sector of the partition holding the bonds.
const BONDS_SECTOR: u32 = 1;

const AUTOMATE_MAGIC: &[u8; 4] = b"TDA1";
/// Magic, the flag, CRC, padded to whole words.
const AUTOMATE_RECORD_LEN: usize = 12;
/// Sector of the partition holding the automation flag.
const AUTOMATE_SECTOR: u32 = 2;

const SCHEDULE_MAGIC: &[u8; 4] = b"TDC1";
/// Bytes per entry: minute of day (16 bits), dim (0 for off), forced.
const ENTRY_LEN: usize = 4;
/// Magic, count, the entries at their maximum, CRC, padded to whole words.
const SCHEDULE_RECORD_LEN: usize = 44;
/// Sector of the partition holding the automation schedule.
const SCHEDULE_SECTOR: u32 = 3;

const FLAG_RANDOM_ADDRESS: u8 = 1;
const FLAG_HAS_IRK: u8 = 2;
const FLAG_AUTHENTICATED: u8 = 4;

#[derive(Clone, Default)]
pub struct Settings {
    /// WiFi network name; empty if none is set.
    pub ssid: String<32>,
    /// WiFi password; empty for an open network.
    pub password: String<64>,
}

/// The store, shared by the serial console and the BLE task.
pub type Shared = embassy_sync::mutex::Mutex<embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex, Store>;

/// The settings partition.
pub struct Store {
    flash: FlashStorage<'static>,
    partition: PartitionEntry,
}

impl Store {
    /// Finds the settings partition. Fails if the flash was written with a
    /// partition table that doesn't have one.
    pub fn new(mut flash: FlashStorage<'static>) -> Result<Self, Error> {
        let mut table = [0u8; PARTITION_TABLE_MAX_LEN];
        let partition = read_partition_table(&mut flash, &mut table)?
            .iter()
            .find(|p| p.label_as_str() == LABEL)
            .ok_or(Error::Invalid)?;
        Ok(Self { flash, partition })
    }

    /// The stored settings, or empty ones if there are none.
    pub fn load(&mut self) -> Settings {
        let mut record = [0u8; RECORD_LEN];
        if self.read_sector(SETTINGS_SECTOR, &mut record).is_err() {
            return Settings::default();
        }
        decode(&record).unwrap_or_default()
    }

    /// Saves the settings, replacing the ones stored.
    pub fn save(&mut self, settings: &Settings) -> Result<(), Error> {
        self.write_sector(SETTINGS_SECTOR, &Aligned(encode(settings)))
    }

    /// The stored bonds, or none if there are none.
    pub fn load_bonds(&mut self) -> Vec<Bond, MAX_BONDS> {
        let mut record = [0u8; BONDS_RECORD_LEN];
        if self.read_sector(BONDS_SECTOR, &mut record).is_err() {
            return Vec::new();
        }
        decode_bonds(&record).unwrap_or_default()
    }

    /// Saves the bonds, replacing the ones stored.
    pub fn save_bonds(&mut self, bonds: &[Bond]) -> Result<(), Error> {
        let bonds = bonds.get(..MAX_BONDS).unwrap_or(bonds);
        self.write_sector(BONDS_SECTOR, &Aligned(encode_bonds(bonds)))
    }

    /// Whether automation is enabled; off if nothing is stored.
    pub fn load_automate(&mut self) -> bool {
        let mut record = [0u8; AUTOMATE_RECORD_LEN];
        if self.read_sector(AUTOMATE_SECTOR, &mut record).is_err() {
            return false;
        }
        decode_automate(&record).unwrap_or(false)
    }

    /// Saves whether automation is enabled.
    pub fn save_automate(&mut self, on: bool) -> Result<(), Error> {
        self.write_sector(AUTOMATE_SECTOR, &Aligned(encode_automate(on)))
    }

    /// The stored automation schedule, or an empty one if there is none.
    pub fn load_schedule(&mut self) -> Entries {
        let mut record = [0u8; SCHEDULE_RECORD_LEN];
        if self.read_sector(SCHEDULE_SECTOR, &mut record).is_err() {
            return Entries::new();
        }
        decode_schedule(&record).unwrap_or_default()
    }

    /// Saves the schedule, replacing the one stored.
    pub fn save_schedule(&mut self, entries: &[Entry]) -> Result<(), Error> {
        self.write_sector(SCHEDULE_SECTOR, &Aligned(encode_schedule(entries)))
    }

    fn read_sector(&mut self, sector: u32, buf: &mut [u8]) -> Result<(), Error> {
        let offset = sector * FlashStorage::SECTOR_SIZE;
        self.partition.as_flash_region(&mut self.flash).read(offset, buf)
    }

    /// Erases the sector and writes the record at its start.
    ///
    /// This runs its steps one by one through the low-level API, logging
    /// each.
    fn write_sector<const N: usize>(&mut self, sector: u32, record: &Aligned<N>) -> Result<(), Error> {
        let offset = self.partition.offset() + sector * FlashStorage::SECTOR_SIZE;
        let sector = offset / FlashStorage::SECTOR_SIZE;

        step(format_args!("unlocking flash"));
        // SAFETY: no preconditions.
        check(unsafe { esp_storage::ll::spiflash_unlock() })?;

        step(format_args!("erasing sector {sector} (flash 0x{offset:X})"));
        // SAFETY: the sector is inside the settings partition, so inside the
        // flash.
        check(unsafe { esp_storage::ll::spiflash_erase_sector(sector) })?;

        step(format_args!("writing {N} bytes"));
        // SAFETY: the record is word-aligned and inside the partition.
        check(unsafe { esp_storage::ll::spiflash_write(offset, record.0.as_ptr().cast(), N as u32) })?;

        step(format_args!("saved"));
        Ok(())
    }
}

/// Logs a step of saving, then pauses so the line gets out over USB before
/// the next flash operation, in case that one hangs.
fn step(what: core::fmt::Arguments) {
    log::info!("Settings: {what}");
    esp_hal::delay::Delay::new().delay_millis(20);
}

/// Logs the result of a low-level flash operation; non-zero is the ROM's
/// error code.
fn check(result: Result<(), i32>) -> Result<(), Error> {
    match result {
        Ok(()) => {
            step(format_args!("  ok"));
            Ok(())
        }
        Err(code) => {
            step(format_args!("  failed with ROM code {code}"));
            Err(Error::StorageError)
        }
    }
}

/// A record, aligned so the flash can be written straight from it.
#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

fn encode_schedule(entries: &[Entry]) -> [u8; SCHEDULE_RECORD_LEN] {
    let mut record = [0xFF; SCHEDULE_RECORD_LEN];
    record[..4].copy_from_slice(SCHEDULE_MAGIC);
    record[4] = entries.len() as u8;
    let mut pos = 5;
    for entry in entries {
        record[pos..][..2].copy_from_slice(&entry.minute.to_le_bytes());
        record[pos + 2] = entry.dim.unwrap_or(0);
        record[pos + 3] = u8::from(entry.forced);
        pos += ENTRY_LEN;
    }
    let crc = crc32(&record[..pos]);
    record[pos..][..4].copy_from_slice(&crc.to_le_bytes());
    record
}

fn decode_schedule(record: &[u8; SCHEDULE_RECORD_LEN]) -> Option<Entries> {
    if &record[..4] != SCHEDULE_MAGIC {
        return None;
    }
    let count = usize::from(record[4]);
    if count > MAX_ENTRIES {
        return None;
    }
    let end = 5 + count * ENTRY_LEN;
    let crc = u32::from_le_bytes(record[end..end + 4].try_into().ok()?);
    if crc != crc32(&record[..end]) {
        return None;
    }
    let mut entries = Entries::new();
    for bytes in record[5..end].chunks_exact(ENTRY_LEN) {
        let minute = u16::from_le_bytes([bytes[0], bytes[1]]);
        if minute >= 24 * 60 {
            return None;
        }
        entries
            .push(Entry { minute, dim: (bytes[2] != 0).then_some(bytes[2]), forced: bytes[3] != 0 })
            .ok()?;
    }
    Some(entries)
}

fn encode_automate(on: bool) -> [u8; AUTOMATE_RECORD_LEN] {
    let mut record = [0xFF; AUTOMATE_RECORD_LEN];
    record[..4].copy_from_slice(AUTOMATE_MAGIC);
    record[4] = u8::from(on);
    let crc = crc32(&record[..5]);
    record[5..9].copy_from_slice(&crc.to_le_bytes());
    record
}

fn decode_automate(record: &[u8; AUTOMATE_RECORD_LEN]) -> Option<bool> {
    if &record[..4] != AUTOMATE_MAGIC {
        return None;
    }
    let crc = u32::from_le_bytes(record[5..9].try_into().ok()?);
    (crc == crc32(&record[..5])).then_some(record[4] != 0)
}

fn encode_bonds(bonds: &[Bond]) -> [u8; BONDS_RECORD_LEN] {
    let mut record = [0xFF; BONDS_RECORD_LEN];
    record[..4].copy_from_slice(BONDS_MAGIC);
    record[4] = bonds.len() as u8;
    let mut pos = 5;
    for bond in bonds {
        let mut flags = 0;
        if bond.random_address {
            flags |= FLAG_RANDOM_ADDRESS;
        }
        if bond.irk.is_some() {
            flags |= FLAG_HAS_IRK;
        }
        if bond.authenticated {
            flags |= FLAG_AUTHENTICATED;
        }
        record[pos] = flags;
        record[pos + 1..][..6].copy_from_slice(&bond.address);
        record[pos + 7..][..16].copy_from_slice(&bond.ltk.to_le_bytes());
        record[pos + 23..][..16].copy_from_slice(&bond.irk.unwrap_or(0).to_le_bytes());
        pos += BOND_LEN;
    }
    let crc = crc32(&record[..pos]);
    record[pos..][..4].copy_from_slice(&crc.to_le_bytes());
    record
}

fn decode_bonds(record: &[u8; BONDS_RECORD_LEN]) -> Option<Vec<Bond, MAX_BONDS>> {
    if &record[..4] != BONDS_MAGIC {
        return None;
    }
    let count = usize::from(record[4]);
    if count > MAX_BONDS {
        return None;
    }
    let end = 5 + count * BOND_LEN;
    let crc = u32::from_le_bytes(record[end..end + 4].try_into().ok()?);
    if crc != crc32(&record[..end]) {
        return None;
    }
    let mut bonds = Vec::new();
    for entry in record[5..end].chunks_exact(BOND_LEN) {
        let flags = entry[0];
        let irk = u128::from_le_bytes(entry[23..39].try_into().ok()?);
        bonds
            .push(Bond {
                address: entry[1..7].try_into().ok()?,
                random_address: flags & FLAG_RANDOM_ADDRESS != 0,
                ltk: u128::from_le_bytes(entry[7..23].try_into().ok()?),
                irk: (flags & FLAG_HAS_IRK != 0).then_some(irk),
                authenticated: flags & FLAG_AUTHENTICATED != 0,
            })
            .ok()?;
    }
    Some(bonds)
}

fn encode(settings: &Settings) -> [u8; RECORD_LEN] {
    let mut record = [0xFF; RECORD_LEN];
    record[..4].copy_from_slice(MAGIC);
    let mut pos = 4;
    for field in [settings.ssid.as_bytes(), settings.password.as_bytes()] {
        record[pos] = field.len() as u8;
        record[pos + 1..][..field.len()].copy_from_slice(field);
        pos += 1 + field.len();
    }
    let crc = crc32(&record[..pos]);
    record[pos..][..4].copy_from_slice(&crc.to_le_bytes());
    record
}

fn decode(record: &[u8; RECORD_LEN]) -> Option<Settings> {
    if &record[..4] != MAGIC {
        return None;
    }
    let mut pos = 4;
    let ssid = String::try_from(field(record, &mut pos)?).ok()?;
    let password = String::try_from(field(record, &mut pos)?).ok()?;
    let crc = u32::from_le_bytes(record.get(pos..pos + 4)?.try_into().ok()?);
    (crc == crc32(&record[..pos])).then_some(Settings { ssid, password })
}

/// Reads the length-prefixed text field at `pos` and moves past it.
fn field<'a>(record: &'a [u8], pos: &mut usize) -> Option<&'a str> {
    let len = usize::from(*record.get(*pos)?);
    let bytes = record.get(*pos + 1..*pos + 1 + len)?;
    *pos += 1 + len;
    core::str::from_utf8(bytes).ok()
}

/// CRC-32 (IEEE), bit by bit: the record is tiny.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
