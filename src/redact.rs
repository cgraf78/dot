//! Credential redaction for the `update.last-failure` record.
//!
//! A pull error, a merge hook's last line, or a provider diagnostic can
//! echo a remote URL with credentials in it: an `insteadOf` rewrite or a
//! token-bearing remote turns into `https://user:token@host/...` in Git's
//! own messages. The record keeps such a line beyond the run, and `dot
//! doctor` prints it to a terminal or an agent transcript, so both its
//! writer and its reader pass every field through [`credentials`]. Other
//! output (retained update logs, the overlay `remote URL drift` row) is
//! not redacted here.
//!
//! Same rule as Shdeps' pull-failure record, so the two tools redact one
//! line alike. Shdeps is a separate, independently released tool, so the
//! rule is restated here rather than shared through a dependency.

/// Schemes whose user name is an account name, never a secret
/// (`ssh://git@host`): a user-only userinfo stays visible for them.
const USER_SCHEMES: [&str; 4] = ["ssh", "git+ssh", "ssh+git", "git"];

/// Replace the userinfo of every `scheme://userinfo@host` URL in `text`
/// with `***`, keeping the scheme, host, and the rest of the text. The
/// authority ends at the first `/`, `?`, `#`, or whitespace, as Git's own
/// URL parser ends it, and its last `@` separates userinfo from host, so a
/// password containing `@` or a quote is still hidden whole. A user name
/// alone is redacted too (a bare token often sits in that position) except
/// for SSH-style schemes, whose user is an account such as `git`. Text
/// without `://` comes back unchanged.
pub fn credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (head, tail) = rest.split_at(index + 3);
        out.push_str(head);
        let scheme_start = head[..index]
            .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || "+-.".contains(ch)))
            .map_or(0, |at| at + 1);
        let scheme = head[scheme_start..index].to_ascii_lowercase();
        let end = tail
            .find(|ch: char| ch == '/' || ch == '?' || ch == '#' || ch.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at)
                if authority[..at].contains(':') || !USER_SCHEMES.contains(&scheme.as_str()) =>
            {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
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
            credentials("from https://a:b@one/x to ssh://git:pw@two:22/y"),
            "from https://***@one/x to ssh://***@two:22/y"
        );
    }

    #[test]
    fn a_password_containing_a_quote_is_hidden_whole() {
        assert_eq!(
            credentials("fatal: unable to access 'https://user:it's@host/x/': 403"),
            "fatal: unable to access 'https://***@host/x/': 403"
        );
    }

    #[test]
    fn an_ssh_user_name_is_kept() {
        for text in [
            "ssh://git@github.com/o/r.git",
            "git+ssh://git@host/r",
            "SSH://git@host:22/r",
        ] {
            assert_eq!(credentials(text), text);
        }
        // A password is still a secret there.
        assert_eq!(credentials("ssh://git:pw@host/r"), "ssh://***@host/r");
    }

    #[test]
    fn a_query_or_fragment_ends_the_host() {
        for text in [
            "https://example.com?email=me@x.example",
            "https://example.com#me@x.example",
        ] {
            assert_eq!(credentials(text), text);
        }
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
