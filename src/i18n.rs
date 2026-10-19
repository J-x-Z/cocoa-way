//! Opt-in translation of Cocoa-Way UI text, never runtime/user data.
//!
//! Call `initialize` before constructing UI. Reads never initialize, query the
//! environment, access preferences, or acquire a lock. Rebuild existing UI after
//! `set_preference`; System is resolved again when that preference is applied.

use std::borrow::Cow;
use std::sync::Once;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Language {
    System,
    English,
    SimplifiedChinese,
    TraditionalChinese,
    Japanese,
    Korean,
    German,
    French,
    Spanish,
    Portuguese,
    Russian,
}

impl Language {
    pub const ALL: [Self; 11] = [
        Self::System,
        Self::English,
        Self::SimplifiedChinese,
        Self::TraditionalChinese,
        Self::Japanese,
        Self::Korean,
        Self::German,
        Self::French,
        Self::Spanish,
        Self::Portuguese,
        Self::Russian,
    ];

    pub const fn code(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::English => "en",
            Self::SimplifiedChinese => "zh-Hans",
            Self::TraditionalChinese => "zh-Hant",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::German => "de",
            Self::French => "fr",
            Self::Spanish => "es",
            Self::Portuguese => "pt",
            Self::Russian => "ru",
        }
    }

    pub const fn native_name(self) -> &'static str {
        match self {
            Self::System => "Follow System",
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
            Self::TraditionalChinese => "繁體中文",
            Self::Japanese => "日本語",
            Self::Korean => "한국어",
            Self::German => "Deutsch",
            Self::French => "Français",
            Self::Spanish => "Español",
            Self::Portuguese => "Português",
            Self::Russian => "Русский",
        }
    }
}

static INITIALIZE: Once = Once::new();
// Both nibbles fit in one byte, so a writer cannot publish a mismatched pair.
static LANGUAGES: AtomicU8 = AtomicU8::new(Language::English as u8);

fn decode(value: u8) -> Language {
    Language::ALL
        .get(value as usize)
        .copied()
        .unwrap_or(Language::English)
}

fn publish(preference: Language, resolved: Language) {
    LANGUAGES.store((preference as u8) << 4 | resolved as u8, Ordering::Relaxed);
}

pub fn preference() -> Language {
    decode(LANGUAGES.load(Ordering::Relaxed) >> 4)
}

/// Returns English before initialization and never returns System.
pub fn current_language() -> Language {
    decode(LANGUAGES.load(Ordering::Relaxed) & 0x0f)
}

/// The development override affects only the initial in-memory selection. It is
/// never saved and never overrides a subsequent explicit preference change.
pub fn initialize() {
    INITIALIZE.call_once(|| {
        let saved = platform::read_preference();
        let override_code = std::env::var("COCOA_WAY_LANGUAGE").ok();
        let preference = initial_preference(saved.as_deref(), override_code.as_deref());
        publish(
            preference,
            resolve(preference, &platform::preferred_languages()),
        );
    });
}

pub fn set_preference(language: Language) -> Result<(), String> {
    initialize();
    let resolved = resolve(language, &platform::preferred_languages());
    platform::write_preference(language.code())?;
    publish(language, resolved);
    Ok(())
}

fn initial_preference(saved: Option<&str>, override_code: Option<&str>) -> Language {
    override_code
        .and_then(parse_language)
        .or_else(|| saved.and_then(parse_language))
        .unwrap_or(Language::System)
}

fn parse_language(code: &str) -> Option<Language> {
    let normalized = code.trim().replace('_', "-").to_ascii_lowercase();
    let mut parts = normalized.split('-');
    let primary = parts.next()?;
    Some(match primary {
        "system" | "auto" if parts.next().is_none() => Language::System,
        "en" => Language::English,
        "zh" => {
            let parts: Vec<_> = parts.collect();
            // An explicit script takes precedence over an inconsistent region.
            if parts.contains(&"hant") {
                Language::TraditionalChinese
            } else if parts.contains(&"hans") {
                Language::SimplifiedChinese
            } else if parts.iter().any(|part| matches!(*part, "tw" | "hk" | "mo")) {
                Language::TraditionalChinese
            } else {
                Language::SimplifiedChinese
            }
        }
        "ja" => Language::Japanese,
        "ko" => Language::Korean,
        "de" => Language::German,
        "fr" => Language::French,
        "es" => Language::Spanish,
        "pt" => Language::Portuguese,
        "ru" => Language::Russian,
        _ => return None,
    })
}

fn resolve(preference: Language, preferred_languages: &[String]) -> Language {
    if preference != Language::System {
        return preference;
    }
    preferred_languages
        .iter()
        .filter_map(|code| parse_language(code))
        .find(|language| *language != Language::System)
        .unwrap_or(Language::English)
}

pub fn tr(text: &str) -> Cow<'_, str> {
    tr_for(current_language(), text)
}

/// System uses the cached resolved language; unknown keys borrow the input.
pub fn tr_for(language: Language, text: &str) -> Cow<'_, str> {
    let language = if language == Language::System {
        current_language()
    } else {
        language
    };
    let translated = core_translation(language, text).or_else(|| {
        (language == Language::SimplifiedChinese)
            .then(|| chinese_translation(text))
            .flatten()
    });
    Cow::Borrowed(translated.unwrap_or(text))
}

/// Replaces supplied ASCII identifier placeholders once. Missing/invalid tokens,
/// doubled braces (including command templates), and inserted values stay literal.
/// Duplicate arguments use the first value; replacements are never re-interpreted.
pub fn tr_format(text: &str, arguments: &[(&str, &str)]) -> String {
    format_named(&tr(text), arguments)
}

fn placeholder_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn format_named(text: &str, arguments: &[(&str, &str)]) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find('{') {
        output.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        if remaining.starts_with("{{") {
            output.push_str("{{");
            remaining = &remaining[2..];
            continue;
        }
        let Some(end) = remaining.find('}') else {
            break;
        };
        let name = &remaining[1..end];
        let replacement = placeholder_name(name)
            .then(|| {
                arguments
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| *value)
            })
            .flatten();
        output.push_str(replacement.unwrap_or(&remaining[..=end]));
        remaining = &remaining[end + 1..];
    }
    output.push_str(remaining);
    output
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{ClassType, msg_send, msg_send_id};
    use objc2_foundation::NSString;

    const SUITE: &str = "com.jxz.cocoa-way";
    const KEY: &str = "language";

    fn defaults() -> Option<Retained<AnyObject>> {
        let class = AnyClass::get("NSUserDefaults")?;
        unsafe {
            let allocated = msg_send_id![class, alloc];
            msg_send_id![allocated, initWithSuiteName: &*NSString::from_str(SUITE)]
        }
    }

    pub(super) fn read_preference() -> Option<String> {
        let defaults = defaults()?;
        unsafe {
            let value: Option<Retained<AnyObject>> =
                msg_send_id![&*defaults, objectForKey: &*NSString::from_str(KEY)];
            let value = value?;
            let is_string: bool = msg_send![&*value, isKindOfClass: NSString::class()];
            if !is_string {
                return None;
            }
            let value: Retained<NSString> = Retained::cast(value);
            Some(value.to_string())
        }
    }

    pub(super) fn write_preference(code: &str) -> Result<(), String> {
        let defaults = defaults().ok_or("Language preferences are unavailable.")?;
        unsafe {
            let _: () = msg_send![&*defaults, setObject: &*NSString::from_str(code)
                forKey: &*NSString::from_str(KEY)];
            let saved: bool = msg_send![&*defaults, synchronize];
            if saved {
                Ok(())
            } else {
                Err("Could not save the interface language.".into())
            }
        }
    }

    pub(super) fn preferred_languages() -> Vec<String> {
        let Some(class) = AnyClass::get("NSLocale") else {
            return Vec::new();
        };
        unsafe {
            let languages: Retained<AnyObject> = msg_send_id![class, preferredLanguages];
            let count: usize = msg_send![&*languages, count];
            (0..count)
                .map(|index| {
                    let language: Retained<NSString> =
                        msg_send_id![&*languages, objectAtIndex: index];
                    language.to_string()
                })
                .collect()
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) fn read_preference() -> Option<String> {
        None
    }
    pub(super) fn preferred_languages() -> Vec<String> {
        Vec::new()
    }
    pub(super) fn write_preference(_: &str) -> Result<(), String> {
        Err("Language preferences require macOS NSUserDefaults.".into())
    }
}

#[path = "i18n/catalog.rs"]
mod catalog;
use catalog::{chinese_translation, core_translation};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn codes(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn placeholders(text: &str) -> Vec<&str> {
        let mut names = Vec::new();
        let mut remaining = text;
        while let Some(start) = remaining.find('{') {
            remaining = &remaining[start..];
            if remaining.starts_with("{{") {
                remaining = &remaining[2..];
                continue;
            }
            let Some(end) = remaining.find('}') else {
                break;
            };
            let name = &remaining[1..end];
            if placeholder_name(name) {
                names.push(name);
            }
            remaining = &remaining[end + 1..];
        }
        names.sort_unstable();
        names
    }

    #[test]
    fn supported_languages_have_unique_round_trippable_codes_and_names() {
        let mut seen = HashSet::new();
        for language in Language::ALL {
            assert!(seen.insert(language.code()));
            assert_eq!(parse_language(language.code()), Some(language));
            assert!(!language.native_name().is_empty());
            assert_eq!(decode(language as u8), language);
        }
        assert_eq!(Language::ALL.len(), 11);
        assert_eq!(decode(255), Language::English);
    }

    #[test]
    fn locale_scripts_regions_and_variants_are_resolved() {
        for (code, language) in [
            ("zh", Language::SimplifiedChinese),
            ("zh-CN", Language::SimplifiedChinese),
            ("zh-SG", Language::SimplifiedChinese),
            ("zh-TW", Language::TraditionalChinese),
            ("zh_HK", Language::TraditionalChinese),
            ("zh-MO", Language::TraditionalChinese),
            ("zh-Hans-TW", Language::SimplifiedChinese),
            ("ZH_hANT_CN", Language::TraditionalChinese),
            (" en-GB ", Language::English),
            ("pt-BR", Language::Portuguese),
            ("de-DE", Language::German),
            ("ja-JP", Language::Japanese),
        ] {
            assert_eq!(parse_language(code), Some(language), "{code}");
        }
        for code in [
            "",
            "  ",
            "unknown",
            "english",
            "system-en",
            "https://example.com",
            "niri",
        ] {
            assert_eq!(parse_language(code), None, "{code}");
        }
    }

    #[test]
    fn preference_and_initial_override_parsing_is_pure() {
        assert_eq!(initial_preference(None, None), Language::System);
        assert_eq!(
            initial_preference(Some("broken"), Some("broken")),
            Language::System
        );
        assert_eq!(initial_preference(Some("de"), None), Language::German);
        assert_eq!(
            initial_preference(Some("de"), Some("ja")),
            Language::Japanese
        );
        assert_eq!(
            initial_preference(Some("de"), Some("invalid")),
            Language::German
        );
        assert_eq!(
            initial_preference(Some("zh-Hans"), Some("system")),
            Language::System
        );
        // A subsequent explicit selection does not consult the initial override.
        assert_eq!(
            resolve(Language::French, &codes(&["zh-Hans"])),
            Language::French
        );
    }

    #[test]
    fn system_uses_first_supported_preferred_language_with_english_fallback() {
        assert_eq!(
            resolve(Language::System, &codes(&["it-IT", "zh-Hant", "en"])),
            Language::TraditionalChinese
        );
        assert_eq!(
            resolve(Language::System, &codes(&["system", "ko-KR"])),
            Language::Korean
        );
        assert_eq!(
            resolve(Language::System, &codes(&["en-US", "zh-CN"])),
            Language::English
        );
        assert_eq!(
            resolve(Language::System, &codes(&["it", "ar"])),
            Language::English
        );
        assert_eq!(resolve(Language::System, &[]), Language::English);
    }

    #[test]
    fn unknown_text_commands_paths_urls_and_examples_remain_borrowed_verbatim() {
        for text in [
            "",
            "My personal application",
            "Niri Desktop",
            "container",
            "auto",
            "niri",
            "niri / single-app / shell",
            "WAYLAND_DEBUG=1; RUST_LOG=info",
            "container system start",
            "docker inspect --format '{{.State.Status}}' app",
            "https://github.com/apple/container/releases/latest?language=zh-Hans",
            "/Users/example/Applications/My App",
            "ghcr.io",
            "library/ubuntu:24.04",
            "project-data",
            "runtime log: Ready {count}",
        ] {
            for language in Language::ALL {
                let result = tr_for(language, text);
                assert!(matches!(result, Cow::Borrowed(_)));
                assert_eq!(result, text, "{language:?}: {text}");
            }
            assert_eq!(tr(text), text);
        }
    }

    #[test]
    fn core_is_translated_and_extended_keys_fall_back_honestly() {
        for language in Language::ALL.into_iter().skip(2) {
            assert_ne!(tr_for(language, "Cancel"), "Cancel");
            assert_ne!(tr_for(language, "Interface language"), "Interface language");
        }
        assert_eq!(tr_for(Language::English, "Applications"), "Applications");
        assert_eq!(tr_for(Language::SimplifiedChinese, "Applications"), "应用");
        assert_eq!(
            tr_for(Language::Japanese, "Display policy"),
            "Display policy"
        );
    }

    #[test]
    fn named_formatting_is_nonrecursive_and_preserves_missing_or_malformed_tokens() {
        let arguments = [
            ("name", "用户 {count} $HOME https://example.com"),
            ("count", "2"),
            ("count", "9"),
        ];
        assert_eq!(
            format_named("{name}: {count}, {count}; {missing}", &arguments),
            "用户 {count} $HOME https://example.com: 2, 2; {missing}"
        );
        for text in [
            "{{name}}",
            "{{.Repository}}:{{.Tag}}",
            "{}",
            "{0}",
            "{bad key}",
            "{name",
            "name}",
            "{outer {name}}",
            "{名前}",
        ] {
            assert_eq!(format_named(text, &arguments), text);
        }
        assert_eq!(
            format_named("{name}{count}", &[("name", ""), ("count", "")]),
            ""
        );
        assert_eq!(
            tr_format("I18N test {value}", &[("value", "{literal}")]),
            "I18N test {literal}"
        );
    }

    #[test]
    fn translated_templates_preserve_opaque_values_and_allow_reordering() {
        let translated = tr_for(
            Language::SimplifiedChinese,
            "{runtime} application backed by {image}",
        );
        assert_eq!(
            format_named(
                &translated,
                &[
                    ("runtime", "Apple Container"),
                    ("image", "ghcr.io/user/应用:{tag}")
                ]
            ),
            "使用 ghcr.io/user/应用:{tag} 镜像的 Apple Container 应用"
        );
        let translated = tr_for(Language::SimplifiedChinese, "Socket\n{path}");
        assert_eq!(
            format_named(&translated, &[("path", "/tmp/{slot}/wayland-1")]),
            "套接字\n/tmp/{slot}/wayland-1"
        );
        assert_eq!(
            tr_for(Language::SimplifiedChinese, "{count} active"),
            "{count} 个进行中"
        );
    }

    #[test]
    fn catalog_keys_are_unique_and_every_translation_preserves_placeholder_parity() {
        let mut keys = HashSet::new();
        for (key, values) in catalog::CORE {
            assert!(keys.insert(*key), "Duplicate key: {key}");
            for value in values {
                assert!(!value.is_empty(), "Empty translation: {key}");
                assert_eq!(
                    placeholders(key),
                    placeholders(value),
                    "Placeholder mismatch: {key}"
                );
            }
        }
        for (key, value) in catalog::CHINESE {
            assert!(keys.insert(*key), "Duplicate key: {key}");
            assert!(!value.is_empty(), "Empty translation: {key}");
            assert_eq!(
                placeholders(key),
                placeholders(value),
                "Placeholder mismatch: {key}"
            );
            assert_eq!(tr_for(Language::SimplifiedChinese, key), *value);
        }
        assert!(
            keys.len() >= 200,
            "Main-interface catalog unexpectedly shrank"
        );
    }

    #[test]
    fn parent_settings_and_templates_and_model_statuses_are_covered() {
        for key in [
            "Personalize Cocoa-Way without changing your Linux applications.",
            "Interface language",
            "Follow System",
            "Configuration file",
            "Apply",
            "Language changes apply immediately to Cocoa-Way. Linux application languages and keyboard layouts are not changed.",
            "English and Simplified Chinese cover the main interface. Other languages currently translate navigation and common actions; remaining text falls back to English.",
            "{runtime} application backed by {image}",
            "{count} saved applications",
            "{count} active",
            "{count} managed application attachments",
            "Socket\n{path}",
            "Display process {pid}",
            "Instance #{id} on {display}",
            "Named display: {display}",
            "{policy} · Desktop presentation",
            "{count}s ago",
            "{count}m ago",
            "{count}h ago",
            "{count}d ago",
            "Uses the {display} display policy",
            "1 mount",
            "{count} mounts",
            "1 variable",
            "{count} variables",
            "Dedicated display: {display}",
            "0.0 fps · idle",
            "Ready",
            "Invalid profile",
            "Blocked",
            "Starting",
            "Running",
            "Stopping",
            "Exited",
            "Failed",
            "Unavailable",
            "Degraded",
            "Free",
            "Allocating",
            "Attached",
            "Closing",
            "Queued",
            "Completed",
            "Cancelled",
            "Validate profile",
            "Check runtime",
            "Check image",
            "Create container",
            "Allocate display",
            "Start Waypipe",
            "Start application",
            "Mark instance running",
        ] {
            assert_ne!(
                tr_for(Language::SimplifiedChinese, key),
                key,
                "Missing Chinese key: {key}"
            );
        }
    }
}
