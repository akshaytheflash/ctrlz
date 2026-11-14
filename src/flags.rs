//! Flag classification and rendering.
//!
//! A flag is one notable filesystem change. Deletions are always recorded,
//! because they are the thing a rogue agent does that cannot be re-derived.
//! Ordinary edits are only recorded while an agent is known to be working,
//! which keeps the ledger from turning into a general filesystem changelog.

use std::path::Path;

use serde_json::{json, Value};

use crate::global::{Config, Flag};
use crate::util::{format_ago, format_timestamp, human_size};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }

    pub fn rank(self) -> i64 {
        match self {
            Self::Info => 0,
            Self::Warn => 1,
            Self::Critical => 2,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "critical" | "crit" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Severity for a deletion. Anything we could not capture is critical: the
/// user needs to know immediately that the bytes are gone for good.
pub fn deletion_severity(config: &Config, path: &Path, recoverable: bool) -> Severity {
    if !recoverable {
        return Severity::Critical;
    }
    if config.is_protected(path) {
        return Severity::Critical;
    }
    Severity::Warn
}

/// Severity for an edit. Protected files are still only `warn` here: the file
/// exists and a restore can undo the content change.
pub fn modification_severity(config: &Config, path: &Path) -> Severity {
    if config.is_protected(path) {
        Severity::Warn
    } else {
        Severity::Info
    }
}

pub fn render(flags: &[Flag]) -> String {
    if flags.is_empty() {
        return "no flags. nothing destructive has been seen in this window.\n".to_string();
    }
    let mut out = format!(
        "{:>6}  {:<10}  {:<8}  {:<10}  {:<10}  {:<9}  path\n",
        "id", "when", "severity", "kind", "agent", "state"
    );
    for flag in flags {
        let state = if flag.kind == "deleted" || flag.kind == "modified" {
            if flag.recoverable {
                "recoverable"
            } else {
                "GONE"
            }
        } else {
            "-"
        };
        let agent = if flag.agent.is_empty() {
            "unattributed".to_string()
        } else {
            truncate(&flag.agent, 10)
        };
        out.push_str(&format!(
            "{:>6}  {:<10}  {:<8}  {:<10}  {:<10}  {:<9}  {}\n",
            flag.id,
            format_ago(flag.at),
            flag.severity,
            flag.kind,
            agent,
            state,
            flag.path
        ));
        if !flag.note.is_empty() {
            out.push_str(&format!("              note: {}\n", flag.note));
        }
    }
    out
}

pub fn describe(flag: &Flag) -> String {
    let mut out = format!("flag {}\n", flag.id);
    out.push_str(&format!("  when        {}\n", format_timestamp(flag.at)));
    out.push_str(&format!("  kind        {}\n", flag.kind));
    out.push_str(&format!("  severity    {}\n", flag.severity));
    out.push_str(&format!("  path        {}\n", flag.path));
    out.push_str(&format!("  root        {}\n", flag.root));
    if !flag.agent.is_empty() {
        out.push_str(&format!(
            "  agent       {} (session {}, {} attribution)\n",
            flag.agent, flag.session, flag.confidence
        ));
    } else {
        out.push_str("  agent       unattributed\n");
    }
    if flag.before_hash.is_empty() {
        out.push_str("  content     not captured\n");
    } else {
        out.push_str(&format!(
            "  content     {} at capture ({})\n",
            human_size(flag.size),
            if flag.recoverable {
                "recoverable"
            } else {
                "blob missing"
            }
        ));
    }
    if !flag.note.is_empty() {
        out.push_str(&format!("  note        {}\n", flag.note));
    }
    out
}

pub fn to_json(flags: &[Flag]) -> Value {
    Value::Array(
        flags
            .iter()
            .map(|flag| {
                json!({
                    "id": flag.id,
                    "at": flag.at,
                    "when": format_timestamp(flag.at),
                    "kind": flag.kind,
                    "severity": flag.severity,
                    "path": flag.path,
                    "root": flag.root,
                    "agent": if flag.agent.is_empty() { Value::Null } else { json!(flag.agent) },
                    "session": flag.session,
                    "confidence": flag.confidence,
                    "recoverable": flag.recoverable,
                    "size": flag.size,
                    "note": flag.note,
                })
            })
            .collect(),
    )
}

pub fn flag_to_json(flag: &Flag) -> Value {
    to_json(std::slice::from_ref(flag))[0].clone()
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::global::{Config, Flag};

    fn config() -> Config {
        Config::new(vec![if cfg!(windows) {
            r"C:\work".into()
        } else {
            "/work".into()
        }])
    }

    fn flag(kind: &str, severity: &str, agent: &str) -> Flag {
        Flag {
            id: 1,
            at: 1_700_000_000_000,
            kind: kind.into(),
            path: "/work/.env".into(),
            root: "/work".into(),
            severity: severity.into(),
            agent: agent.into(),
            session: String::new(),
            confidence: "window".into(),
            before_hash: "abc".into(),
            size: 12,
            mode: 0o644,
            mtime_ns: 0,
            recoverable: true,
            note: String::new(),
        }
    }

    #[test]
    fn a_protected_deletion_is_critical() {
        let config = config();
        assert_eq!(
            deletion_severity(&config, Path::new("/work/.env"), true),
            Severity::Critical
        );
    }

    #[test]
    fn an_ordinary_deletion_is_a_warning() {
        let config = config();
        assert_eq!(
            deletion_severity(&config, Path::new("/work/src/main.rs"), true),
            Severity::Warn
        );
    }

    #[test]
    fn an_uncaptured_deletion_is_always_critical() {
        let config = config();
        assert_eq!(
            deletion_severity(&config, Path::new("/work/src/main.rs"), false),
            Severity::Critical
        );
    }

    #[test]
    fn editing_a_protected_file_warns() {
        let config = config();
        assert_eq!(
            modification_severity(&config, Path::new("/work/.env")),
            Severity::Warn
        );
        assert_eq!(
            modification_severity(&config, Path::new("/work/a.rs")),
            Severity::Info
        );
    }

    #[test]
    fn severity_ranks_are_ordered() {
        assert!(Severity::Critical > Severity::Warn);
        assert!(Severity::Warn > Severity::Info);
        assert_eq!(Severity::parse("CRIT"), Some(Severity::Critical));
        assert_eq!(Severity::parse("nonsense"), None);
    }

    #[test]
    fn the_table_marks_lost_content() {
        let mut lost = flag("deleted", "critical", "");
        lost.recoverable = false;
        let text = render(&[lost]);
        assert!(text.contains("GONE"));
        assert!(text.contains("unattributed"));
        assert!(text.contains("/work/.env"));
    }

    #[test]
    fn an_empty_ledger_says_so() {
        assert!(render(&[]).contains("no flags"));
    }

    #[test]
    fn json_exposes_null_for_unattributed_changes() {
        let value = to_json(&[flag("deleted", "warn", "")]);
        assert_eq!(value[0]["agent"], Value::Null);
        assert_eq!(value[0]["severity"], "warn");
        assert_eq!(value[0]["recoverable"], true);
    }
}
