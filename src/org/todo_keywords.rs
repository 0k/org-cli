//! Custom org-mode TODO keyword sets.
//!
//! orgize's default [`ParseConfig`](orgize::ParseConfig) hardcodes the
//! active/done keyword sets to `TODO`/`DONE`. Real org files routinely
//! declare their own sequences with in-buffer `#+TODO:` (or the legacy
//! `#+SEQ_TODO:` / `#+TYP_TODO:`) lines, and Emacs lets you point at a
//! global list too. This module models such a set and knows how to
//! derive one from either a CLI spec string or an in-buffer declaration.
//!
//! orgize does NOT read `#+TODO:` lines itself, so callers must
//! pre-scan the buffer (see [`scan_buffer`]) and feed the result into
//! `ParseConfig`.

use serde::Serialize;

/// A resolved pair of org-mode TODO keyword sets.
///
/// `active` holds the "not done" states (e.g. `TODO`, `WIP`) and `done`
/// the terminal states (e.g. `DONE`, `CANCELED`). The order within each
/// vector is preserved as declared, matching Emacs' left-to-right
/// sequence semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TodoKeywords {
    /// Active ("not done") keywords, in declaration order.
    pub active: Vec<String>,
    /// Done (terminal) keywords, in declaration order.
    pub done: Vec<String>,
}

impl Default for TodoKeywords {
    /// The stock org-mode set: a single `TODO` active state and a
    /// single `DONE` terminal state — identical to orgize's default
    /// `ParseConfig`.
    fn default() -> Self {
        Self {
            active: vec!["TODO".to_string()],
            done: vec!["DONE".to_string()],
        }
    }
}

/// Strip a trailing fast-access / logging annotation from a keyword word.
///
/// Org annotates keywords with a parenthesized suffix carrying the
/// fast-access key and logging directives, e.g. `WIP(w!)` or
/// `CANCELED(c@/!)`. Only a suffix that closes the word (the word ends
/// with `)`) is treated as an annotation; a stray `(` without a closing
/// `)` leaves the word untouched.
///
/// Returns the bare keyword. An empty result (the word was *only* an
/// annotation, like `(x)`) is reported to the caller as `None`.
fn strip_annotation(word: &str) -> Option<&str> {
    let bare = match word.rfind('(') {
        Some(open) if word.ends_with(')') => &word[..open],
        _ => word,
    };
    if bare.is_empty() { None } else { Some(bare) }
}

/// Parse a TODO keyword declaration body into a [`TodoKeywords`].
///
/// The `spec` is the text after the directive name (the part after
/// `#+TODO:` or the raw `--todo-keywords` value), for example
/// `"TODO WIP(w!) BLOCKED | DONE CANCELED(c@/!)"`.
///
/// Rules (Emacs semantics):
/// - Words are whitespace-separated.
/// - A single `|` separates the active set (left) from the done set
///   (right).
/// - When no `|` is present, the LAST word is the done state and all
///   preceding words are active states.
/// - Fast-access / logging annotations (a trailing `(...)` group) are
///   stripped from each word.
///
/// # Errors
///
/// Returns `Err` when the spec yields no keywords at all, when a `|` has
/// nothing on one side, or when a word is only an annotation.
pub fn parse_spec(spec: &str) -> Result<TodoKeywords, String> {
    let bar_count = spec.split_whitespace().filter(|w| *w == "|").count();
    if bar_count > 1 {
        return Err(format!(
            "invalid TODO spec {spec:?}: at most one `|` separator allowed"
        ));
    }

    if bar_count == 1 {
        let mut parts = spec.splitn(2, '|');
        let active_raw = parts.next().unwrap_or("");
        let done_raw = parts.next().unwrap_or("");
        let active = strip_words(active_raw)?;
        let done = strip_words(done_raw)?;
        if active.is_empty() || done.is_empty() {
            return Err(format!(
                "invalid TODO spec {spec:?}: `|` needs keywords on both sides"
            ));
        }
        return Ok(TodoKeywords { active, done });
    }

    // No separator: the last word is the done state, the rest active.
    let mut words = strip_words(spec)?;
    if words.is_empty() {
        return Err(format!("invalid TODO spec {spec:?}: no keywords found"));
    }
    let done = vec![words.pop().expect("non-empty checked above")];
    Ok(TodoKeywords {
        active: words,
        done,
    })
}

/// Strip annotations from every whitespace-separated word in `segment`.
///
/// The `|` token, if present, must be filtered out by the caller before
/// reaching here (it is treated as a normal word otherwise). A word that
/// is only an annotation is an error.
fn strip_words(segment: &str) -> Result<Vec<String>, String> {
    segment
        .split_whitespace()
        .map(|word| {
            strip_annotation(word)
                .map(str::to_string)
                .ok_or_else(|| format!("invalid TODO keyword {word:?}: only an annotation"))
        })
        .collect()
}

/// Recognized in-buffer directive names that declare TODO sequences.
///
/// Matched case-insensitively against the `#+NAME:` prefix.
const DIRECTIVES: [&str; 3] = ["TODO", "SEQ_TODO", "TYP_TODO"];

/// Extract the declaration body from a single line, if it is a TODO
/// directive line.
///
/// Returns the spec text (everything after the `:`), or `None` when the
/// line is not a `#+TODO:` / `#+SEQ_TODO:` / `#+TYP_TODO:` line. The
/// directive name is matched case-insensitively.
fn directive_body(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("#+")?;
    let (name, body) = rest.split_once(':')?;
    let name_upper = name.trim().to_ascii_uppercase();
    if DIRECTIVES.contains(&name_upper.as_str()) {
        Some(body)
    } else {
        None
    }
}

/// Scan `content` for in-buffer TODO keyword declarations.
///
/// Recognizes `#+TODO:`, `#+SEQ_TODO:` and `#+TYP_TODO:` lines
/// (case-insensitive on the directive name) anywhere in the buffer — not
/// just before the first headline, matching Emacs. Multiple declaration
/// lines MERGE: the active sets are unioned and the done sets are
/// unioned, preserving first-occurrence order and removing duplicates
/// (several sequences are a legal org idiom).
///
/// Returns `None` when the buffer carries no (valid) declaration.
/// Individual malformed declaration lines are skipped; if every
/// declaration line is malformed the result is `None`.
pub fn scan_buffer(content: &str) -> Option<TodoKeywords> {
    let mut active: Vec<String> = Vec::new();
    let mut done: Vec<String> = Vec::new();
    let mut found = false;

    for line in content.lines() {
        let Some(body) = directive_body(line) else {
            continue;
        };
        let Ok(parsed) = parse_spec(body) else {
            // Malformed declaration line: skip, keep scanning.
            continue;
        };
        found = true;
        push_unique(&mut active, parsed.active);
        push_unique(&mut done, parsed.done);
    }

    if found {
        Some(TodoKeywords { active, done })
    } else {
        None
    }
}

/// Append every item of `incoming` to `target`, skipping values already
/// present (preserving first-occurrence order).
fn push_unique(target: &mut Vec<String>, incoming: Vec<String>) {
    for item in incoming {
        if !target.contains(&item) {
            target.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TodoKeywords, parse_spec, scan_buffer};
    use indoc::indoc;

    #[test]
    fn default_is_todo_done() {
        assert_eq!(
            TodoKeywords::default(),
            TodoKeywords {
                active: vec!["TODO".to_string()],
                done: vec!["DONE".to_string()],
            }
        );
    }

    #[test]
    fn parse_spec_with_bar_separates_active_and_done() {
        assert_eq!(
            parse_spec("TODO WIP BLOCKED | DONE CANCELED").unwrap(),
            TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string(), "BLOCKED".to_string()],
                done: vec!["DONE".to_string(), "CANCELED".to_string()],
            }
        );
    }

    #[test]
    fn parse_spec_without_bar_last_word_is_done() {
        assert_eq!(
            parse_spec("TODO WIP DONE").unwrap(),
            TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string()],
            }
        );
    }

    #[test]
    fn parse_spec_single_word_is_done_only() {
        assert_eq!(
            parse_spec("DONE").unwrap(),
            TodoKeywords {
                active: vec![],
                done: vec!["DONE".to_string()],
            }
        );
    }

    #[test]
    fn parse_spec_strips_fast_access_and_logging_annotations() {
        assert_eq!(
            parse_spec("TODO WIP(w!) | DONE CANCELED(c@/!)").unwrap(),
            TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string(), "CANCELED".to_string()],
            }
        );
    }

    #[test]
    fn parse_spec_empty_is_error() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("   ").is_err());
    }

    #[test]
    fn parse_spec_bare_bar_is_error() {
        assert!(parse_spec("|").is_err());
        assert!(parse_spec("TODO |").is_err());
        assert!(parse_spec("| DONE").is_err());
    }

    #[test]
    fn parse_spec_annotation_only_word_is_error() {
        assert!(parse_spec("TODO (w) | DONE").is_err());
    }

    #[test]
    fn parse_spec_rejects_multiple_bars() {
        assert!(parse_spec("TODO | WIP | DONE").is_err());
    }

    #[test]
    fn scan_buffer_returns_none_without_declaration() {
        let content = indoc! {"
            * TODO A task
            * Some heading
        "};
        assert_eq!(scan_buffer(content), None);
    }

    #[test]
    fn scan_buffer_reads_todo_directive() {
        let content = indoc! {"
            #+TODO: TODO WIP | DONE CANCELED
            * WIP Working
        "};
        assert_eq!(
            scan_buffer(content),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string(), "CANCELED".to_string()],
            })
        );
    }

    #[test]
    fn scan_buffer_recognizes_seq_and_typ_directives() {
        let seq = "#+SEQ_TODO: TODO WIP | DONE\n";
        assert_eq!(
            scan_buffer(seq),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string()],
            })
        );
        let typ = "#+TYP_TODO: FRED SARA | DONE\n";
        assert_eq!(
            scan_buffer(typ),
            Some(TodoKeywords {
                active: vec!["FRED".to_string(), "SARA".to_string()],
                done: vec!["DONE".to_string()],
            })
        );
    }

    #[test]
    fn scan_buffer_directive_name_is_case_insensitive() {
        let content = "#+todo: TODO WIP | DONE\n";
        assert_eq!(
            scan_buffer(content),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string()],
            })
        );
    }

    #[test]
    fn scan_buffer_merges_multiple_declarations_dedup() {
        let content = indoc! {"
            #+TODO: TODO WIP | DONE
            #+TODO: TODO NEXT | DONE CANCELED
            * heading
        "};
        assert_eq!(
            scan_buffer(content),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string(), "NEXT".to_string()],
                done: vec!["DONE".to_string(), "CANCELED".to_string()],
            })
        );
    }

    #[test]
    fn scan_buffer_skips_malformed_lines() {
        let content = indoc! {"
            #+TODO: |
            #+TODO: TODO WIP | DONE
        "};
        assert_eq!(
            scan_buffer(content),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string()],
            })
        );
    }

    #[test]
    fn scan_buffer_all_malformed_returns_none() {
        let content = indoc! {"
            #+TODO: |
            #+TODO:
        "};
        assert_eq!(scan_buffer(content), None);
    }

    #[test]
    fn scan_buffer_finds_declaration_after_headline() {
        let content = indoc! {"
            * A heading
            #+TODO: TODO WIP | DONE
        "};
        assert_eq!(
            scan_buffer(content),
            Some(TodoKeywords {
                active: vec!["TODO".to_string(), "WIP".to_string()],
                done: vec!["DONE".to_string()],
            })
        );
    }
}
