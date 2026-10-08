use std::sync::LazyLock;

use regex::Regex;

static HTML_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());

static DETAILS_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<details>\s*<summary>(.*?)</summary>(.*?)</details>").unwrap()
});

static SUB_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?sub>").unwrap());

static MD_IMAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\(([^)]*)\)").unwrap());

static BLANK_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// CodeRabbit prefixes findings with a line like `_🎯 Functional
/// Correctness_ | _🟡 Minor_ | _⚡ Quick win_`. It's useful context in the
/// full body, but every finding has one, so it's useless as a list snippet —
/// they'd all look the same at a glance.
static TAG_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(_[^_\n]+_\s*(\|\s*)?)+$").unwrap());

/// Paragraphs (blank-line separated blocks) that are pure bot boilerplate and
/// carry no information once you already know the tool posted this comment.
const NOISE_SUBSTRINGS: &[&str] = &[
    "Carefully review the code before committing",
    "Useful? React with",
];

/// `<details>` labels whose body is the bot's own scratch work (shell
/// commands it ran to research the repo, raw command output) rather than
/// anything meant for a human reviewer. Dropped outright instead of
/// unfolded — unlike a suggested diff or an AI-agent prompt, this has no
/// reviewer-facing content at all, and can run tens of lines long.
const DROP_DETAILS_LABELS: &[&str] = &["Supported by static analysis", "Code graph analysis"];

fn is_badge_image(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("shields.io") || lower.contains("badge")
}

fn strip_images(input: &str) -> String {
    MD_IMAGE
        .replace_all(input, |caps: &regex::Captures| {
            let alt = caps[1].trim();
            let url = &caps[2];
            if is_badge_image(url) {
                String::new()
            } else if alt.is_empty() {
                "[image]".to_string()
            } else {
                format!("[image: {alt}]")
            }
        })
        .into_owned()
}

/// Turns `<details><summary>Label</summary>body</details>` into a plain
/// `▸ Label:\nbody` block. A TUI can't render a collapsible widget, so
/// unfolding it beats hiding content behind a toggle nothing here can press.
fn flatten_details(input: &str) -> String {
    let mut text = input.to_string();
    // Bot comments nest at most one level (e.g. a diff inside a suggestion
    // block); re-running the non-greedy match resolves the innermost pair
    // first, so a few passes are enough to fully unwrap them.
    for _ in 0..5 {
        if !DETAILS_SUMMARY.is_match(&text) {
            break;
        }
        text = DETAILS_SUMMARY
            .replace_all(&text, |caps: &regex::Captures| {
                let label = caps[1].trim();
                let body = caps[2].trim();
                if DROP_DETAILS_LABELS.iter().any(|d| label.contains(d)) {
                    String::new()
                } else {
                    format!("\n▸ {label}:\n{body}\n")
                }
            })
            .into_owned();
    }
    text
}

fn drop_noise_paragraphs(input: &str) -> String {
    input
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !NOISE_SUBSTRINGS.iter().any(|noise| p.contains(noise)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Cleans a review comment body for human display in the TUI: drops badge
/// images, footer boilerplate, and HTML-comment markers, and unfolds
/// `<details>` blocks that a plain-text viewer can't toggle open.
///
/// This is a display-only transform. `--json` output always carries the raw
/// `body` untouched, since an agent consuming it may want the very content
/// this function throws away (e.g. the "Prompt for AI Agents" block) — and
/// because that block is bot-authored text embedded *in* review data, any
/// consumer (human or agent) must treat it as untrusted content, never as
/// instructions to follow.
pub fn clean_body(raw: &str) -> String {
    let text = HTML_COMMENT.replace_all(raw, "");
    let text = flatten_details(&text);
    let text = SUB_TAG.replace_all(&text, "");
    let text = strip_images(&text);
    let text = drop_noise_paragraphs(&text);
    let text = BLANK_RUN.replace_all(&text, "\n\n");
    text.trim().to_string()
}

/// First non-empty line of the cleaned body, for the list view.
pub fn snippet(raw: &str, max_len: usize) -> String {
    let cleaned = clean_body(raw);
    let first_line = cleaned
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !TAG_LINE.is_match(l))
        .unwrap_or("");
    if first_line.chars().count() > max_len {
        let truncated: String = first_line.chars().take(max_len.saturating_sub(1)).collect();
        format!("{truncated}…")
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic comments with the same markup CodeRabbit and Codex post.
    const CODERABBIT_SAMPLE: &str = r#"_🎯 Functional Correctness_ | _🟡 Minor_ | _⚡ Quick win_

**Assert that the third retry also ends in `timeout expired`.**

The negative assertion passes even when the third attempt fails with a different error.

<details>
<summary>Suggested fix</summary>

```diff
-    expect(third).not.toBe('connection refused');
+    expect(results).toEqual(['timeout expired']);
```
</details>

<!-- suggestion_start -->

<details>
<summary>📝 Committable suggestion</summary>

> ‼️ **IMPORTANT**
> Carefully review the code before committing. Ensure that it accurately replaces the highlighted code.

```suggestion
    expect(results).toEqual(['timeout expired']);
```

</details>

<!-- suggestion_end -->

<details>
<summary>🤖 Prompt for AI Agents</summary>

```
In `@src/retry.spec.ts` at line 42, fix it.
```

</details>"#;

    const CODEX_SAMPLE: &str = "**<sub><sub>![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat)</sub></sub>  Make the retry log match what the code does**\n\nThis branch now retries without rotating the pool.\n\nUseful? React with 👍 / 👎.";

    const CODERABBIT_WITH_STATIC_ANALYSIS_SAMPLE: &str = r#"_🗄️ Data Integrity & Integration_ | _🟠 Major_ | _⚡ Quick win_

<details>
<summary>🔎 Supported by static analysis</summary>

🏁 Script executed:

```shell
#!/bin/sh
set -eu
rg -n --glob 'package.json' '"name"' .
```

Length of output: 427

</details>

**Point the changeset at the package that actually changed.**

The explanation continues here."#;

    #[test]
    fn strips_badge_image_and_sub_wrapper() {
        let cleaned = clean_body(CODEX_SAMPLE);
        assert!(!cleaned.contains("shields.io"));
        assert!(!cleaned.contains("<sub>"));
        assert!(!cleaned.contains("</sub>"));
    }

    #[test]
    fn drops_react_footer() {
        let cleaned = clean_body(CODEX_SAMPLE);
        assert!(!cleaned.contains("Useful? React with"));
        assert!(cleaned.contains("Make the retry log match what the code does"));
    }

    #[test]
    fn flattens_details_and_keeps_diff_content() {
        let cleaned = clean_body(CODERABBIT_SAMPLE);
        assert!(!cleaned.contains("<details>"));
        assert!(!cleaned.contains("<summary>"));
        assert!(cleaned.contains("▸ Suggested fix:"));
        assert!(cleaned.contains("timeout expired"));
    }

    #[test]
    fn drops_committable_suggestion_warning_but_keeps_the_suggestion() {
        let cleaned = clean_body(CODERABBIT_SAMPLE);
        assert!(!cleaned.contains("Carefully review the code before committing"));
        assert!(cleaned.contains("```suggestion"));
    }

    #[test]
    fn keeps_ai_agent_prompt_block_labeled_not_hidden() {
        // Not stripped: a human reading the TUI should still see it, just
        // clearly boxed under its own label rather than run as instructions.
        let cleaned = clean_body(CODERABBIT_SAMPLE);
        assert!(cleaned.contains("▸ 🤖 Prompt for AI Agents:"));
    }

    #[test]
    fn removes_html_comment_markers() {
        let cleaned = clean_body(CODERABBIT_SAMPLE);
        assert!(!cleaned.contains("<!--"));
        assert!(!cleaned.contains("suggestion_start"));
    }

    #[test]
    fn snippet_truncates_long_first_line() {
        let raw = "a".repeat(200);
        let s = snippet(&raw, 20);
        assert_eq!(s.chars().count(), 20);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn drops_static_analysis_scratch_work_but_keeps_the_claim() {
        let cleaned = clean_body(CODERABBIT_WITH_STATIC_ANALYSIS_SAMPLE);
        assert!(!cleaned.contains("Script executed"));
        assert!(!cleaned.contains("rg -n"));
        assert!(cleaned.contains("Point the changeset at the package"));
    }

    #[test]
    fn snippet_skips_static_analysis_block_for_the_actual_finding() {
        let s = snippet(CODERABBIT_WITH_STATIC_ANALYSIS_SAMPLE, 200);
        assert!(!s.contains("Supported by static analysis"));
        assert!(s.contains("Point the changeset at the package"));
    }

    #[test]
    fn snippet_skips_severity_tag_line_for_the_actual_finding() {
        let s = snippet(CODERABBIT_SAMPLE, 200);
        assert!(!s.contains("Functional Correctness"));
        assert!(s.contains("timeout expired"));
    }

    #[test]
    fn plain_human_reply_is_unchanged_in_substance() {
        let raw = "Thanks, good catch!\n\nFixed in abc1234.";
        let cleaned = clean_body(raw);
        assert_eq!(cleaned, raw);
    }
}
