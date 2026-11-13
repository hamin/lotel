//! Self-hosted static assets baked into the binary: the latin woff2 faces
//! the skin's @font-face rules point at (/assets/fonts/<file>).
//!
//! The bytes are embedded with `include_bytes!` so lotel stays a single
//! file — nothing reads a directory at runtime. Font bytes arrive from
//! Google's CSS API (latin subset, SIL OFL; texts under `assets/fonts/
//! LICENSES/`) via `scripts/gen-skin.py`'s FACES table — the names here and
//! the URLs there must match, and the smoke/boot check is that a page's
//! text still renders in its chosen pairing.

use topcoat::{
    Result,
    context::Cx,
    router::{Body, path_param, response::Response, route},
};

path_param!(file);

/// Every font served under /assets/fonts/, by file name. One entry per
/// @font-face gen-skin.py emits: variable families (Recursive, Figtree,
/// Spline Sans Mono, Onest) ship one file across a weight range; DM Mono is
/// static, so one file per weight.
const FONTS: &[(&str, &[u8])] = &[
    (
        "recursive-latin.woff2",
        include_bytes!("../assets/fonts/recursive-latin.woff2"),
    ),
    (
        "figtree-latin.woff2",
        include_bytes!("../assets/fonts/figtree-latin.woff2"),
    ),
    (
        "spline-sans-mono-latin.woff2",
        include_bytes!("../assets/fonts/spline-sans-mono-latin.woff2"),
    ),
    (
        "onest-latin.woff2",
        include_bytes!("../assets/fonts/onest-latin.woff2"),
    ),
    (
        "dm-mono-400-latin.woff2",
        include_bytes!("../assets/fonts/dm-mono-400-latin.woff2"),
    ),
    (
        "dm-mono-500-latin.woff2",
        include_bytes!("../assets/fonts/dm-mono-500-latin.woff2"),
    ),
];

/// Resolve a file name to its embedded bytes; unknown names return None.
fn font_bytes(name: &str) -> Option<&'static [u8]> {
    FONTS.iter().find(|(n, _)| *n == name).map(|(_, bytes)| *bytes)
}

/// GET /assets/fonts/<file>: the embedded font bytes, cached forever (the
/// file name is the content's identity — a new build ships a new name or
/// the same name with a fresh binary, and the binary itself is versioned).
#[route(GET "./fonts/{file}")]
async fn font(cx: &Cx) -> Result<Response> {
    let built = match font_bytes(file(cx)) {
        Some(bytes) => Response::builder()
            .header("Content-Type", "font/woff2")
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .body(Body::from(bytes)),
        None => Response::builder()
            .status(404)
            .body(Body::from("no such font")),
    };
    Ok(built?)
}

#[cfg(test)]
mod tests {
    use super::{FONTS, font_bytes};

    /// A woff2 face, fingerprinted by its magic so a wrong byte order or a
    /// stubbed-out file is caught by a unit test, not by blank text on screen.
    fn is_woff2(bytes: &[u8]) -> bool {
        bytes.starts_with(b"wOF2")
    }

    #[test]
    fn known_files_resolve_and_are_real_woff2() {
        for (name, _) in FONTS {
            let bytes = font_bytes(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(is_woff2(bytes), "{name} is not a woff2");
            assert!(bytes.len() > 1024, "{name} looks truncated");
        }
    }

    #[test]
    fn unknown_and_traversal_names_resolve_to_nothing() {
        assert!(font_bytes("nope.woff2").is_none());
        assert!(font_bytes("").is_none());
        assert!(font_bytes("../secret.woff2").is_none());
        assert!(font_bytes("recursive-latin.ttf").is_none());
    }

    #[test]
    fn names_are_unique() {
        for (i, (a, _)) in FONTS.iter().enumerate() {
            assert!(
                FONTS[i + 1..].iter().all(|(b, _)| a != b),
                "duplicate font name {a}"
            );
        }
    }
}
