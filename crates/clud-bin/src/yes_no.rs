//! The one format for a typed yes/no question and its answer.
//!
//! Every line-read confirmation renders as `Question? [y/N]: ` and the user
//! types the answer on that same line. The capital letter is the default an
//! empty answer takes. Build prompts with [`question`] and read answers with
//! [`answer`]; do not spell out `[y/N]` or match on `"y" | "yes"` by hand.
//! Key-driven selectors (`crate::selector`) are a different kind of prompt.

/// What an empty answer means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Default {
    Yes,
    No,
}

/// `text [y/N]: `, ready to print without a newline before reading the answer.
pub fn question(text: &str, default: Default) -> String {
    let choices = match default {
        Default::Yes => "[Y/n]",
        Default::No => "[y/N]",
    };
    format!("{text} {choices}: ")
}

/// Whether a typed line means yes. `y`/`yes` and `n`/`no` in any case; an
/// empty line is `default`; anything else is no, so a typo never consents.
pub fn answer(line: &str, default: Default) -> bool {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "" => default == Default::Yes,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_ends_in_choices_colon_and_a_space() {
        assert_eq!(question("Go?", Default::No), "Go? [y/N]: ");
        assert_eq!(question("Go?", Default::Yes), "Go? [Y/n]: ");
    }

    #[test]
    fn answers_are_case_insensitive_and_typos_refuse() {
        for yes in ["y", "Y", "yes", "YES", " Yes \n"] {
            assert!(answer(yes, Default::No), "{yes:?}");
        }
        for no in ["n", "no", "NO", "yep", "x", "ye s"] {
            assert!(!answer(no, Default::Yes), "{no:?}");
        }
    }

    #[test]
    fn empty_answer_takes_the_default() {
        assert!(!answer("\n", Default::No));
        assert!(answer("\n", Default::Yes));
    }
}
