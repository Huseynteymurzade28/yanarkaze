//! Settings that survive a reboot: aliases and the autorun command.
//!
//! The board stores an opaque blob ([`System::settings_store`]); everything about its
//! format lives here. The blob is a small header and plain text lines, so it is easy to
//! read in a memory dump and new kinds of lines can be added later:
//!
//! ```text
//! "YNKZ" | version (1) | length (u16 LE) | FNV-1a of the text (u32 LE) | text
//! alias hot alert 30; temp
//! autorun led heartbeat; oled dash
//! ```
//!
//! [`System::settings_store`]: crate::platform::System::settings_store

use heapless::{String, Vec};

pub const MAX_ALIASES: usize = 16;
pub const NAME_LEN: usize = 16;
pub const BODY_LEN: usize = 80;
/// The largest blob [`Settings::encode`] produces (all aliases and the autorun at full length).
pub const BLOB_MAX: usize = HEADER + MAX_ALIASES * (6 + NAME_LEN + 1 + BODY_LEN + 1) + 8 + BODY_LEN + 1 + 16 + 11;

const MAGIC: &[u8; 4] = b"YNKZ";
const VERSION: u8 = 1;
const HEADER: usize = 4 + 1 + 2 + 4;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub aliases: Vec<(String<NAME_LEN>, String<BODY_LEN>), MAX_ALIASES>,
    /// Run once at boot; empty for none.
    pub autorun: String<BODY_LEN>,
    /// Seconds between temperature log records; 0 when the log is off.
    pub log_secs: u32,
    /// The colour OLED is wired up (`oled color on`).
    pub oled_color: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsError {
    BadName,
    TooLong,
    Full,
    /// The name is a built-in command, which an alias would hide.
    Builtin,
}

impl core::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            SettingsError::BadName => "alias names are 1-16 letters, digits, '-' or '_'",
            SettingsError::TooLong => "too long (80 characters at most)",
            SettingsError::Full => "no room for more aliases (16 at most)",
            SettingsError::Builtin => "that is a built-in command",
        })
    }
}

fn fnv1a(data: &[u8]) -> u32 {
    data.iter().fold(0x811C_9DC5, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

impl Settings {
    pub fn alias(&self, name: &str) -> Option<&str> {
        self.aliases.iter().find(|(n, _)| n == name).map(|(_, body)| body.as_str())
    }

    /// Adds or replaces an alias. `builtin` says whether a name is a built-in command.
    pub fn set_alias(&mut self, name: &str, body: &str, builtin: impl Fn(&str) -> bool) -> Result<(), SettingsError> {
        let valid = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if name.is_empty() || name.len() > NAME_LEN || !name.chars().all(valid) {
            return Err(SettingsError::BadName);
        }
        if builtin(name) {
            return Err(SettingsError::Builtin);
        }
        let body: String<BODY_LEN> = body.trim().try_into().map_err(|_| SettingsError::TooLong)?;
        if let Some(slot) = self.aliases.iter_mut().find(|(n, _)| n == name) {
            slot.1 = body;
            return Ok(());
        }
        let name = name.try_into().map_err(|_| SettingsError::BadName)?;
        self.aliases.push((name, body)).map_err(|_| SettingsError::Full)
    }

    /// Removes an alias; false if there was none.
    pub fn remove_alias(&mut self, name: &str) -> bool {
        match self.aliases.iter().position(|(n, _)| n == name) {
            Some(i) => {
                self.aliases.remove(i);
                true
            }
            None => false,
        }
    }

    pub fn set_autorun(&mut self, cmd: &str) -> Result<(), SettingsError> {
        self.autorun = cmd.trim().try_into().map_err(|_| SettingsError::TooLong)?;
        Ok(())
    }

    /// Writes the blob into `out` (at least [`BLOB_MAX`] bytes); returns its length.
    pub fn encode(&self, out: &mut [u8]) -> usize {
        let mut len = HEADER;
        let mut put = |s: &str| {
            out[len..len + s.len()].copy_from_slice(s.as_bytes());
            len += s.len();
        };
        for (name, body) in &self.aliases {
            put("alias ");
            put(name);
            put(" ");
            put(body);
            put("\n");
        }
        if !self.autorun.is_empty() {
            put("autorun ");
            put(&self.autorun);
            put("\n");
        }
        if self.oled_color {
            put("oled color\n");
        }
        let mut log: String<16> = String::new();
        if self.log_secs > 0 && core::fmt::Write::write_fmt(&mut log, format_args!("log {}\n", self.log_secs)).is_ok() {
            put(&log);
        }
        let text_len = len - HEADER;
        out[0..4].copy_from_slice(MAGIC);
        out[4] = VERSION;
        out[5..7].copy_from_slice(&(text_len as u16).to_le_bytes());
        let checksum = fnv1a(&out[HEADER..len]);
        out[7..11].copy_from_slice(&checksum.to_le_bytes());
        len
    }

    /// Reads a blob. `None` for erased flash, another format or a damaged blob; lines
    /// this version does not understand are skipped.
    pub fn decode(blob: &[u8]) -> Option<Self> {
        if blob.len() < HEADER || &blob[0..4] != MAGIC || blob[4] != VERSION {
            return None;
        }
        let text_len = u16::from_le_bytes([blob[5], blob[6]]) as usize;
        let text = blob.get(HEADER..HEADER + text_len)?;
        if fnv1a(text) != u32::from_le_bytes([blob[7], blob[8], blob[9], blob[10]]) {
            return None;
        }
        let mut settings = Settings::default();
        for line in core::str::from_utf8(text).ok()?.lines() {
            let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
            match kind {
                "alias" => {
                    let (name, body) = rest.split_once(' ').unwrap_or((rest, ""));
                    let _ = settings.set_alias(name, body, |_| false);
                }
                "autorun" => {
                    let _ = settings.set_autorun(rest);
                }
                "log" => settings.log_secs = rest.parse().unwrap_or(0),
                "oled" => settings.oled_color = rest == "color",
                _ => {}
            }
        }
        Some(settings)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpandError {
    /// The expansion would not fit in a command line.
    TooLong,
    /// Aliases that use each other in a circle (or nest more than 8 deep).
    Loop,
    /// `repeat` or `every` would wrap an alias of several `;`-separated commands, but they
    /// repeat a single command.
    Compound,
}

impl core::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ExpandError::TooLong => "the line is too long once the aliases are filled in",
            ExpandError::Loop => "the aliases use each other in a circle",
            ExpandError::Compound => "repeat and every take one command, and that alias has several",
        })
    }
}

pub const LINE_LEN: usize = 256;

/// Replaces aliases in a command line: the first word of each `;`-separated command, and
/// the command `repeat` / `every` wrap. Arguments after an alias are appended to its
/// body, so `alias say echo hi` makes `say there` run `echo hi there`.
pub fn expand(line: &str, settings: &Settings) -> Result<String<LINE_LEN>, ExpandError> {
    let mut current: String<LINE_LEN> = line.try_into().map_err(|_| ExpandError::TooLong)?;
    for _ in 0..8 {
        let mut next: String<LINE_LEN> = String::new();
        let mut changed = false;
        for (i, part) in current.split(';').map(str::trim).filter(|p| !p.is_empty()).enumerate() {
            if i > 0 {
                next.push_str("; ").map_err(|_| ExpandError::TooLong)?;
            }
            let (first, rest) = split_word(part);
            // `repeat 3 x` and `every 2 x` keep their prefix and expand x.
            let (prefix, word, args) = if matches!(first, "repeat" | "every") {
                let (count, inner) = split_word(rest);
                let (word, args) = split_word(inner);
                (Some((first, count)), word, args)
            } else {
                (None, first, rest)
            };
            let body = settings.alias(word);
            if let Some((wrapper, count)) = prefix {
                // A wrapped command must stay a single command.
                if body.is_some_and(|b| b.contains(';')) {
                    return Err(ExpandError::Compound);
                }
                for s in [wrapper, " ", count, " "] {
                    next.push_str(s).map_err(|_| ExpandError::TooLong)?;
                }
            }
            match body {
                Some(body) => {
                    changed = true;
                    next.push_str(body).map_err(|_| ExpandError::TooLong)?;
                }
                None => next.push_str(word).map_err(|_| ExpandError::TooLong)?,
            }
            if !args.is_empty() {
                next.push(' ').map_err(|_| ExpandError::TooLong)?;
                next.push_str(args).map_err(|_| ExpandError::TooLong)?;
            }
        }
        if !changed {
            return Ok(current);
        }
        current = next;
    }
    Err(ExpandError::Loop)
}

fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    fn with(aliases: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (name, body) in aliases {
            s.set_alias(name, body, |_| false).unwrap();
        }
        s
    }

    #[test]
    fn round_trip() {
        let mut s = with(&[("blink", "repeat 5 led toggle"), ("hot", "alert 30; temp")]);
        s.set_autorun("led heartbeat; oled dash").unwrap();
        s.log_secs = 60;
        s.oled_color = true;
        let mut blob = [0u8; BLOB_MAX];
        let n = s.encode(&mut blob);
        assert_eq!(Settings::decode(&blob[..n]), Some(s.clone()));
        // Trailing bytes (the rest of a flash page) do not matter.
        let mut page = [0xFFu8; 2048];
        page[..n].copy_from_slice(&blob[..n]);
        assert_eq!(Settings::decode(&page), Some(s));
    }

    #[test]
    fn rejects_erased_damaged_and_foreign_blobs() {
        assert_eq!(Settings::decode(&[0xFF; 64]), None);
        assert_eq!(Settings::decode(&[]), None);
        let s = with(&[("x", "echo x")]);
        let mut blob = [0u8; BLOB_MAX];
        let n = s.encode(&mut blob);
        blob[HEADER + 3] ^= 1; // one flipped bit in the text
        assert_eq!(Settings::decode(&blob[..n]), None);
    }

    #[test]
    fn the_largest_blob_fits() {
        let mut s = Settings::default();
        let body = "x".repeat(BODY_LEN);
        for i in 0..MAX_ALIASES {
            let name = std::format!("alias-number-{i:03}");
            s.set_alias(&name[..NAME_LEN], &body, |_| false).unwrap();
        }
        s.set_autorun(&body).unwrap();
        let mut blob = [0u8; BLOB_MAX];
        let n = s.encode(&mut blob);
        assert!(n <= BLOB_MAX && BLOB_MAX <= 2048, "{n} / {BLOB_MAX}");
        assert_eq!(Settings::decode(&blob[..n]), Some(s));
    }

    #[test]
    fn alias_rules() {
        let mut s = Settings::default();
        let builtin = |n: &str| n == "led";
        assert_eq!(s.set_alias("led", "x", builtin), Err(SettingsError::Builtin));
        assert_eq!(s.set_alias("a b", "x", builtin), Err(SettingsError::BadName));
        assert_eq!(s.set_alias("", "x", builtin), Err(SettingsError::BadName));
        assert_eq!(s.set_alias("ok", &"y".repeat(81), builtin), Err(SettingsError::TooLong));
        s.set_alias("ok", "echo 1", builtin).unwrap();
        s.set_alias("ok", "echo 2", builtin).unwrap(); // replaces
        assert_eq!(s.aliases.len(), 1);
        assert_eq!(s.alias("ok"), Some("echo 2"));
        assert!(s.remove_alias("ok") && !s.remove_alias("ok"));
    }

    #[test]
    fn expansion() {
        let s = with(&[("hi", "echo hello"), ("both", "hi; temp"), ("t", "led toggle")]);
        let e = |line| expand(line, &s).map(|x| std::string::String::from(x.as_str()));
        assert_eq!(e("temp").unwrap(), "temp");
        assert_eq!(e("hi there").unwrap(), "echo hello there");
        assert_eq!(e("both; uptime").unwrap(), "echo hello; temp; uptime");
        assert_eq!(e("repeat 3 t").unwrap(), "repeat 3 led toggle");
        assert_eq!(e("every 0.5 hi you").unwrap(), "every 0.5 echo hello you");
        // `hi` as an argument is not expanded.
        assert_eq!(e("echo hi").unwrap(), "echo hi");
    }

    #[test]
    fn expansion_limits() {
        let s = with(&[("a", "b"), ("b", "a"), ("two", "echo 1; echo 2")]);
        assert_eq!(expand("a", &s), Err(ExpandError::Loop));
        assert_eq!(expand("repeat 2 two", &s), Err(ExpandError::Compound));
        let long = with(&[("x", &"y".repeat(80)), ("xx", "x; x; x; x")]);
        assert_eq!(expand("xx", &long), Err(ExpandError::TooLong));
    }
}
