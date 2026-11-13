use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("path escapes the project root: {0}")]
    Escape(String),
    #[error("invalid stored path: {0}")]
    Invalid(String),
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("size is empty".into());
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(split);
    let value: f64 = number.parse().map_err(|_| format!("not a size: {text}"))?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => return Err(format!("unknown size suffix: {other}")),
    };
    Ok((value * multiplier) as u64)
}

pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| format!("not a duration: {text}"))?;
    let millis = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" => value * 1000.0,
        "ms" => value,
        "m" | "min" | "mins" => value * 60_000.0,
        "h" | "hr" | "hrs" => value * 3_600_000.0,
        other => return Err(format!("unknown duration suffix: {other}")),
    };
    Ok(Duration::from_millis(millis as u64))
}

pub fn format_ago(ms: i64) -> String {
    let delta = now_ms() - ms;
    if delta < 0 {
        return "in the future".into();
    }
    match delta {
        0..=45_000 => "just now".into(),
        45_001..=5_400_000 => format!("{}m ago", delta / 60_000),
        5_400_001..=79_200_000 => format!("{}h ago", delta / 3_600_000),
        _ => format!("{}d ago", delta / 86_400_000),
    }
}

pub fn format_timestamp(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

pub fn mtime_ns(meta: &fs::Metadata) -> i64 {
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    if let Ok(after) = modified.duration_since(UNIX_EPOCH) {
        return after.as_nanos().min(i64::MAX as u128) as i64;
    }
    if let Ok(before) = UNIX_EPOCH.duration_since(modified) {
        return -(before.as_nanos().min(i64::MAX as u128) as i64);
    }
    0
}

pub fn set_mtime(path: &Path, ns: i64) -> io::Result<()> {
    let delta = Duration::from_nanos(ns.unsigned_abs());
    let time = if ns >= 0 {
        UNIX_EPOCH + delta
    } else {
        UNIX_EPOCH - delta
    };
    let file = fs::OpenOptions::new().write(true).open(long_path(path))?;
    file.set_times(fs::FileTimes::new().set_modified(time))
}

#[cfg(unix)]
pub fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(windows)]
pub fn file_mode(meta: &fs::Metadata) -> u32 {
    if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(long_path(path), fs::Permissions::from_mode(mode & 0o7777))
}

#[cfg(windows)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut permissions = fs::metadata(long_path(path))?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(long_path(path), permissions)
}

#[cfg(unix)]
pub fn make_writable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(long_path(path))?;
    let mode = meta.permissions().mode();
    if mode & 0o200 == 0 {
        fs::set_permissions(long_path(path), fs::Permissions::from_mode(mode | 0o200))?;
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
pub fn make_writable(path: &Path) -> io::Result<()> {
    let meta = fs::metadata(long_path(path))?;
    let mut permissions = meta.permissions();
    if permissions.readonly() {
        // Windows has no PermissionsExt; clearing the readonly flag is the API.
        permissions.set_readonly(false);
        fs::set_permissions(long_path(path), permissions)?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn is_reparse(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    fs::symlink_metadata(path)
        .map(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn is_reparse(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn long_path(path: &Path) -> PathBuf {
    // Verbatim paths are not normalized by Windows, so a forward slash is a
    // literal character there. Store paths use '/', filesystem paths do not.
    let text = path.as_os_str().to_string_lossy().replace('/', r"\");
    if text.starts_with(r"\\?\") {
        return PathBuf::from(text);
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    PathBuf::from(format!(r"\\?\{text}"))
}

#[cfg(not(windows))]
pub fn long_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

pub fn validate_rel_path(rel: &str) -> Result<(), PathError> {
    if rel.is_empty() {
        return Err(PathError::Invalid("empty path".into()));
    }
    if rel.starts_with('/') {
        return Err(PathError::Escape(rel.into()));
    }
    if rel.contains('\0') {
        return Err(PathError::Invalid(format!("NUL in path: {rel}")));
    }
    if rel.contains('\\') {
        return Err(PathError::Invalid(format!("path uses a backslash: {rel}")));
    }
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(PathError::Escape(rel.into()));
        }
        #[cfg(windows)]
        if part.contains(':') {
            return Err(PathError::Invalid(format!("drive letter in path: {rel}")));
        }
    }
    Ok(())
}

pub fn join_rel(root: &Path, rel: &str) -> Result<PathBuf, PathError> {
    validate_rel_path(rel)?;
    let base = long_path(root);
    let mut path = base.clone();
    for part in rel.split('/') {
        path.push(part);
    }
    if !path.starts_with(&base) {
        return Err(PathError::Escape(rel.into()));
    }
    Ok(path)
}

pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    text.into_owned()
}

pub fn rel_string(path: &Path) -> String {
    path.components()
        .filter_map(|part| match part {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}
