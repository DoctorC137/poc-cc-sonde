/// Masks `://user:PASSWORD@host` credentials and query strings in a URL for safe logging.
/// Works for any scheme (redis, http, https, …) and both `:password@` and `user:password@` forms.
/// Query strings (`?key=value`) are replaced with `?****` to prevent token leakage.
pub(crate) fn sanitize_url_for_log(url: &str) -> String {
    // Find the authority section: after "://" and before the next "/"
    let mut result = if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..authority_end];

        // Password is present only when there is an '@' in the authority
        if let Some(at_pos) = authority.rfind('@') {
            let user_info = &authority[..at_pos];
            // Password starts after the first ':' in user_info (covers both ":pass" and "user:pass")
            if let Some(colon_pos) = user_info.find(':') {
                let password_start = scheme_end + 3 + colon_pos + 1;
                let password_end = scheme_end + 3 + at_pos;
                if password_start < password_end {
                    let mut masked = url.to_string();
                    masked.replace_range(password_start..password_end, "****");
                    masked
                } else {
                    url.to_string()
                }
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        }
    } else {
        url.to_string()
    };

    // Also mask query strings which may contain tokens or signed parameters
    if let Some(q_pos) = result.find('?') {
        result.truncate(q_pos);
        result.push_str("?****");
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_credentials() {
        assert_eq!(
            sanitize_url_for_log("redis://localhost:6379"),
            "redis://localhost:6379"
        );
    }

    #[test]
    fn test_password_only() {
        assert_eq!(
            sanitize_url_for_log("redis://:secret@localhost:6379"),
            "redis://:****@localhost:6379"
        );
    }

    #[test]
    fn test_user_and_password() {
        assert_eq!(
            sanitize_url_for_log("https://user:secret@host/path"),
            "https://user:****@host/path"
        );
    }

    #[test]
    fn test_percent_encoded_password() {
        assert_eq!(
            sanitize_url_for_log("redis://:p%40ss%3Aword@host:6379"),
            "redis://:****@host:6379"
        );
    }

    #[test]
    fn test_query_string_masked() {
        assert_eq!(
            sanitize_url_for_log("https://host/api?token=SECRET&foo=bar"),
            "https://host/api?****"
        );
    }

    #[test]
    fn test_no_query_string_unchanged() {
        assert_eq!(
            sanitize_url_for_log("https://host/api/path"),
            "https://host/api/path"
        );
    }

    #[test]
    fn test_credentials_and_query_string_both_masked() {
        assert_eq!(
            sanitize_url_for_log("https://user:pass@host/path?token=SECRET"),
            "https://user:****@host/path?****"
        );
    }
}
