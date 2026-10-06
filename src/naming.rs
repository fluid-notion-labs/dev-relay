pub const EXE_SUFFIX: &str = ".exe";

pub fn has_exe_suffix(name: &str) -> bool {
    name.ends_with(EXE_SUFFIX)
}

pub fn base_of(name: &str) -> &str {
    name.strip_suffix(EXE_SUFFIX).unwrap_or(name)
}

pub fn suffix_key(name: &str) -> &'static str {
    if has_exe_suffix(name) { "exe" } else { "" }
}

pub fn is_bin_name(name: &str) -> bool {
    let base = base_of(name);
    let (hex, seq) = match base.split_once("-d") {
        Some((h, s)) => (h, Some(s)),
        None => (base, None),
    };
    let hex_ok = (4..=40).contains(&hex.len())
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let seq_ok = match seq {
        None => true,
        Some(s) => (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()),
    };
    hex_ok && seq_ok
}

pub fn is_session_id(s: &str) -> bool {
    (6..=16).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub fn next_dirty_name(sha: &str, exe: bool, existing: &[String]) -> String {
    let suffix = if exe { EXE_SUFFIX } else { "" };
    let prefix = format!("{sha}-d");
    let mut max = 0u32;
    for name in existing {
        let Some(base) = name.strip_suffix(suffix) else {
            continue;
        };
        if let Some(digits) = base.strip_prefix(&prefix)
            && let Ok(n) = digits.parse::<u32>()
        {
            max = max.max(n);
        }
    }
    format!("{sha}-d{}{}", max + 1, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        for good in [
            "0123abc",
            "abcdef0",
            "0123abc-d1",
            "0123abc.exe",
            "0123abc-d12.exe",
        ] {
            assert!(is_bin_name(good), "{good}");
        }
        for bad in [
            "",
            "abc",
            "ABC123",
            "0123abc.exe.exe",
            "0123abc-d",
            "0123abc-d12345",
            "0123abc-D1",
            "0123abc-d1x",
            "../etc/passwd",
            "0123abc.exe/bins",
        ] {
            assert!(!is_bin_name(bad), "{bad}");
        }
    }

    #[test]
    fn validates_session_ids() {
        for good in ["ab12cd", "ab12cdx", "0123456789abcdef"] {
            assert!(is_session_id(good), "{good}");
        }
        for bad in [
            "",
            "ab12c",
            "0123456789abcdef0",
            "AB12CD",
            "a b",
            "../x",
            "ab-12",
        ] {
            assert!(!is_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn dirty_names_progress() {
        let existing = vec!["0123abc".to_string(), "0123abc-d1".to_string()];
        assert_eq!(next_dirty_name("0123abc", false, &existing), "0123abc-d2");
        assert_eq!(
            next_dirty_name("0123abc", true, &existing),
            "0123abc-d1.exe"
        );
        let with_exe = vec!["0123abc-d2.exe".to_string()];
        assert_eq!(
            next_dirty_name("0123abc", true, &with_exe),
            "0123abc-d3.exe"
        );
        assert_eq!(next_dirty_name("0123abc", false, &with_exe), "0123abc-d1");
    }

    #[test]
    fn suffix_helpers() {
        assert!(has_exe_suffix("a.exe"));
        assert!(!has_exe_suffix("a"));
        assert_eq!(base_of("a.exe"), "a");
        assert_eq!(base_of("a"), "a");
        assert_eq!(suffix_key("a.exe"), "exe");
        assert_eq!(suffix_key("a"), "");
    }
}
