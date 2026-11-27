use std::path::Path;

/// `path` is an HTTP(S) address rather than a file.
pub(super) fn is_http_input(path: &Path) -> bool {
    path.to_str()
        .and_then(|s| s.split_once("://"))
        .is_some_and(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        })
}
