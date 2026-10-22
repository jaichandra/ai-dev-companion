//! Node's POSIX `path` functions, reproduced exactly (the Node code compared
//! and built paths with them, and Rust's `Path` normalizes differently: it
//! keeps `a/../b` and drops trailing slashes).

pub fn is_absolute(p: &str) -> bool {
    p.starts_with('/')
}

/// `path.normalize`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let abs = is_absolute(path);
    let trailing = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => match out.last() {
                Some(last) if *last != ".." => {
                    out.pop();
                }
                _ if !abs => out.push(".."),
                _ => {}
            },
            s => out.push(s),
        }
    }
    let mut r = out.join("/");
    if r.is_empty() {
        return if abs {
            "/".into()
        } else if trailing {
            "./".into()
        } else {
            ".".into()
        };
    }
    if trailing {
        r.push('/');
    }
    if abs { format!("/{r}") } else { r }
}

/// `path.join`.
pub fn join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() { ".".into() } else { normalize(&joined.join("/")) }
}

/// `path.dirname`.
pub fn dirname(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let has_root = is_absolute(p);
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into(); // only slashes
    }
    match trimmed.rfind('/') {
        None => if has_root { "/".into() } else { ".".into() },
        Some(0) => "/".into(),
        Some(i) => trimmed[..i].to_string(),
    }
}

/// `path.basename` (without the extension argument).
pub fn basename(p: &str) -> String {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

/// `path.resolve(p)`: absolute, normalized, no trailing slash (except root).
pub fn resolve(p: &str) -> String {
    let abs = if is_absolute(p) {
        p.to_string()
    } else {
        let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
        format!("{cwd}/{p}")
    };
    let n = normalize(&abs);
    if n.len() > 1 { n.trim_end_matches('/').to_string() } else { n }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_matches_node() {
        for (input, want) in [
            ("", "."),
            (".", "."),
            ("./", "./"),
            ("a/..", "."),
            ("a/../", "./"),
            ("/", "/"),
            ("/..", "/"),
            ("/a/../..", "/"),
            ("../a", "../a"),
            ("a/../../b", "../b"),
            ("/a//b/./c/", "/a/b/c/"),
            ("//a", "/a"),
        ] {
            assert_eq!(normalize(input), want, "{input:?}");
        }
    }

    #[test]
    fn join_matches_node() {
        assert_eq!(join(&["/a", "b", "../c"]), "/a/c");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(join(&[]), ".");
        assert_eq!(join(&["/Users/me", ".local", "bin", "companion"]), "/Users/me/.local/bin/companion");
        assert_eq!(join(&["companion-service", "bin", "companion.js"]), "companion-service/bin/companion.js");
    }

    #[test]
    fn dirname_matches_node() {
        for (input, want) in [
            ("/a/b", "/a"),
            ("/a", "/"),
            ("/", "/"),
            ("a", "."),
            ("", "."),
            ("a/b/", "a"),
            ("/a/b/c/", "/a/b"),
            ("/a//b", "/a/"),
            ("///", "/"),
        ] {
            assert_eq!(dirname(input), want, "{input:?}");
        }
    }

    #[test]
    fn basename_matches_node() {
        assert_eq!(basename("/Users/me/wt/pr 42"), "pr 42");
        assert_eq!(basename("/wt"), "wt");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(basename("x"), "x");
    }

    #[test]
    fn resolve_is_absolute_and_clean() {
        assert_eq!(resolve("/a/b/../c/"), "/a/c");
        assert_eq!(resolve("/"), "/");
        assert!(resolve("rel/x").starts_with('/'));
        assert!(resolve("rel/x").ends_with("/rel/x"));
    }
}
