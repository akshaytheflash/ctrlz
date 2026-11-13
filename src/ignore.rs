const EXCLUDED: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".ctrlz",
];

pub fn is_excluded_name(name: &str) -> bool {
    EXCLUDED.iter().any(|item| name.eq_ignore_ascii_case(item))
}

pub fn is_excluded_rel(rel: &str) -> bool {
    rel.split('/').any(is_excluded_name)
}
