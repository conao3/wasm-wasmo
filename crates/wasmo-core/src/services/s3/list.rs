#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Last {
    Entry,
    Prefix,
}

#[derive(Debug)]
pub struct Page<T> {
    pub entries: Vec<T>,
    pub prefixes: Vec<String>,
    pub truncated: bool,
    pub last: Option<Last>,
}

impl<T> Page<T> {
    pub fn count(&self) -> usize {
        self.entries.len() + self.prefixes.len()
    }
}

/// The `CommonPrefixes` group `key` rolls up into, if any.
pub fn common_prefix<'a>(key: &'a str, prefix: &str, delimiter: &str) -> Option<&'a str> {
    if delimiter.is_empty() {
        return None;
    }
    let rest = key.strip_prefix(prefix)?;
    rest.find(delimiter)
        .map(|i| &key[..prefix.len() + i + delimiter.len()])
}

/// Cuts one page from `items`, sorted by key and already positioned after
/// the marker. Keys sharing a delimited prefix count once; a group equal to
/// `after` was returned on the previous page and is skipped.
pub fn page<'a, T>(
    items: impl IntoIterator<Item = (&'a str, T)>,
    prefix: &str,
    delimiter: &str,
    after: &str,
    max: usize,
) -> Page<T> {
    let mut out = Page {
        entries: Vec::new(),
        prefixes: Vec::new(),
        truncated: false,
        last: None,
    };
    for (key, item) in items {
        if !key.starts_with(prefix) {
            continue;
        }
        let group = common_prefix(key, prefix, delimiter);
        if let Some(g) = group
            && (g == after || out.prefixes.last().is_some_and(|p| p == g))
        {
            continue;
        }
        if out.count() >= max {
            out.truncated = max > 0;
            break;
        }
        match group {
            Some(g) => {
                out.prefixes.push(g.to_string());
                out.last = Some(Last::Prefix);
            }
            None => {
                out.entries.push(item);
                out.last = Some(Last::Entry);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &[
        "a.txt",
        "photos/2024/jan.jpg",
        "photos/2024/feb.jpg",
        "photos/2025/mar.jpg",
        "photos/index.html",
        "videos/x.mp4",
        "z",
    ];

    fn run(prefix: &str, delimiter: &str, after: &str, max: usize) -> Page<&'static str> {
        let mut keys = KEYS.to_vec();
        keys.sort();
        page(
            keys.into_iter().filter(|k| *k > after).map(|k| (k, k)),
            prefix,
            delimiter,
            after,
            max,
        )
    }

    #[test]
    fn groups_by_delimiter() {
        let p = run("", "/", "", 1000);
        assert_eq!(p.entries, vec!["a.txt", "z"]);
        assert_eq!(p.prefixes, vec!["photos/", "videos/"]);
        assert!(!p.truncated);
    }

    #[test]
    fn groups_under_prefix() {
        let p = run("photos/", "/", "", 1000);
        assert_eq!(p.entries, vec!["photos/index.html"]);
        assert_eq!(p.prefixes, vec!["photos/2024/", "photos/2025/"]);
    }

    #[test]
    fn without_delimiter_lists_every_key_under_prefix() {
        let p = run("photos/2024/", "", "", 1000);
        assert_eq!(
            p.entries,
            vec!["photos/2024/feb.jpg", "photos/2024/jan.jpg"]
        );
        assert!(p.prefixes.is_empty());
    }

    #[test]
    fn paginates_across_groups() {
        let first = run("", "/", "", 2);
        assert_eq!(first.entries, vec!["a.txt"]);
        assert_eq!(first.prefixes, vec!["photos/"]);
        assert!(first.truncated);
        assert_eq!(first.last, Some(Last::Prefix));
        let second = run("", "/", "photos/", 2);
        assert_eq!(second.entries, vec!["z"]);
        assert_eq!(second.prefixes, vec!["videos/"]);
        assert!(!second.truncated);
    }

    #[test]
    fn start_after_and_max_keys_zero() {
        let p = run("", "", "photos/index.html", 1000);
        assert_eq!(p.entries, vec!["videos/x.mp4", "z"]);
        let empty = run("", "", "", 0);
        assert_eq!(empty.count(), 0);
        assert!(!empty.truncated);
    }

    #[test]
    fn multi_character_delimiter() {
        assert_eq!(common_prefix("a--b--c", "", "--"), Some("a--"));
        assert_eq!(common_prefix("a--b--c", "a--", "--"), Some("a--b--"));
        assert_eq!(common_prefix("abc", "x", "/"), None);
    }
}
