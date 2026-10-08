//! Which changed files are generated — lockfiles, snapshots, minified
//! bundles, code generators' output, and anything the repository marks
//! `linguist-generated` in its `.gitattributes` (what GitHub itself collapses).

use regex::Regex;

/// Built-in patterns, as `.gitattributes`-style globs with the reason shown.
const BUILT_IN: &[(&str, &str)] = &[
    ("package-lock.json", "lockfile"),
    ("npm-shrinkwrap.json", "lockfile"),
    ("yarn.lock", "lockfile"),
    ("pnpm-lock.yaml", "lockfile"),
    ("bun.lock", "lockfile"),
    ("bun.lockb", "lockfile"),
    ("Cargo.lock", "lockfile"),
    ("Gemfile.lock", "lockfile"),
    ("poetry.lock", "lockfile"),
    ("Pipfile.lock", "lockfile"),
    ("uv.lock", "lockfile"),
    ("composer.lock", "lockfile"),
    ("go.sum", "lockfile"),
    ("flake.lock", "lockfile"),
    ("pubspec.lock", "lockfile"),
    ("mix.lock", "lockfile"),
    ("Podfile.lock", "lockfile"),
    ("packages.lock.json", "lockfile"),
    ("*.min.js", "minified"),
    ("*.min.css", "minified"),
    ("*.js.map", "source map"),
    ("*.css.map", "source map"),
    ("*.snap", "snapshot"),
    ("__snapshots__/**", "snapshot"),
    ("*.pb.go", "generated code"),
    ("*_pb2.py", "generated code"),
    ("*_pb2_grpc.py", "generated code"),
    ("*.pb.swift", "generated code"),
    ("*.g.dart", "generated code"),
    ("*.freezed.dart", "generated code"),
    ("*.generated.*", "generated code"),
];

struct Rule {
    pattern: Regex,
    /// `false` when a `.gitattributes` line explicitly says not generated.
    generated: bool,
    reason: &'static str,
}

/// Turns a `.gitattributes` pattern into an anchored regex over repo paths.
/// A pattern without a slash matches a file name at any depth; one with a
/// slash is relative to the repository root.
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let anchored = pattern.trim_end_matches('/').contains('/');
    let pattern = pattern.trim_start_matches('/');
    let mut re = String::from(if anchored { "^" } else { "^(?:.*/)?" });
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                let end = chars[i..].iter().position(|&c| c == ']')? + i;
                re.extend(&chars[i..=end]);
                i = end + 1;
                continue;
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).ok()
}

pub struct Classifier {
    rules: Vec<Rule>,
}

impl Classifier {
    /// Built-in patterns first, then the repository's own `.gitattributes`,
    /// so the repository has the last word either way.
    pub fn new(gitattributes: Option<&str>) -> Self {
        let mut rules: Vec<Rule> = BUILT_IN
            .iter()
            .filter_map(|&(glob, reason)| {
                Some(Rule {
                    pattern: glob_to_regex(glob)?,
                    generated: true,
                    reason,
                })
            })
            .collect();
        for line in gitattributes.unwrap_or("").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(glob) = parts.next() else { continue };
            let setting = parts.find_map(|attr| match attr {
                "linguist-generated" | "linguist-generated=true" => Some(true),
                "-linguist-generated" | "linguist-generated=false" => Some(false),
                _ => None,
            });
            if let (Some(generated), Some(pattern)) = (setting, glob_to_regex(glob)) {
                rules.push(Rule {
                    pattern,
                    generated,
                    reason: ".gitattributes",
                });
            }
        }
        Classifier { rules }
    }

    /// Why `path` counts as generated, or `None` if it doesn't.
    pub fn classify(&self, path: &str) -> Option<&'static str> {
        let last = self.rules.iter().rev().find(|r| r.pattern.is_match(path))?;
        last.generated.then_some(last.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(glob: &str, path: &str) -> bool {
        glob_to_regex(glob).unwrap().is_match(path)
    }

    #[test]
    fn globs_follow_gitattributes_rules() {
        assert!(matches("*.snap", "src/__tests__/a.test.ts.snap"));
        assert!(matches("yarn.lock", "packages/web/yarn.lock"));
        assert!(matches("/api/*.ts", "api/client.ts"));
        assert!(!matches("/api/*.ts", "src/api/client.ts"));
        assert!(!matches("api/*.ts", "api/v1/client.ts"));
        assert!(matches("gen/**", "gen/a/b/c.ts"));
        assert!(matches("**/gen/*.ts", "x/y/gen/z.ts"));
        assert!(matches("*.[ch]", "lib/x.h"));
    }

    #[test]
    fn built_ins_cover_lockfiles_snapshots_and_codegen() {
        let c = Classifier::new(None);
        assert_eq!(c.classify("pnpm-lock.yaml"), Some("lockfile"));
        assert_eq!(c.classify("apps/web/package-lock.json"), Some("lockfile"));
        assert_eq!(
            c.classify("ui/__snapshots__/Button.test.tsx.snap"),
            Some("snapshot")
        );
        assert_eq!(c.classify("proto/user.pb.go"), Some("generated code"));
        assert_eq!(c.classify("src/main.rs"), None);
        assert_eq!(c.classify("docs/lockfile.md"), None);
    }

    #[test]
    fn the_repository_has_the_last_word() {
        let attrs = "# comments are ignored\n\
                     src/gen/** linguist-generated\n\
                     Cargo.lock -linguist-generated\n\
                     *.ts text eol=lf\n";
        let c = Classifier::new(Some(attrs));
        assert_eq!(c.classify("src/gen/api.ts"), Some(".gitattributes"));
        assert_eq!(c.classify("Cargo.lock"), None, "explicitly not generated");
        assert_eq!(
            c.classify("src/app.ts"),
            None,
            "other attributes don't count"
        );
    }
}
