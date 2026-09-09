//! Shared URI identity for scanned inputs and client requests.

use percent_encoding::percent_decode_str;
use tower_lsp::lsp_types::Url;

/// Normalize file and BIG URIs before using them as index or source-text keys.
/// VS Code percent-encodes drive colons and lowercases drive letters; scanners
/// can also receive either drive spelling through configured base roots.
pub(crate) fn canonical_uri(uri: Url) -> Url {
    if uri.scheme() == "file" {
        if let Ok(path) = uri.to_file_path() {
            if let Ok(canonical) = Url::from_file_path(path) {
                return canonical;
            }
        }
    } else if uri.scheme() == "big" {
        let Ok(mut path) = percent_decode_str(uri.path())
            .decode_utf8()
            .map(|path| path.into_owned())
        else {
            return uri;
        };
        if path.as_bytes().get(1).is_some_and(u8::is_ascii_lowercase)
            && path.as_bytes().get(2) == Some(&b':')
        {
            let drive = char::from(path.as_bytes()[1].to_ascii_uppercase()).to_string();
            path.replace_range(1..2, &drive);
        }
        let mut canonical = Url::parse("big:///").expect("static BIG URI is valid");
        canonical.set_path(&path);
        return canonical;
    }
    uri
}
