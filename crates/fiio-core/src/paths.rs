//! Path handling. Files are opened by their exact on-disk path; lookups use
//! the NFC normalized form, so a playlist entry matches a file whatever
//! Unicode normalization either of them uses.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Lookup key for a path: its NFC form. `None` for non-UTF-8 paths.
pub fn key(path: &Path) -> Option<String> {
    path.to_str().map(|s| s.nfc().collect())
}

/// The form FAT32 compares a name in, as near as can be said without the
/// card: NFC, then each character upper-cased when its upper case is a
/// single character, as FAT's one-to-one upcase table does for ordinary
/// letters (so `ß` stays `ß`). Two card paths with the same key are the
/// same file on the card.
pub fn card_key(name: &str) -> String {
    name.nfc()
        .map(|c| {
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// Makes `path` absolute against `base` and removes `.` and `..` components
/// without touching the filesystem.
pub fn absolute(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_key_folds_case_unicode_included_and_normalization() {
        assert_eq!(card_key("Rock/émile.mp3"), card_key("ROCK/ÉMILE.MP3"));
        assert_eq!(
            card_key("Rock/e\u{301}mile.mp3"),
            card_key("rock/émile.mp3")
        );
        assert_eq!(card_key("Straße"), "STRAßE");
        assert_ne!(card_key("a.mp3"), card_key("b.mp3"));
    }

    #[test]
    fn nfc_and_nfd_have_the_same_key() {
        let nfd = Path::new("/m/Jo\u{301}hann.mp3");
        let nfc = Path::new("/m/J\u{f3}hann.mp3");
        assert_ne!(nfd, nfc);
        assert_eq!(key(nfd), key(nfc));
    }

    #[test]
    fn absolute_is_lexical() {
        let base = Path::new("/mnt/x/Playlists");
        assert_eq!(
            absolute(Path::new("../Music/./a.mp3"), base),
            Path::new("/mnt/x/Music/a.mp3")
        );
        assert_eq!(
            absolute(Path::new("/abs/b.mp3"), base),
            Path::new("/abs/b.mp3")
        );
    }
}
