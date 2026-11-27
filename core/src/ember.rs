//! ember: the file system. Flat (no directories), small files in the flash area a board
//! lends to [`Area::Files`].
//!
//! It sits on [`sequential_storage`]'s key-value map, which takes care of the hard parts:
//! wear levelling, CRCs, and surviving a reset in the middle of a write. Each file is a
//! directory entry (its name and size) plus its contents in 1 KB chunks, all map items:
//!
//! - key `0x01_ii_0000`: the entry of file slot `ii` (name length, name, size);
//! - key `0x02_ii_cccc`: chunk `cccc` of the contents of file slot `ii`.
//!
//! An empty value means "gone". The map could erase items instead, but only on flash that
//! can be written twice (the STM32L4 can not), so a removed file leaves a few small empty
//! items behind; slots are reused, which overwrites them.

use core::fmt;
use core::ops::Range;

use embedded_storage_async::nor_flash::{ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash};
use heapless::String;
use sequential_storage::cache::{Cache, Uncached};
use sequential_storage::map::{MapConfig, MapStorage};

use crate::platform::{Area, HwError, System};

/// How many files there can be.
pub const MAX_FILES: usize = 32;
/// The longest file name.
pub const NAME_LEN: usize = 24;
/// Contents are stored in pieces of this many bytes.
pub const CHUNK: usize = 1024;
const MAX_CHUNKS: usize = 8;
/// The largest file.
pub const MAX_SIZE: usize = CHUNK * MAX_CHUNKS;
/// The flash geometry ember expects: 2 KB pages, written 8 bytes at a time.
pub const PAGE: usize = 2048;
const WORD: usize = 8;
/// Room for a key (4 bytes) and the largest value, rounded up to the flash word.
const BUF: usize = (4 + CHUNK).div_ceil(WORD) * WORD + WORD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    /// Names are 1 to 24 letters, digits, `.`, `_` or `-`, starting with a letter or `_`.
    BadName,
    TooBig,
    /// The flash is full.
    Full,
    TooManyFiles,
    Flash,
    /// The board has no flash for files.
    Unsupported,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FsError::NotFound => "no such file",
            FsError::BadName => "bad file name (letters, digits, . _ -; start with a letter; at most 24)",
            FsError::TooBig => "file too big (8 KB at most)",
            FsError::Full => "no room left in flash",
            FsError::TooManyFiles => "too many files (32 at most)",
            FsError::Flash => "the flash did not answer",
            FsError::Unsupported => "this board has no flash for files",
        })
    }
}

impl<E> From<sequential_storage::Error<E>> for FsError {
    fn from(e: sequential_storage::Error<E>) -> Self {
        match e {
            sequential_storage::Error::FullStorage => FsError::Full,
            _ => FsError::Flash,
        }
    }
}

/// A file name, as ember accepts it. Starting with a letter keeps `calc 256 >> 2` a shift,
/// not a redirection into a file called `2`.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok
        && name.len() <= NAME_LEN
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A file, as `ls` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String<NAME_LEN>,
    pub size: usize,
    slot: u8,
}

fn entry_key(slot: u8) -> u32 {
    0x0100_0000 | (slot as u32) << 16
}

fn chunk_key(slot: u8, chunk: usize) -> u32 {
    0x0200_0000 | (slot as u32) << 16 | chunk as u32
}

fn encode_entry(name: &str, size: usize, out: &mut [u8; 1 + NAME_LEN + 4]) -> usize {
    out[0] = name.len() as u8;
    out[1..1 + name.len()].copy_from_slice(name.as_bytes());
    out[1 + name.len()..5 + name.len()].copy_from_slice(&(size as u32).to_le_bytes());
    5 + name.len()
}

fn decode_entry(slot: u8, value: &[u8]) -> Option<Entry> {
    let len = *value.first()? as usize;
    let name = core::str::from_utf8(value.get(1..1 + len)?).ok()?;
    let size = value.get(1 + len..5 + len)?;
    Some(Entry {
        name: name.try_into().ok()?,
        size: u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize,
        slot,
    })
}

type Map<'a, F> = MapStorage<u32, &'a mut F, Cache<Uncached, Uncached, Uncached, u32>>;

/// The file system on some flash. Every call reads what it needs from flash: nothing is
/// cached, so there is no state to get out of step after a reset.
pub struct Ember<F: NorFlash> {
    flash: F,
    range: Range<u32>,
}

impl<F: NorFlash> Ember<F> {
    pub fn new(flash: F) -> Self {
        let len = flash.capacity() as u32;
        Self { flash, range: 0..len }
    }

    fn map(&mut self) -> Result<Map<'_, F>, FsError> {
        let config = MapConfig::try_new(self.range.clone()).map_err(|_| FsError::Unsupported)?;
        Ok(MapStorage::new(&mut self.flash, config, Cache::new_uncached()))
    }

    /// Every file, in slot order.
    pub async fn list(&mut self) -> Result<heapless::Vec<Entry, MAX_FILES>, FsError> {
        let mut slots: [Option<Entry>; MAX_FILES] = core::array::from_fn(|_| None);
        let mut buf = [0u8; BUF];
        let mut map = self.map()?;
        let mut items = map.fetch_all_items(&mut buf).await?;
        let mut item_buf = [0u8; BUF];
        // The same key can come up more than once; the last one is the current value.
        while let Some((key, value)) = items.next::<&[u8]>(&mut item_buf).await? {
            if key >> 24 == 0x01 {
                let slot = ((key >> 16) & 0xFF) as u8;
                if let Some(s) = slots.get_mut(slot as usize) {
                    *s = decode_entry(slot, value);
                }
            }
        }
        Ok(slots.into_iter().flatten().collect())
    }

    pub async fn find(&mut self, name: &str) -> Result<Option<Entry>, FsError> {
        Ok(self.list().await?.into_iter().find(|e| e.name == name))
    }

    /// Reads a file into `out`; returns its size.
    pub async fn read(&mut self, name: &str, out: &mut [u8]) -> Result<usize, FsError> {
        let entry = self.find(name).await?.ok_or(FsError::NotFound)?;
        if entry.size > out.len() {
            return Err(FsError::TooBig);
        }
        let mut buf = [0u8; BUF];
        let mut map = self.map()?;
        for (i, piece) in out[..entry.size].chunks_mut(CHUNK).enumerate() {
            let value = map.fetch_item::<&[u8]>(&mut buf, &chunk_key(entry.slot, i)).await?.ok_or(FsError::Flash)?;
            if value.len() < piece.len() {
                return Err(FsError::Flash);
            }
            piece.copy_from_slice(&value[..piece.len()]);
        }
        Ok(entry.size)
    }

    /// Creates or replaces a file.
    pub async fn write(&mut self, name: &str, data: &[u8]) -> Result<(), FsError> {
        self.write_at(name, data, false).await
    }

    /// Adds to the end of a file, creating it if needed.
    pub async fn append(&mut self, name: &str, data: &[u8]) -> Result<(), FsError> {
        self.write_at(name, data, true).await
    }

    async fn write_at(&mut self, name: &str, data: &[u8], append: bool) -> Result<(), FsError> {
        if !valid_name(name) {
            return Err(FsError::BadName);
        }
        let files = self.list().await?;
        let existing = files.iter().find(|e| e.name == name).cloned();
        let slot = match &existing {
            Some(e) => e.slot,
            None => (0..MAX_FILES as u8).find(|s| files.iter().all(|e| e.slot != *s)).ok_or(FsError::TooManyFiles)?,
        };
        let old_size = existing.as_ref().map_or(0, |e| e.size);
        let start = if append { old_size } else { 0 };
        let new_size = start + data.len();
        if new_size > MAX_SIZE {
            return Err(FsError::TooBig);
        }

        let mut buf = [0u8; BUF];
        let mut chunk = [0u8; CHUNK];
        let mut map = self.map()?;
        // Rewrite the chunks from the one the new data starts in; when appending, that one
        // keeps its old beginning.
        let first = start / CHUNK;
        let mut written = 0;
        for i in first..new_size.div_ceil(CHUNK) {
            let chunk_start = i * CHUNK;
            let mut len = 0;
            if chunk_start < start {
                let keep = start - chunk_start;
                let old = map.fetch_item::<&[u8]>(&mut buf, &chunk_key(slot, i)).await?.ok_or(FsError::Flash)?;
                chunk[..keep].copy_from_slice(old.get(..keep).ok_or(FsError::Flash)?);
                len = keep;
            }
            let take = (CHUNK - len).min(data.len() - written);
            chunk[len..len + take].copy_from_slice(&data[written..written + take]);
            written += take;
            len += take;
            map.store_item(&mut buf, &chunk_key(slot, i), &&chunk[..len]).await?;
        }
        // Chunks the file no longer reaches: mark them gone, so the map can drop them.
        for i in new_size.div_ceil(CHUNK)..old_size.div_ceil(CHUNK) {
            map.store_item(&mut buf, &chunk_key(slot, i), &&[][..]).await?;
        }
        // The entry last: until it is written, the old size still holds.
        let mut entry = [0u8; 1 + NAME_LEN + 4];
        let n = encode_entry(name, new_size, &mut entry);
        map.store_item(&mut buf, &entry_key(slot), &&entry[..n]).await?;
        Ok(())
    }

    pub async fn remove(&mut self, name: &str) -> Result<(), FsError> {
        let entry = self.find(name).await?.ok_or(FsError::NotFound)?;
        let mut buf = [0u8; BUF];
        let mut map = self.map()?;
        map.store_item(&mut buf, &entry_key(entry.slot), &&[][..]).await?;
        for i in 0..entry.size.div_ceil(CHUNK) {
            map.store_item(&mut buf, &chunk_key(entry.slot, i), &&[][..]).await?;
        }
        Ok(())
    }

    /// Renames `from` to `to`, replacing `to` if it exists.
    pub async fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
        if !valid_name(to) {
            return Err(FsError::BadName);
        }
        let entry = self.find(from).await?.ok_or(FsError::NotFound)?;
        if from == to {
            return Ok(());
        }
        if self.find(to).await?.is_some() {
            self.remove(to).await?;
        }
        let mut buf = [0u8; BUF];
        let mut map = self.map()?;
        let mut value = [0u8; 1 + NAME_LEN + 4];
        let n = encode_entry(to, entry.size, &mut value);
        map.store_item(&mut buf, &entry_key(entry.slot), &&value[..n]).await?;
        Ok(())
    }

    /// Erases everything.
    pub async fn format(&mut self) -> Result<(), FsError> {
        let range = self.range.clone();
        self.flash.erase(range.start, range.end).await.map_err(|_| FsError::Flash)
    }

    /// The flash ember has, in bytes.
    pub fn capacity(&self) -> usize {
        self.range.len()
    }
}

/// The board's [`Area::Files`] as the flash ember runs on.
pub struct AreaFlash<'a, S: System> {
    sys: &'a mut S,
    pages: usize,
}

impl<'a, S: System> AreaFlash<'a, S> {
    /// `None` when the board has no file area, or one with pages of another size.
    pub fn new(sys: &'a mut S) -> Option<Self> {
        match sys.flash_area(Area::Files) {
            Some((pages, PAGE)) if pages >= 2 => Some(Self { sys, pages }),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct FlashError(pub HwError);

impl NorFlashError for FlashError {
    fn kind(&self) -> NorFlashErrorKind {
        match self.0 {
            HwError::OutOfRange => NorFlashErrorKind::OutOfBounds,
            _ => NorFlashErrorKind::Other,
        }
    }
}

impl<S: System> ErrorType for AreaFlash<'_, S> {
    type Error = FlashError;
}

impl<S: System> ReadNorFlash for AreaFlash<'_, S> {
    const READ_SIZE: usize = 1;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.sys.area_read(Area::Files, offset as usize, bytes).map_err(FlashError)
    }

    fn capacity(&self) -> usize {
        self.pages * PAGE
    }
}

impl<S: System> NorFlash for AreaFlash<'_, S> {
    const WRITE_SIZE: usize = WORD;
    const ERASE_SIZE: usize = PAGE;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        for page in from as usize / PAGE..to as usize / PAGE {
            self.sys.area_erase(Area::Files, page).map_err(FlashError)?;
        }
        Ok(())
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.sys.area_write(Area::Files, offset as usize, bytes).map_err(FlashError)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use embassy_futures::block_on;
    use std::vec::Vec;

    /// Flash like the STM32L4's: 2 KB pages, 8-byte words, each written once between erases.
    struct FakeFlash {
        bytes: Vec<u8>,
        written: Vec<bool>,
    }

    impl FakeFlash {
        fn new(pages: usize) -> Self {
            Self { bytes: std::vec![0xFF; pages * PAGE], written: std::vec![false; pages * PAGE / WORD] }
        }
    }

    impl ErrorType for FakeFlash {
        type Error = FlashError;
    }

    impl ReadNorFlash for FakeFlash {
        const READ_SIZE: usize = 1;
        async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), FlashError> {
            let o = offset as usize;
            bytes.copy_from_slice(&self.bytes[o..o + bytes.len()]);
            Ok(())
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
    }

    impl NorFlash for FakeFlash {
        const WRITE_SIZE: usize = WORD;
        const ERASE_SIZE: usize = PAGE;
        async fn erase(&mut self, from: u32, to: u32) -> Result<(), FlashError> {
            self.bytes[from as usize..to as usize].fill(0xFF);
            self.written[from as usize / WORD..to as usize / WORD].fill(false);
            Ok(())
        }
        async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), FlashError> {
            let o = offset as usize;
            assert!(o.is_multiple_of(WORD) && bytes.len().is_multiple_of(WORD), "unaligned write");
            for w in o / WORD..(o + bytes.len()) / WORD {
                assert!(!self.written[w], "word at {} written twice", w * WORD);
                self.written[w] = true;
            }
            self.bytes[o..o + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }

    fn names(fs: &mut Ember<FakeFlash>) -> Vec<(std::string::String, usize)> {
        block_on(fs.list()).unwrap().iter().map(|e| (e.name.as_str().into(), e.size)).collect()
    }

    fn contents(fs: &mut Ember<FakeFlash>, name: &str) -> Vec<u8> {
        let mut out = std::vec![0u8; MAX_SIZE];
        let n = block_on(fs.read(name, &mut out)).unwrap();
        out.truncate(n);
        out
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("notes.txt") && valid_name("_x") && valid_name("a-b_c.2"));
        assert!(!valid_name("") && !valid_name("2") && !valid_name("a b") && !valid_name("a/b"));
        assert!(!valid_name("a234567890123456789012345")); // 25 characters
    }

    #[test]
    fn write_read_list_remove() {
        let mut fs = Ember::new(FakeFlash::new(8));
        assert!(names(&mut fs).is_empty());
        block_on(fs.write("hello.txt", b"hi there\n")).unwrap();
        block_on(fs.write("empty", b"")).unwrap();
        assert_eq!(names(&mut fs), [("hello.txt".into(), 9), ("empty".into(), 0)]);
        assert_eq!(contents(&mut fs, "hello.txt"), b"hi there\n");
        assert_eq!(contents(&mut fs, "empty"), b"");
        block_on(fs.remove("hello.txt")).unwrap();
        assert_eq!(names(&mut fs), [("empty".into(), 0)]);
        assert_eq!(block_on(fs.remove("hello.txt")), Err(FsError::NotFound));
        assert_eq!(block_on(fs.write("bad name", b"x")), Err(FsError::BadName));
    }

    #[test]
    fn big_files_span_chunks_and_shrink() {
        let mut fs = Ember::new(FakeFlash::new(16));
        let big: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        block_on(fs.write("big", &big)).unwrap();
        assert_eq!(contents(&mut fs, "big"), big);
        block_on(fs.write("big", b"small now")).unwrap();
        assert_eq!(contents(&mut fs, "big"), b"small now");
        let too_big = std::vec![0u8; MAX_SIZE + 1];
        assert_eq!(block_on(fs.write("x", &too_big)), Err(FsError::TooBig));
    }

    #[test]
    fn append_across_a_chunk_boundary() {
        let mut fs = Ember::new(FakeFlash::new(16));
        let mut expected = Vec::new();
        for i in 0..60 {
            let line = std::format!("line {i:02} of the log, padded to a fair length\n");
            block_on(fs.append("log.txt", line.as_bytes())).unwrap();
            expected.extend_from_slice(line.as_bytes());
        }
        assert!(expected.len() > 2 * CHUNK);
        assert_eq!(contents(&mut fs, "log.txt"), expected);
    }

    #[test]
    fn rename_replaces_the_target() {
        let mut fs = Ember::new(FakeFlash::new(8));
        block_on(fs.write("a", b"A")).unwrap();
        block_on(fs.write("b", b"B")).unwrap();
        block_on(fs.rename("a", "b")).unwrap();
        assert_eq!(names(&mut fs), [("b".into(), 1)]);
        assert_eq!(contents(&mut fs, "b"), b"A");
        assert_eq!(block_on(fs.rename("nope", "c")), Err(FsError::NotFound));
    }

    #[test]
    fn rewriting_many_times_reuses_the_flash() {
        // Far more data over time than the flash holds: the map must reclaim old values.
        let mut fs = Ember::new(FakeFlash::new(8)); // 16 KB
        let data = std::vec![7u8; 1500];
        for _ in 0..100 {
            block_on(fs.write("same", &data)).unwrap();
        }
        assert_eq!(contents(&mut fs, "same"), data);
        // Slots are reused after a remove.
        for i in 0..200 {
            let name = std::format!("f{}", i % 5);
            block_on(fs.write(&name, b"x")).unwrap();
            block_on(fs.remove(&name)).unwrap();
        }
        assert_eq!(names(&mut fs), [("same".into(), 1500)]);
    }

    #[test]
    fn too_many_files() {
        let mut fs = Ember::new(FakeFlash::new(16));
        for i in 0..MAX_FILES {
            block_on(fs.write(&std::format!("f{i}"), b"")).unwrap();
        }
        assert_eq!(block_on(fs.write("one_more", b"")), Err(FsError::TooManyFiles));
    }

    #[test]
    fn survives_a_remount() {
        let mut fs = Ember::new(FakeFlash::new(8));
        block_on(fs.write("keep", b"still here")).unwrap();
        let flash = fs.flash;
        let mut fs = Ember::new(flash);
        assert_eq!(contents(&mut fs, "keep"), b"still here");
    }
}
