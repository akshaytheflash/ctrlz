//! A dependency-free glob matcher for `protect` and `exclude` patterns.
//!
//! Supported syntax: `*` matches any run of characters and `?` matches exactly
//! one. A pattern containing `/` matches a suffix of the path, so `.ssh/**`
//! covers `home/me/.ssh/id_ed25519`. A pattern without `/` matches the file
//! name, so `*.pem` covers `keys/server.pem`.
//!
//! Matching is case-sensitive except on Windows, where the filesystem is.

pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    let pattern = normalise(pattern);
    let path = normalise(path);
    if pattern.is_empty() {
        return false;
    }

    let (pattern, path) = if cfg!(windows) {
        (pattern.to_lowercase(), path.to_lowercase())
    } else {
        (pattern, path)
    };

    if pattern.contains('/') {
        suffix_match(&pattern, &path)
    } else {
        let name = path.rsplit('/').next().unwrap_or(path.as_str());
        wildcard(&pattern, name)
    }
}

pub fn any_matches<'a, I>(patterns: I, path: &str) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    patterns
        .into_iter()
        .any(|pattern| pattern_matches(pattern, path))
}

fn normalise(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string()
}

fn suffix_match(pattern: &str, path: &str) -> bool {
    if wildcard(pattern, path) {
        return true;
    }
    path.match_indices('/')
        .any(|(index, _)| wildcard(pattern, &path[index + 1..]))
}

fn wildcard(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(index) = star {
            p = index + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_matches_any_directory() {
        assert!(pattern_matches(".env", ".env"));
        assert!(pattern_matches(".env", "app/.env"));
        assert!(pattern_matches(".env", "deep/nested/.env"));
        assert!(!pattern_matches(".env", "app/.env.local"));
    }

    #[test]
    fn a_star_matches_a_suffix() {
        assert!(pattern_matches("*.pem", "keys/server.pem"));
        assert!(pattern_matches("*.pem", "server.pem"));
        assert!(!pattern_matches("*.pem", "server.pem.txt"));
    }

    #[test]
    fn a_question_mark_matches_one_character() {
        assert!(pattern_matches("id_?sa", "id_rsa"));
        assert!(!pattern_matches("id_?sa", "id_rrsa"));
    }

    #[test]
    fn a_pattern_with_a_separator_matches_a_path_suffix() {
        assert!(pattern_matches(".ssh/**", ".ssh/id_ed25519"));
        assert!(pattern_matches(".ssh/**", "home/me/.ssh/id_ed25519"));
        assert!(pattern_matches("a/*/c.txt", "x/a/b/c.txt"));
        assert!(!pattern_matches(".ssh/**", "ssh/id_ed25519"));
    }

    #[test]
    fn double_star_and_single_star_agree_here() {
        assert!(pattern_matches(".aws/*", ".aws/credentials"));
        assert!(pattern_matches(".aws/**", ".aws/credentials"));
    }

    #[test]
    fn backslashes_are_normalised() {
        assert!(pattern_matches(".ssh/*", r"home\me\.ssh\id_rsa"));
    }

    #[test]
    fn any_matches_stops_at_the_first_hit() {
        let patterns = ["*.pem", "*.key", ".env"];
        assert!(any_matches(patterns, "server.key"));
        assert!(!any_matches(patterns, "server.txt"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        assert!(!pattern_matches("", "anything"));
    }
}
