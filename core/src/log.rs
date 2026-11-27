//! The temperature log: fixed-size records in a ring of flash pages the board lends us.
//!
//! Flash can only be erased a page at a time and written once in between, so the log is a
//! ring: records go into the next free slot, and entering a page erases it first, dropping
//! that page's oldest records. Every record carries a sequence number, so after a reset the
//! newest one (the highest number) tells where to carry on. Nothing else has to be kept.

use crate::env::crc8;
use crate::platform::{HwError, System};

/// Bytes per record: one 64-bit flash word on the STM32L4, the smallest it can write.
pub const RECORD: usize = 8;

/// One logged reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Record {
    /// Counts up across resets; tells the newest record.
    pub seq: u32,
    /// Hundredths of a degree Celsius.
    pub centi_c: i16,
    /// The first record after a boot: the time since the previous one is unknown.
    pub boot: bool,
    /// Read from a room sensor rather than the chip's own thermometer.
    pub room: bool,
}

impl Record {
    pub fn temp_c(&self) -> f32 {
        self.centi_c as f32 / 100.0
    }

    pub fn encode(&self) -> [u8; RECORD] {
        let mut b = [0u8; RECORD];
        b[..4].copy_from_slice(&self.seq.to_le_bytes());
        b[4..6].copy_from_slice(&self.centi_c.to_le_bytes());
        b[6] = self.boot as u8 | (self.room as u8) << 1;
        b[7] = crc8(&b[..7]);
        b
    }

    /// `None` for an erased slot or a damaged record (a write cut short by a reset).
    pub fn decode(b: &[u8; RECORD]) -> Option<Self> {
        if b[7] != crc8(&b[..7]) || b.iter().all(|&x| x == 0xFF) {
            return None;
        }
        Some(Self {
            seq: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            centi_c: i16::from_le_bytes([b[4], b[5]]),
            boot: b[6] & 1 != 0,
            room: b[6] & 2 != 0,
        })
    }
}

/// What the log needs from flash. [`SysFlash`] provides it on top of a [`System`].
pub trait LogFlash {
    /// Number of pages and bytes per page; `None` when the board has no room for a log.
    fn geometry(&self) -> Option<(usize, usize)>;
    fn read(&mut self, offset: usize, buf: &mut [u8; RECORD]) -> Result<(), HwError>;
    fn write(&mut self, offset: usize, record: &[u8; RECORD]) -> Result<(), HwError>;
    fn erase(&mut self, page: usize) -> Result<(), HwError>;
}

pub struct SysFlash<'a, S: System>(pub &'a mut S);

impl<S: System> LogFlash for SysFlash<'_, S> {
    fn geometry(&self) -> Option<(usize, usize)> {
        self.0.log_geometry()
    }
    fn read(&mut self, offset: usize, buf: &mut [u8; RECORD]) -> Result<(), HwError> {
        self.0.log_read(offset, buf)
    }
    fn write(&mut self, offset: usize, record: &[u8; RECORD]) -> Result<(), HwError> {
        self.0.log_write(offset, record)
    }
    fn erase(&mut self, page: usize) -> Result<(), HwError> {
        self.0.log_erase(page)
    }
}

/// Where the log stands, found by [`Log::mount`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Log {
    /// Slot the next record goes to.
    next_slot: usize,
    next_seq: u32,
    /// Records stored.
    pub count: usize,
    /// Slots in all.
    pub capacity: usize,
    per_page: usize,
}

impl Log {
    /// Scans the flash for the newest record.
    pub fn mount(flash: &mut impl LogFlash) -> Result<Self, HwError> {
        let (pages, page_size) = flash.geometry().ok_or(HwError::Unsupported)?;
        let per_page = page_size / RECORD;
        let capacity = pages * per_page;
        let mut log = Self { capacity, per_page, ..Self::default() };
        let mut newest: Option<(usize, u32)> = None;
        for slot in 0..capacity {
            if let Some(r) = read_slot(flash, slot)? {
                log.count += 1;
                if newest.is_none_or(|(_, seq)| r.seq > seq) {
                    newest = Some((slot, r.seq));
                }
            }
        }
        if let Some((slot, seq)) = newest {
            log.next_slot = (slot + 1) % capacity;
            log.next_seq = seq.wrapping_add(1);
        }
        Ok(log)
    }

    /// Stores a reading. `boot` marks the first one since a reset.
    pub fn append(&mut self, flash: &mut impl LogFlash, centi_c: i16, boot: bool, room: bool) -> Result<(), HwError> {
        let (_, page_size) = flash.geometry().ok_or(HwError::Unsupported)?;
        let slot = self.next_slot;
        if slot.is_multiple_of(self.per_page) {
            // Entering a page: it still holds the oldest records, which go now.
            let page = slot / self.per_page;
            for s in slot..slot + self.per_page {
                if read_slot(flash, s)?.is_some() {
                    self.count -= 1;
                }
            }
            flash.erase(page)?;
        }
        let record = Record { seq: self.next_seq, centi_c, boot, room };
        flash.write(slot / self.per_page * page_size + slot % self.per_page * RECORD, &record.encode())?;
        self.count += 1;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_slot = (slot + 1) % self.capacity;
        Ok(())
    }

    /// Erases every page.
    pub fn clear(&mut self, flash: &mut impl LogFlash) -> Result<(), HwError> {
        for page in 0..self.capacity / self.per_page.max(1) {
            flash.erase(page)?;
        }
        *self = Self { capacity: self.capacity, per_page: self.per_page, ..Self::default() };
        Ok(())
    }

    /// Calls `f` with every record, oldest first.
    pub fn for_each(&self, flash: &mut impl LogFlash, mut f: impl FnMut(Record)) -> Result<(), HwError> {
        for i in 0..self.capacity {
            if let Some(r) = read_slot(flash, (self.next_slot + i) % self.capacity)? {
                f(r);
            }
        }
        Ok(())
    }
}

fn read_slot(flash: &mut impl LogFlash, slot: usize) -> Result<Option<Record>, HwError> {
    let (_, page_size) = flash.geometry().ok_or(HwError::Unsupported)?;
    let per_page = page_size / RECORD;
    let mut b = [0u8; RECORD];
    flash.read(slot / per_page * page_size + slot % per_page * RECORD, &mut b)?;
    Ok(Record::decode(&b))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    /// Flash that behaves like the real thing: erased to 0xFF, and a write can only clear bits.
    struct FakeFlash {
        pages: usize,
        page_size: usize,
        bytes: Vec<u8>,
    }

    impl FakeFlash {
        fn new(pages: usize, page_size: usize) -> Self {
            Self { pages, page_size, bytes: std::vec![0xFF; pages * page_size] }
        }
    }

    impl LogFlash for FakeFlash {
        fn geometry(&self) -> Option<(usize, usize)> {
            Some((self.pages, self.page_size))
        }
        fn read(&mut self, offset: usize, buf: &mut [u8; RECORD]) -> Result<(), HwError> {
            buf.copy_from_slice(&self.bytes[offset..offset + RECORD]);
            Ok(())
        }
        fn write(&mut self, offset: usize, record: &[u8; RECORD]) -> Result<(), HwError> {
            let word = &mut self.bytes[offset..offset + RECORD];
            assert!(word.iter().all(|&b| b == 0xFF), "write to unerased flash at {offset}");
            word.copy_from_slice(record);
            Ok(())
        }
        fn erase(&mut self, page: usize) -> Result<(), HwError> {
            self.bytes[page * self.page_size..(page + 1) * self.page_size].fill(0xFF);
            Ok(())
        }
    }

    fn temps(log: &Log, flash: &mut FakeFlash) -> Vec<i16> {
        let mut v = Vec::new();
        log.for_each(flash, |r| v.push(r.centi_c)).unwrap();
        v
    }

    #[test]
    fn records_round_trip_and_damage_is_caught() {
        let r = Record { seq: 70000, centi_c: -1234, boot: true, room: false };
        let mut b = r.encode();
        assert_eq!(Record::decode(&b), Some(r));
        b[4] ^= 1;
        assert_eq!(Record::decode(&b), None);
        assert_eq!(Record::decode(&[0xFF; RECORD]), None);
    }

    #[test]
    fn appends_survive_a_remount() {
        let mut flash = FakeFlash::new(4, 64); // 8 records a page, 32 in all
        let mut log = Log::mount(&mut flash).unwrap();
        assert_eq!((log.count, log.capacity), (0, 32));
        for t in 0..5 {
            log.append(&mut flash, t, t == 0, false).unwrap();
        }
        // A reset: the log carries on after the newest record.
        let mut log = Log::mount(&mut flash).unwrap();
        assert_eq!(log.count, 5);
        log.append(&mut flash, 5, true, false).unwrap();
        assert_eq!(temps(&log, &mut flash), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_full_ring_drops_the_oldest_page() {
        let mut flash = FakeFlash::new(4, 64);
        let mut log = Log::mount(&mut flash).unwrap();
        for t in 0..32 {
            log.append(&mut flash, t, false, false).unwrap();
        }
        assert_eq!(log.count, 32);
        // The 33rd record goes to slot 0: page 0 (records 0-7) is erased first.
        log.append(&mut flash, 32, false, false).unwrap();
        assert_eq!(log.count, 25);
        let t = temps(&log, &mut flash);
        assert_eq!((t[0], *t.last().unwrap(), t.len()), (8, 32, 25));
        // And the order still holds after a remount mid-page, many laps later.
        for t in 33..100 {
            log.append(&mut flash, t, false, false).unwrap();
        }
        let log = Log::mount(&mut flash).unwrap();
        let t = temps(&log, &mut flash);
        assert_eq!(*t.last().unwrap(), 99);
        assert!(t.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn clear_empties_it() {
        let mut flash = FakeFlash::new(2, 64);
        let mut log = Log::mount(&mut flash).unwrap();
        log.append(&mut flash, 1, false, false).unwrap();
        log.clear(&mut flash).unwrap();
        assert_eq!(Log::mount(&mut flash).unwrap().count, 0);
        log.append(&mut flash, 2, false, false).unwrap();
        assert_eq!(temps(&log, &mut flash), [2]);
    }
}
