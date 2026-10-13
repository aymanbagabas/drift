use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use url::Url;

/// Find explicit URLs delimited by whitespace, angle brackets, double quotes,
/// or backticks. Other punctuation stays part of the candidate.
pub fn find(text: &str) -> impl Iterator<Item = (Range<usize>, String)> + '_ {
    static CANDIDATE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"[A-Za-z][A-Za-z0-9+.-]*:[^\s<>"`]*"#).unwrap());
    CANDIDATE
        .find_iter(text)
        .filter_map(move |candidate| {
            if text[..candidate.start()]
                .chars()
                .next_back()
                .is_some_and(|c| {
                    c.is_alphanumeric() || matches!(c, '_' | '/' | '\\' | '.' | '+' | '-')
                })
                || candidate.as_str().chars().any(char::is_control)
            {
                return None;
            }
            Url::parse(candidate.as_str())
                .ok()
                .map(|url| (candidate.range(), String::from(url)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_schemes_and_preserves_display_ranges() {
        let text = "https://example.org/a file:///tmp/a ssh://host/a mailto:a@example.org \
                    magnet:?xt=urn:btih:abc vscode://file/tmp/a urn:isbn:123";
        let links: Vec<_> = find(text).collect();
        assert_eq!(links.len(), 7);
        for (range, target) in links {
            assert_eq!(Url::parse(&text[range]).unwrap().as_str(), target);
        }
    }

    #[test]
    fn handles_boundaries_and_unicode() {
        let text = "界 <https://example.org/a_(b)> \"https://example.org/é?q=1#part\"";
        let links: Vec<_> = find(text).collect();
        assert_eq!(links.len(), 2);
        assert_eq!(&text[links[0].0.clone()], "https://example.org/a_(b)");
        assert_eq!(links[1].1, "https://example.org/%C3%A9?q=1#part");
        assert!(find("example.org /tmp/a https://").next().is_none());
    }

    #[test]
    fn preserves_candidates_accepted_by_url() {
        for candidate in [
            "https://example.org/it's",
            "https://example.org/path!",
            "https://example.org/path.",
            "https://example.org/path,",
            "https://example.org/path;",
            "https://example.org/?",
            "https://example.org/a)",
            "x://host/path",
            "custom::value",
            "custom:",
            "C:/file",
            "std::string",
            "https://example.org/\\path",
        ] {
            let expected = Url::parse(candidate).unwrap().to_string();
            assert_eq!(
                find(candidate).collect::<Vec<_>>(),
                vec![(0..candidate.len(), expected)],
                "{candidate}"
            );
        }
    }

    #[test]
    fn uses_simple_delimiters() {
        for (text, candidate) in [
            ("\"https://example.org/it's!\"", "https://example.org/it's!"),
            ("\"https://example.org/a?\"", "https://example.org/a?"),
            ("<https://example.org/a.>", "https://example.org/a."),
            ("<https://example.org/a_(b)>", "https://example.org/a_(b)"),
            ("<https://[::1]/a>", "https://[::1]/a"),
            ("`x://host/path;`", "x://host/path;"),
        ] {
            assert_eq!(
                find(text).collect::<Vec<_>>(),
                vec![(
                    1..1 + candidate.len(),
                    Url::parse(candidate).unwrap().to_string()
                )],
                "{text}",
            );
        }
    }

    #[test]
    fn rejects_terminal_controls() {
        assert!(find("https://example.org/\x1b]8;;evil\x07").next().is_none());
        assert!(find("https://example.org/\u{009c}evil").next().is_none());
    }

    #[test]
    fn finds_multiple_urls_inside_double_quotes() {
        for text in [
            r#"const url = " https://example.org/a";"#,
            r#"const urls = "https://example.org/a https://example.org/b";"#,
            r#"Don't include "text https://example.org/a"."#,
            r#"say "escaped \" quote https://example.org/a"."#,
        ] {
            let links: Vec<_> = find(text).collect();
            let expected = if text.contains("https://example.org/b") {
                2
            } else {
                1
            };
            assert_eq!(links.len(), expected, "{text}");
            for (range, target) in links {
                let candidate = &text[range];
                assert!(
                    matches!(candidate, "https://example.org/a" | "https://example.org/b"),
                    "{candidate}"
                );
                assert_eq!(target, Url::parse(candidate).unwrap().as_str());
            }
        }
    }
}
