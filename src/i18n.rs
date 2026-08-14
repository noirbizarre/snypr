//! Internationalization (i18n) — Fluent-backed message catalogs.
//!
//! Translations live as `.ftl` files under `i18n/<lang>/snypr.ftl` and are
//! embedded into the binary by [`rust_embed`]. The active language is selected
//! once at process start via [`init`]; thereafter every call to [`fl!`] resolves
//! through the global [`LANGUAGE_LOADER`].
//!
//! Precedence applied by [`init`]:
//!
//! 1. Explicit override (`--lang BCP47` on the CLI, or `language` in the
//!    TOML config).
//! 2. The desktop / POSIX environment (`LC_ALL`, `LC_MESSAGES`, `LANG`).
//! 3. The compile-time fallback declared in `i18n.toml` (currently `en`).
//!
//! Missing keys in a non-fallback bundle fall back to English instead of
//! panicking, matching standard `i18n-embed` semantics.
//!
//! Re-export [`fl`] from the crate root (`crate::i18n::fl!`) so call sites
//! read naturally:
//!
//! ```ignore
//! use crate::i18n::fl;
//! button.set_tooltip_text(Some(&fl!("toolbar-undo-tooltip")));
//! ```

use i18n_embed::DesktopLanguageRequester;
use i18n_embed::LanguageLoader;
use i18n_embed::fluent::{FluentLanguageLoader, fluent_language_loader};
use once_cell::sync::Lazy;
use rust_embed::RustEmbed;
use unic_langid::LanguageIdentifier;

/// Embedded message catalogs. Every `i18n/<lang>/snypr.ftl` is baked into
/// the binary at compile time.
#[derive(RustEmbed)]
#[folder = "i18n/"]
pub struct Localizations;

/// Process-wide Fluent loader. Initialised lazily with the fallback language
/// (English) so calls to [`fl!`] before [`init`] still produce sane output
/// during early startup.
pub static LANGUAGE_LOADER: Lazy<FluentLanguageLoader> = Lazy::new(|| {
    let loader: FluentLanguageLoader = fluent_language_loader!();
    // Best-effort: load the fallback so callers before `init` get English
    // strings instead of raw keys. Failure here means the embedded catalog
    // is broken at build time, which the compile-time `fl!` checks should
    // already have caught.
    if let Err(err) = loader.load_fallback_language(&Localizations) {
        tracing::warn!(error = ?err, "failed to load fallback i18n bundle");
    }
    loader
});

/// Initialise the active language.
///
/// `explicit` wins over every other source when it parses as a BCP-47 tag.
/// Otherwise the standard desktop requester reads `LC_ALL` / `LC_MESSAGES` /
/// `LANG`. Unknown / unparseable tags fall back to the compile-time fallback.
pub fn init(explicit: Option<&str>) {
    let requested = match explicit.and_then(parse_lang) {
        Some(lang) => vec![lang],
        None => DesktopLanguageRequester::requested_languages(),
    };
    if let Err(err) = i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, &requested) {
        tracing::warn!(error = ?err, "i18n locale selection failed; using fallback");
    }
}

fn parse_lang(s: &str) -> Option<LanguageIdentifier> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<LanguageIdentifier>() {
        Ok(lang) => Some(lang),
        Err(err) => {
            tracing::warn!(value = trimmed, error = ?err, "ignoring invalid language tag");
            None
        }
    }
}

/// Re-export [`i18n_embed_fl::fl`] so call sites can `use crate::i18n::fl;`
/// without depending on `i18n-embed-fl` directly.
pub use i18n_embed_fl::fl as _fl;

#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        $crate::i18n::_fl!($crate::i18n::LANGUAGE_LOADER, $message_id)
    }};
    ($message_id:literal, $($args:tt)*) => {{
        $crate::i18n::_fl!($crate::i18n::LANGUAGE_LOADER, $message_id, $($args)*)
    }};
}

pub use crate::fl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_loads_english() {
        // Force English explicitly; the loader's `language()` reports the
        // best-match against the embedded catalogs.
        let en: LanguageIdentifier = "en".parse().unwrap();
        i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, &[en]).unwrap();
        let msg = fl!("notify-copied");
        assert_eq!(msg, "Screenshot copied to clipboard");
    }

    #[test]
    fn french_catalog_resolves() {
        let fr: LanguageIdentifier = "fr".parse().unwrap();
        i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, &[fr]).unwrap();
        let msg = fl!("notify-copied");
        assert!(
            msg.contains("presse-papiers"),
            "expected French rendering, got: {msg}"
        );
        // Restore fallback so other tests are not affected.
        let en: LanguageIdentifier = "en".parse().unwrap();
        i18n_embed::select(&*LANGUAGE_LOADER, &Localizations, &[en]).unwrap();
    }

    #[test]
    fn invalid_lang_tag_is_ignored() {
        assert!(parse_lang("not a tag!!").is_none());
        assert!(parse_lang("").is_none());
        assert!(parse_lang("fr-FR").is_some());
    }
}
