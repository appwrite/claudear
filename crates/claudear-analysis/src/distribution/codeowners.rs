//! GitHub CODEOWNERS parsing.

use regex_lite::Regex;

/// The owners a repository's CODEOWNERS file names for each path.
///
/// Only individual `@login` owners are kept: team owners (`@org/team`) and
/// email owners cannot be matched to a roster login.
#[derive(Debug, Clone, Default)]
pub struct CodeOwners {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: Regex,
    owners: Vec<String>,
}

impl CodeOwners {
    /// Parse a CODEOWNERS file, skipping comments and patterns that do not
    /// compile.
    pub fn parse(text: &str) -> Self {
        let rules = text
            .lines()
            .filter_map(|line| {
                let line = line.split_once('#').map_or(line, |(rule, _)| rule).trim();
                let mut fields = line.split_whitespace();
                let pattern = compile(fields.next()?)?;
                let owners = fields
                    .filter_map(|owner| owner.strip_prefix('@'))
                    .filter(|owner| !owner.contains('/'))
                    .map(str::to_string)
                    .collect();
                Some(Rule { pattern, owners })
            })
            .collect();
        Self { rules }
    }

    /// Logins owning `path`. As on GitHub, the last matching rule wins, so a
    /// later rule with no owners leaves the path unowned.
    pub fn owners(&self, path: &str) -> &[String] {
        let path = path.trim_start_matches('/');
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.pattern.is_match(path))
            .map_or(&[], |rule| rule.owners.as_slice())
    }
}

/// Translate a gitignore-style CODEOWNERS pattern into an anchored regex.
///
/// A pattern containing a slash other than a trailing one is anchored to the
/// repository root; otherwise it matches at any depth. A pattern matching a
/// directory owns everything beneath it.
fn compile(pattern: &str) -> Option<Regex> {
    let directory = pattern.ends_with('/');
    let trimmed = pattern.trim_end_matches('/');
    let anchored = trimmed.contains('/');
    let body = trimmed.trim_start_matches('/');
    if body.is_empty() {
        return None;
    }

    let mut expression = String::from(if anchored { "^" } else { "^(?:.*/)?" });
    let characters: Vec<char> = body.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        match characters[index] {
            '*' if characters.get(index + 1) == Some(&'*') => {
                if characters.get(index + 2) == Some(&'/') {
                    expression.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    expression.push_str(".*");
                    index += 2;
                }
            }
            '*' => {
                expression.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                expression.push_str("[^/]");
                index += 1;
            }
            character => {
                expression.push_str(&regex_lite::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    expression.push_str(if directory { "/.*$" } else { "(?:/.*)?$" });
    Regex::new(&expression).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
# Default owners
*               @root-owner @org/platform
*.rs            @rustacean   # inline comment
/docs/          @writer
src/api/**      @api-owner
**/migrations   @dba
build/??.sh     @builder
/vendor/
";

    fn owners(path: &str) -> Vec<String> {
        CodeOwners::parse(FILE).owners(path).to_vec()
    }

    #[test]
    fn default_rule_keeps_individuals_and_drops_teams() {
        assert_eq!(owners("README.md"), vec!["root-owner"]);
    }

    #[test]
    fn extension_patterns_match_at_any_depth() {
        assert_eq!(owners("crates/core/src/lib.rs"), vec!["rustacean"]);
    }

    #[test]
    fn anchored_directories_own_their_contents_only_at_the_root() {
        assert_eq!(owners("docs/guide/intro.md"), vec!["writer"]);
        assert_eq!(owners("site/docs/intro.md"), vec!["root-owner"]);
    }

    #[test]
    fn double_star_spans_directories() {
        assert_eq!(owners("src/api/v1/routes.ts"), vec!["api-owner"]);
        assert_eq!(owners("db/a/b/migrations/001.sql"), vec!["dba"]);
        assert_eq!(owners("migrations/001.sql"), vec!["dba"]);
    }

    #[test]
    fn question_mark_matches_one_character() {
        assert_eq!(owners("build/ab.sh"), vec!["builder"]);
        assert_eq!(owners("build/abc.sh"), vec!["root-owner"]);
    }

    #[test]
    fn a_later_rule_without_owners_unowns_the_path() {
        assert!(owners("vendor/lib/x.rs").is_empty());
    }

    #[test]
    fn empty_file_owns_nothing() {
        assert!(CodeOwners::parse("").owners("anything").is_empty());
    }
}
