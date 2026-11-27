//! Scripts: files of command lines, run with `run boot.ynk` or just `boot`.
//!
//! Each line is a command line as typed at the prompt (aliases, `;`, `>` and `>>` work), `#`
//! starts a comment line, and `$1`..`$9`, `$*` and `$0` are the script's arguments and name.
//! A script can run another one. That is done with an explicit stack of frames rather than
//! recursion (an async fn cannot call itself without a heap), each frame remembering the
//! file and where it is in it; the line comes from flash each time, so the whole nest needs
//! a single file buffer.

use core::fmt::Write as _;

use embedded_io_async::{Read, Write};
use heapless::String;

use crate::ember::{self, NAME_LEN};
use crate::files::{ember as open_ember, split_redirect};
use crate::platform::System;
use crate::settings;
use crate::shell::{Shell, split_word};

/// The file name ending of scripts; `boot` runs `boot.ynk`.
pub const EXTENSION: &str = ".ynk";
/// Scripts running scripts, at most this deep.
const DEPTH: usize = 4;
const LINE: usize = 256;

type Name = String<NAME_LEN>;
type Args = String<80>;

struct Frame {
    name: Name,
    args: Args,
    /// Offset of the next line in the file.
    next: usize,
    /// The rest of a line that ran a nested script, to finish when that one is done.
    pending: String<LINE>,
}

/// Replaces `$1`..`$9` with the arguments, `$*` with all of them, `$0` with the script's
/// name and `$$` with `$`. A missing argument becomes nothing.
pub(crate) fn substitute(line: &str, name: &str, args: &str) -> Option<String<LINE>> {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c).ok()?;
            continue;
        }
        match chars.peek().copied() {
            Some(d @ '1'..='9') => {
                chars.next();
                let n = d as usize - '1' as usize;
                out.push_str(args.split_whitespace().nth(n).unwrap_or("")).ok()?;
            }
            Some('*') => {
                chars.next();
                out.push_str(args.trim()).ok()?;
            }
            Some('0') => {
                chars.next();
                out.push_str(name).ok()?;
            }
            Some('$') => {
                chars.next();
                out.push('$').ok()?;
            }
            _ => out.push('$').ok()?,
        }
    }
    Some(out)
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    /// If `cmd` runs a script (`run x.ynk args`, or `x args` for an `x.ynk` that is not a
    /// command), the script's file name and arguments. A missing file is reported later, by
    /// `run_script`, so `run nope` still says why nothing happened.
    pub(crate) async fn script_of(&mut self, cmd: &str) -> Option<(Name, Args)> {
        let (first, rest) = split_word(cmd);
        if first == "run" {
            let (file, args) = split_word(rest);
            return Some((file.try_into().ok().unwrap_or_default(), args.try_into().ok().unwrap_or_default()));
        }
        if crate::shell::is_command(first) || !ember::valid_name(first) || first.len() + EXTENSION.len() > NAME_LEN {
            return None;
        }
        let mut file = Name::new();
        let _ = write!(file, "{first}{EXTENSION}");
        let exists = match open_ember(&mut self.sys) {
            Ok(mut fs) => fs.find(&file).await.ok().flatten().is_some(),
            Err(_) => false,
        };
        exists.then(|| (file, rest.try_into().ok().unwrap_or_default()))
    }

    /// Runs a script to the end, or until a key is pressed.
    pub(crate) async fn run_script(&mut self, name: &str, args: &str) {
        if name.is_empty() {
            self.term.print("usage: run <file> [args]   (or just the name of a .ynk file)\r\n").await;
            return;
        }
        // `run boot` finds boot.ynk when there is no file called just "boot".
        let mut file: Name = name.try_into().unwrap_or_default();
        if !name.contains('.') {
            let mut with_ext = Name::new();
            if write!(with_ext, "{name}{EXTENSION}").is_ok() {
                let (plain, ext) = match open_ember(&mut self.sys) {
                    Ok(mut fs) => (
                        fs.find(name).await.ok().flatten().is_some(),
                        fs.find(&with_ext).await.ok().flatten().is_some(),
                    ),
                    Err(_) => (false, false),
                };
                if !plain && ext {
                    file = with_ext;
                }
            }
        }
        let mut stack: heapless::Vec<Frame, DEPTH> = heapless::Vec::new();
        let _ = stack.push(Frame {
            name: file,
            args: args.try_into().unwrap_or_default(),
            next: 0,
            pending: String::new(),
        });
        let mut buf = [0u8; ember::MAX_SIZE];
        'frames: while let Some(top) = stack.last_mut() {
            let line: String<LINE> = if !top.pending.is_empty() {
                core::mem::take(&mut top.pending)
            } else {
                let read = match open_ember(&mut self.sys) {
                    Ok(mut fs) => fs.read(&top.name, &mut buf).await,
                    Err(e) => Err(e),
                };
                let n = match read {
                    Ok(n) => n,
                    Err(e) => {
                        out!(self.term, "{}: {e}\r\n", top.name.as_str());
                        stack.pop();
                        continue;
                    }
                };
                if top.next >= n {
                    stack.pop();
                    continue;
                }
                let end = buf[top.next..n].iter().position(|&b| b == b'\n').map_or(n, |i| top.next + i);
                let raw = core::str::from_utf8(&buf[top.next..end]).unwrap_or("").trim();
                top.next = end + 1;
                if raw.is_empty() || raw.starts_with('#') {
                    continue;
                }
                let Some(line) = substitute(raw, &top.name, &top.args) else {
                    out!(self.term, "{}: a line is too long after filling in $1..$9\r\n", top.name.as_str());
                    break;
                };
                match settings::expand(&line, &self.settings) {
                    Ok(expanded) => expanded.as_str().try_into().unwrap_or_default(),
                    Err(e) => {
                        out!(self.term, "{}: {e}\r\n", top.name.as_str());
                        continue;
                    }
                }
            };

            let mut offset = 0;
            for part in line.split(';') {
                offset += part.len() + 1;
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                let (cmd, redirect) = split_redirect(part);
                if let Some((script, script_args)) = self.script_of(cmd).await {
                    if redirect.is_some() {
                        self.term.print("note: > after a script inside a script is ignored\r\n").await;
                    }
                    if stack.is_full() {
                        out!(self.term, "scripts run each other more than {DEPTH} deep: stopped\r\n");
                        break 'frames;
                    }
                    let rest = line.get(offset.min(line.len())..).unwrap_or("");
                    if let Some(top) = stack.last_mut() {
                        top.pending = rest.try_into().unwrap_or_default();
                    }
                    let _ = stack.push(Frame { name: script, args: script_args, next: 0, pending: String::new() });
                    continue 'frames;
                }
                match redirect {
                    Some((file, append)) => self.run_redirected(cmd, file, append).await,
                    None => self.run_part(cmd).await,
                }
            }
            self.term.flush().await;
            // A key between lines stops the script (and the ones that ran it).
            if self.key_or_timeout(0).await {
                self.term.print("script stopped\r\n").await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_filled_in() {
        let s = |line: &str| substitute(line, "greet.ynk", "Ali  Veli").unwrap();
        assert_eq!(s("echo hello $1 and $2").as_str(), "echo hello Ali and Veli");
        assert_eq!(s("echo [$3]").as_str(), "echo []");
        assert_eq!(s("echo $* from $0").as_str(), "echo Ali  Veli from greet.ynk");
        assert_eq!(s("echo $$5 $x $").as_str(), "echo $5 $x $");
    }
}
