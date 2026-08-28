use std::borrow::Cow;

pub fn tokenize(s: &str) -> Vec<Cow<'_, str>> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| Cow::Owned(token.to_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    fn tokens(input: &str) -> Vec<String> {
        tokenize(input).into_iter().map(|token| token.into_owned()).collect()
    }

    #[test]
    fn tokenizes_punctuation_and_lowercases() {
        assert_eq!(tokens("Hello, world!"), vec!["hello", "world"]);
    }

    #[test]
    fn tokenizes_tabs_and_newlines() {
        assert_eq!(
            tokens("Rust\tprogramming\nlanguage"),
            vec!["rust", "programming", "language"]
        );
    }

    #[test]
    fn filters_multiple_spaces() {
        assert_eq!(tokens("  multiple   spaces  "), vec!["multiple", "spaces"]);
    }

    #[test]
    fn empty_input_has_no_tokens() {
        assert_eq!(tokens(""), Vec::<String>::new());
    }

    #[test]
    fn punctuation_only_has_no_tokens() {
        assert_eq!(tokens("!@#$%"), Vec::<String>::new());
    }

    #[test]
    fn underscores_and_hyphens_are_separators() {
        assert_eq!(tokens("alpha-beta_gamma"), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn digits_are_alphanumeric() {
        assert_eq!(tokens("123abc"), vec!["123abc"]);
    }
}
