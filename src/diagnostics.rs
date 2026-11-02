//! Bounded diagnostic logs. Task events and results keep their own retention rules.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub const DAEMON_BYTES: u64 = 4 * 1024 * 1024;
pub const PROTOCOL_BYTES: u64 = 16 * 1024 * 1024;

/// Keep one previous file, at most `limit` bytes, before appending a bounded record.
/// The caller serializes writers and opens `file` in append mode.
pub fn append(file: &mut File, path: &Path, text: &str, limit: u64) -> io::Result<()> {
    let marker = " [diagnostic line truncated]\n";
    let record = if text.len() as u64 > limit {
        let mut end = (limit as usize).saturating_sub(marker.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{marker}", &text[..end])
    } else {
        text.to_owned()
    };
    let len = file.metadata()?.len();
    if len > 0 && len.saturating_add(record.len() as u64) > limit {
        let mut input = BufReader::new(File::open(path)?);
        if len > limit {
            input.seek(SeekFrom::Start(len - limit))?;
            input.skip_until(b'\n')?;
        }
        let mut previous = File::create(path.with_extension("log.1"))?;
        io::copy(&mut input, &mut previous)?;
        previous.flush()?;
        // Preserve the current file if saving its predecessor fails.
        // Windows append handles cannot truncate; a separate write handle can.
        fs::OpenOptions::new().write(true).open(path)?.set_len(0)?;
    }
    file.write_all(record.as_bytes())
}

/// Replace a log with its retained lines using memory proportional to one line.
pub fn retain_lines(path: &Path, mut keep: impl FnMut(&[u8]) -> bool) -> io::Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let tmp = path.with_extension("trim.tmp");
    let result = (|| {
        let mut input = BufReader::new(file);
        let mut output = BufWriter::new(File::create(&tmp)?);
        let mut line = Vec::new();
        loop {
            line.clear();
            if input.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if keep(&line) {
                output.write_all(&line)?;
            }
        }
        output.flush()?;
        drop(output);
        drop(input);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_rotation_keeps_recent_data_and_bounds_large_utf8_records() {
        let dir = std::env::temp_dir().join(format!("axiopera-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("protocol.log");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let first = format!("{}\n", "a".repeat(80));
        append(&mut file, &path, &first, 128).unwrap();
        append(&mut file, &path, &first, 128).unwrap();
        assert_eq!(
            fs::read_to_string(path.with_extension("log.1")).unwrap(),
            first
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        append(&mut file, &path, &"测".repeat(200), 128).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.len() <= 128 && text.ends_with("[diagnostic line truncated]\n"));
        fs::remove_file(path.with_extension("log.1")).unwrap();
        fs::create_dir(path.with_extension("log.1")).unwrap();
        assert!(append(&mut file, &path, &first, 128).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        drop(file);
        fs::remove_dir_all(dir).unwrap();
    }
}
