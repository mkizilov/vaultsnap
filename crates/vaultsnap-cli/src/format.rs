//! Text formatting shared by the commands: sizes, UTC timestamps, permission strings, and file
//! names made safe to print.

use std::fmt::Write as _;
use std::path::Path;

use vaultsnap_core::history::EntryType;
use vaultsnap_core::scan::Timestamp;

/// A byte count in binary units with one decimal (`1.5 KiB`), or exact bytes below 1 KiB.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// [`human_size`] followed by the exact byte count when the two differ: `1.5 KiB (1536 bytes)`.
pub fn size_with_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        human_size(bytes)
    } else {
        format!("{} ({bytes} bytes)", human_size(bytes))
    }
}

/// Year, month, day for a day count since 1970-01-01 in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `YYYY-MM-DDTHH:MM:SS` for a time in UTC.
fn date_time(secs: i64) -> String {
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let in_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (in_day / 3600, in_day % 3600 / 60, in_day % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

/// A time in UTC to the second: `2025-06-15T15:06:40Z`. Always UTC, so output does not depend on
/// the machine's time zone.
pub fn time_seconds(time: Timestamp) -> String {
    format!("{}Z", date_time(time.secs))
}

/// A time in UTC with nanoseconds: `2025-06-15T15:06:40.000000005Z`. Fixed width, so the text
/// sorts like the time.
pub fn time_nanos(time: Timestamp) -> String {
    format!("{}.{:09}Z", date_time(time.secs), time.nanos)
}

/// Characters that do not show or that reorder the text around them, which a hostile file name
/// could use to disguise itself.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

/// Makes a file name or path safe to print on a terminal, one line, without changing its
/// meaning. Printable text is kept as it is. Everything else is written in a form that can be
/// read back exactly:
///
/// * `\\` for a backslash, `\n`, `\r`, `\t` for those characters;
/// * `\xNN` for any other ASCII control character, and for each byte that is not valid UTF-8;
/// * `\u{N}` for non-ASCII control characters and invisible or direction-changing characters.
///
/// So a name can neither inject terminal escape sequences nor span lines.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_ascii_control() => {
                    let _ = write!(out, "\\x{:02x}", u32::from(c));
                }
                c if c.is_control() || is_invisible(c) => {
                    let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
                }
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// A path made safe to print, like [`escape`].
pub fn escape_path(path: &Path) -> String {
    escape(&path_bytes(path))
}

/// The `ls -l` style permission string: a type letter and nine permission letters, with the
/// set-user-id, set-group-id, and sticky bits shown as `s`/`S` and `t`/`T`. `?` for permissions
/// the snapshot does not have.
pub fn mode_string(kind: EntryType, mode: Option<u32>) -> String {
    let letter = match kind {
        EntryType::Directory => 'd',
        EntryType::Symlink => 'l',
        EntryType::Fifo => 'p',
        EntryType::Socket => 's',
        EntryType::CharDevice => 'c',
        EntryType::BlockDevice => 'b',
        EntryType::File | EntryType::HardLink => '-',
        EntryType::Other => '?',
    };
    let mut out = String::with_capacity(10);
    out.push(letter);
    let Some(mode) = mode else {
        out.push_str("?????????");
        return out;
    };
    for (shift, special, with_exec, without_exec) in
        [(6_u32, 0o4000, 's', 'S'), (3, 0o2000, 's', 'S'), (0, 0o1000, 't', 'T')]
    {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(match (bits & 1 != 0, mode & special != 0) {
            (true, true) => with_exec,
            (false, true) => without_exec,
            (true, false) => 'x',
            (false, false) => '-',
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp { secs, nanos: 0 }
    }

    #[test]
    fn sizes_are_shown_in_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(human_size(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn exact_bytes_are_added_when_they_say_more() {
        assert_eq!(size_with_bytes(0), "0 B");
        assert_eq!(size_with_bytes(1000), "1000 B");
        assert_eq!(size_with_bytes(1536), "1.5 KiB (1536 bytes)");
    }

    #[test]
    fn times_are_utc_and_correct_on_known_dates() {
        let known = [
            (0, "1970-01-01T00:00:00Z"),
            (-1, "1969-12-31T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_000_000_000, "2001-09-09T01:46:40Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (1_750_000_000, "2025-06-15T15:06:40Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
            (-2_208_988_800, "1900-01-01T00:00:00Z"),
        ];
        for (secs, text) in known {
            assert_eq!(time_seconds(at(secs)), text, "{secs}");
        }
    }

    #[test]
    fn leap_days_and_year_ends_are_handled() {
        // 1900 was not a leap year, 2000 was, 2100 will not be.
        assert_eq!(time_seconds(at(-2_203_891_200)), "1900-03-01T00:00:00Z");
        assert_eq!(time_seconds(at(951_868_800)), "2000-03-01T00:00:00Z");
        assert_eq!(time_seconds(at(4_107_542_400)), "2100-03-01T00:00:00Z");
        assert_eq!(time_seconds(at(1_735_689_599)), "2024-12-31T23:59:59Z");
        assert_eq!(time_seconds(at(1_735_689_600)), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn nanoseconds_have_fixed_width() {
        let time = Timestamp { secs: 1_750_000_000, nanos: 5 };
        assert_eq!(time_nanos(time), "2025-06-15T15:06:40.000000005Z");
        let time = Timestamp { secs: 1_700_000_000, nanos: 123_456_789 };
        assert_eq!(time_nanos(time), "2023-11-14T22:13:20.123456789Z");
        assert_eq!(time_seconds(time), "2023-11-14T22:13:20Z", "cut, not rounded");
    }

    #[test]
    fn ordinary_names_are_left_alone() {
        let names = ["plain.txt", "with space", "ünïcödé 日本語", "emoji-😀", "-dash", "a/b/c"];
        for name in names.into_iter().chain(["'\"*?"]) {
            assert_eq!(escape(name.as_bytes()), name);
        }
    }

    #[test]
    fn names_that_could_confuse_a_terminal_are_escaped_reversibly() {
        assert_eq!(escape(b"new\nline"), "new\\nline");
        assert_eq!(escape(b"cr\rtab\t"), "cr\\rtab\\t");
        assert_eq!(escape(b"back\\slash"), "back\\\\slash");
        assert_eq!(escape(b"\x1b[31mred\x1b[0m"), "\\x1b[31mred\\x1b[0m");
        assert_eq!(escape(b"nul\x00del\x7f"), "nul\\x00del\\x7f");
        assert_eq!(escape(b"bad-\xff\xfe-bytes"), "bad-\\xff\\xfe-bytes");
        assert_eq!(escape("c1\u{85}".as_bytes()), "c1\\u{85}");
        assert_eq!(escape("rlo\u{202e}txt".as_bytes()), "rlo\\u{202e}txt");
        assert_eq!(escape("zero\u{200b}width\u{feff}".as_bytes()), "zero\\u{200b}width\\u{feff}");
    }

    #[test]
    fn paths_are_escaped_like_names() {
        assert_eq!(escape_path(Path::new("/out/data/plain.txt")), "/out/data/plain.txt");
        assert_eq!(escape_path(Path::new("/out/new\nline\x1b[2J")), "/out/new\\nline\\x1b[2J");
    }

    #[test]
    fn a_literal_backslash_sequence_is_distinguishable_from_an_escape() {
        assert_ne!(escape(b"\\n"), escape(b"\n"));
        assert_ne!(escape(b"\\xff"), escape(b"\xff"));
        assert_eq!(escape(b"\\xff"), "\\\\xff");
    }

    #[test]
    fn escaped_text_never_contains_a_control_or_invisible_character() {
        let mut bytes: Vec<u8> = (0..=255).collect();
        bytes.extend("\u{85}\u{202e}\u{2066}\u{200f}\u{2028}\u{feff}é日".as_bytes());
        let text = escape(&bytes);
        assert!(!text.chars().any(|c| c.is_control() || is_invisible(c)), "{text:?}");
        assert!(!text.contains('\n'));
    }

    #[test]
    fn permission_strings_follow_ls() {
        let file = |mode| mode_string(EntryType::File, Some(mode));
        assert_eq!(file(0o644), "-rw-r--r--");
        assert_eq!(file(0o755), "-rwxr-xr-x");
        assert_eq!(file(0o000), "----------");
        assert_eq!(file(0o4755), "-rwsr-xr-x");
        assert_eq!(file(0o4644), "-rwSr--r--");
        assert_eq!(file(0o2755), "-rwxr-sr-x");
        assert_eq!(file(0o1666), "-rw-rw-rwT");
        assert_eq!(mode_string(EntryType::Directory, Some(0o1777)), "drwxrwxrwt");
        assert_eq!(mode_string(EntryType::Symlink, Some(0o777)), "lrwxrwxrwx");
        assert_eq!(mode_string(EntryType::Fifo, None), "p?????????");
        assert_eq!(mode_string(EntryType::HardLink, Some(0o600)), "-rw-------");
    }
}
