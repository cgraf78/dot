//! Credential redaction for untrusted text that Dot persists or prints.
//!
//! A pull error, a merge hook's last line, or a provider diagnostic can
//! echo a remote URL with credentials in it: an `insteadOf` rewrite or a
//! token-bearing remote turns into `https://user:token@host/...` in Git's
//! own messages. Anything Dot keeps beyond the run (the
//! `update.last-failure` record) or shows to a terminal or an agent
//! transcript (`dot doctor`) passes through [`credentials`] first.
//!
//! Same rule as Shdeps' pull-failure record, so the two tools redact one
//! line identically: the whole URL userinfo becomes `***`. Shdeps is a
//! separate, independently released tool, so the rule is restated here
//! rather than shared through a dependency.

/// Replace the userinfo of every `scheme://userinfo@host` URL in `text`
/// with `***`, keeping the scheme, host, and the rest of the text. The
/// authority ends at the first `/`, quote, or whitespace, and its last `@`
/// separates userinfo from host, so a password containing `@` is still
/// hidden whole. A user name alone (`ssh://git@host`) is redacted too:
/// it costs nothing, and a bare token often sits in that position.
/// Text without `://` comes back unchanged.
pub fn credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (head, tail) = rest.split_at(index + 3);
        out.push_str(head);
        let end = tail
            .find(|ch: char| ch == '/' || ch == '\'' || ch == '"' || ch.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::credentials;

    #[test]
    fn userinfo_is_redacted_and_the_rest_kept() {
        assert_eq!(
            credentials("fatal: unable to access 'https://user:tok3n@github.com/o/r.git/': 403"),
            "fatal: unable to access 'https://***@github.com/o/r.git/': 403"
        );
    }

    #[test]
    fn a_token_without_a_user_is_redacted() {
        assert_eq!(
            credentials("https://ghp_abc@github.com/o/r"),
            "https://***@github.com/o/r"
        );
    }

    #[test]
    fn a_password_containing_at_is_hidden_whole() {
        assert_eq!(
            credentials("https://u:p@ss@host.example/x"),
            "https://***@host.example/x"
        );
    }

    #[test]
    fn every_url_in_the_text_is_redacted() {
        assert_eq!(
            credentials("from https://a:b@one/x to ssh://git@two:22/y"),
            "from https://***@one/x to ssh://***@two:22/y"
        );
    }

    #[test]
    fn a_url_ending_the_text_is_redacted() {
        assert_eq!(
            credentials("remote https://a:b@host"),
            "remote https://***@host"
        );
    }

    #[test]
    fn text_without_credentials_is_unchanged() {
        for text in [
            "",
            "no url here",
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "mail me at a@b.example",
            "https://host/path?next=a@b",
        ] {
            assert_eq!(credentials(text), text);
        }
    }
}
