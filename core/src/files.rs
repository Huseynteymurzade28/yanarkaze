//! The file commands (`ls`, `cat`, `rm`, `mv`, `df`) and `>` / `>>` redirection, on ember.

use embedded_io_async::{Read, Write};

use crate::ember::{self, AreaFlash, Ember, FsError};
use crate::platform::System;
use crate::shell::Shell;

/// The file system on the board's file area.
pub(crate) fn ember<S: System>(sys: &mut S) -> Result<Ember<AreaFlash<'_, S>>, FsError> {
    AreaFlash::new(sys).map(Ember::new).ok_or(FsError::Unsupported)
}

/// Splits `echo hi > notes.txt` into the command and where its output goes: (`echo hi`,
/// `Some(("notes.txt", false))`); `>>` appends (`true`). Only a valid file name counts as a
/// target, so `calc 256 >> 2` stays a shift.
pub(crate) fn split_redirect(part: &str) -> (&str, Option<(&str, bool)>) {
    let Some(i) = part.rfind('>') else { return (part, None) };
    let target = part[i + 1..].trim();
    let append = part[..i].ends_with('>');
    let cmd = part[..if append { i - 1 } else { i }].trim_end();
    if !ember::valid_name(target) || cmd.is_empty() || cmd.ends_with('>') {
        return (part, None);
    }
    (cmd, Some((target, append)))
}

impl<S: System, R: Read, W: Write> Shell<S, R, W> {
    async fn fs_error(&mut self, e: FsError) {
        out!(self.term, "error: {e}\r\n");
    }

    /// Runs `cmd` with its output going into `file`.
    pub(crate) async fn run_redirected(&mut self, cmd: &str, file: &str, append: bool) {
        self.term.start_capture();
        self.run_part(cmd).await;
        let Some(capture) = self.term.end_capture() else { return };
        let result = match ember(&mut self.sys) {
            Ok(mut fs) if append => fs.append(file, &capture.text).await,
            Ok(mut fs) => fs.write(file, &capture.text).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) if capture.overflowed => {
                out!(self.term, "note: only the first {} bytes went to {file}\r\n", capture.text.len())
            }
            Ok(()) => {}
            Err(e) => self.fs_error(e).await,
        }
    }

    pub(crate) async fn cmd_ls(&mut self) {
        let files = match ember(&mut self.sys) {
            Ok(mut fs) => fs.list().await,
            Err(e) => Err(e),
        };
        match files {
            Ok(files) if files.is_empty() => self.term.print("no files yet. try: echo hello > hello.txt\r\n").await,
            Ok(files) => {
                let total: usize = files.iter().map(|f| f.size).sum();
                for f in &files {
                    out!(self.term, "  {:<24} {:>5}\r\n", f.name.as_str(), f.size);
                }
                out!(self.term, "{} file(s), {total} bytes\r\n", files.len());
            }
            Err(e) => self.fs_error(e).await,
        }
    }

    pub(crate) async fn cmd_cat<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let mut any = false;
        for name in names {
            any = true;
            let mut buf = [0u8; ember::MAX_SIZE];
            let read = match ember(&mut self.sys) {
                Ok(mut fs) => fs.read(name, &mut buf).await,
                Err(e) => Err(e),
            };
            match read {
                Ok(n) => {
                    // The terminal wants \r\n; files keep \n.
                    for line in buf[..n].split_inclusive(|&b| b == b'\n') {
                        match line.strip_suffix(b"\n") {
                            Some(text) => {
                                self.term.write_bytes(text).await;
                                self.term.print("\r\n").await;
                            }
                            None => {
                                self.term.write_bytes(line).await;
                                self.term.print("\r\n").await; // no newline at the end of the file
                            }
                        }
                    }
                }
                Err(e) => out!(self.term, "{name}: {e}\r\n"),
            }
        }
        if !any {
            self.term.print("usage: cat <file>...\r\n").await;
        }
    }

    pub(crate) async fn cmd_rm<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        let mut any = false;
        for name in names {
            any = true;
            let result = match ember(&mut self.sys) {
                Ok(mut fs) => fs.remove(name).await,
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                out!(self.term, "{name}: {e}\r\n");
            }
        }
        if !any {
            self.term.print("usage: rm <file>...\r\n").await;
        }
    }

    pub(crate) async fn cmd_mv<'a>(&mut self, mut args: impl Iterator<Item = &'a str>) {
        let (Some(from), Some(to)) = (args.next(), args.next()) else {
            self.term.print("usage: mv <file> <new name>\r\n").await;
            return;
        };
        let result = match ember(&mut self.sys) {
            Ok(mut fs) => fs.rename(from, to).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            self.fs_error(e).await;
        }
    }

    pub(crate) async fn cmd_df(&mut self) {
        let result = match ember(&mut self.sys) {
            Ok(mut fs) => fs.list().await.map(|files| (files, fs.capacity())),
            Err(e) => Err(e),
        };
        match result {
            Ok((files, capacity)) => {
                let used: usize = files.iter().map(|f| f.size).sum();
                out!(
                    self.term,
                    "ember: {} of {} files, {used} bytes, in {} KB of flash\r\n",
                    files.len(),
                    ember::MAX_FILES,
                    capacity / 1024
                );
                out!(
                    self.term,
                    "(files are up to {} KB each; old versions are cleared out as space is needed)\r\n",
                    ember::MAX_SIZE / 1024
                );
            }
            Err(e) => self.fs_error(e).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirections() {
        assert_eq!(split_redirect("echo hi > notes.txt"), ("echo hi", Some(("notes.txt", false))));
        assert_eq!(split_redirect("temp >> log.txt"), ("temp", Some(("log.txt", true))));
        assert_eq!(split_redirect("echo a>b"), ("echo a", Some(("b", false))));
        assert_eq!(split_redirect("calc 256 >> 2"), ("calc 256 >> 2", None));
        assert_eq!(split_redirect("echo hi"), ("echo hi", None));
        assert_eq!(split_redirect("> x"), ("> x", None));
        assert_eq!(split_redirect("echo >>> x"), ("echo >>> x", None));
    }
}
