//! The one glob dialect shared by `sources`, `outputs` and `watch`.
//!
//! Patterns use globset syntax and are matched against `/`-separated paths
//! relative to the task's directory. `*` and `?` never cross a `/`; `**`
//! does. A leading `./` is ignored.

use globset::{Glob, GlobBuilder};

/// Strip any leading `./` so `./src/**` and `src/**` are the same pattern.
#[must_use]
pub fn normalise(pattern: &str) -> &str {
    let mut p = pattern;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.trim_start_matches('/');
    }
    p
}

/// Compile a pattern in the shared dialect.
pub fn compile(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(normalise(pattern))
        .literal_separator(true)
        .build()
}

/// The leading path components of a pattern that contain no wildcard, and
/// whether that is the whole pattern (a plain path rather than a glob).
#[must_use]
pub fn literal_base(pattern: &str) -> (String, bool) {
    let pattern = normalise(pattern).trim_end_matches('/');
    let is_meta = |c: char| matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '\\');
    let parts: Vec<&str> = pattern.split('/').collect();
    let literal = parts.iter().take_while(|c| !c.contains(is_meta)).count();
    (parts[..literal].join("/"), literal == parts.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_stops_at_a_separator_and_double_star_does_not() {
        let star = compile("*.txt").unwrap().compile_matcher();
        assert!(star.is_match("a.txt"));
        assert!(!star.is_match("sub/deep.txt"));
        let all = compile("out/**").unwrap().compile_matcher();
        assert!(all.is_match("out/a"));
        assert!(all.is_match("out/b/c.js"));
    }

    #[test]
    fn leading_dot_slash_is_ignored() {
        assert_eq!(normalise("./data.txt"), "data.txt");
        assert_eq!(normalise(".//./x"), "x");
        assert_eq!(normalise(".config/f"), ".config/f");
        assert!(compile("./data.txt")
            .unwrap()
            .compile_matcher()
            .is_match("data.txt"));
    }

    #[test]
    fn literal_base_stops_at_the_first_wildcard() {
        assert_eq!(literal_base("web/dist/**"), ("web/dist".into(), false));
        assert_eq!(
            literal_base("./gen/schema.json"),
            ("gen/schema.json".into(), true)
        );
        assert_eq!(literal_base("**/*.rs"), (String::new(), false));
        assert_eq!(literal_base("out/"), ("out".into(), true));
    }
}
