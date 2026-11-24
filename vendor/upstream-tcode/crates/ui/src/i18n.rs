//! Translation and locale selection for tcode.

use std::{borrow::Cow, cell::RefCell};

rust_i18n::i18n!("../../locales", fallback = "en");

// `rust_i18n::i18n!` loads the locale directory during macro expansion, but
// changes inside that directory are not reliably tracked by Cargo for normal
// (non-test) incremental builds. These anonymous includes make both locale
// files explicit compiler inputs, so adding a key always rebuilds the embedded
// translation table used by the application.
const _: &str = include_str!("../../../locales/en.yml");
const _: &str = include_str!("../../../locales/zh-CN.yml");

pub const LANGUAGE_ENGLISH: &str = "en";
pub const LANGUAGE_SIMPLIFIED_CHINESE: &str = "zh-CN";

thread_local! {
    /// Mobile supplies the user's first configured language through its native
    /// OS API. Desktop leaves this empty and keeps using `sys_locale`.
    static PLATFORM_SYSTEM_LOCALE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Resolve a persisted override, falling back to the supplied system locale.
pub fn resolve_locale(override_locale: Option<&str>, system_locale: Option<&str>) -> &'static str {
    match override_locale {
        Some(LANGUAGE_ENGLISH) => LANGUAGE_ENGLISH,
        Some(LANGUAGE_SIMPLIFIED_CHINESE) => LANGUAGE_SIMPLIFIED_CHINESE,
        _ if system_locale.is_some_and(|locale| locale.to_ascii_lowercase().starts_with("zh")) => {
            LANGUAGE_SIMPLIFIED_CHINESE
        }
        _ => LANGUAGE_ENGLISH,
    }
}

/// Resolve and apply the requested locale, returning the selected locale.
pub fn apply_locale(override_locale: Option<&str>) -> &'static str {
    let system_locale = system_locale();
    let locale = resolve_locale(override_locale, system_locale.as_deref());
    set_locale(locale);
    locale
}

pub(crate) fn set_platform_system_locale(locale: Option<&str>) {
    PLATFORM_SYSTEM_LOCALE.with(|system_locale| {
        *system_locale.borrow_mut() = locale.map(str::to_owned);
    });
}

fn system_locale() -> Option<String> {
    PLATFORM_SYSTEM_LOCALE
        .with(|locale| locale.borrow().clone())
        .or_else(sys_locale::get_locale)
}

/// Set the process-global translation locale.
pub fn set_locale(locale: &str) {
    rust_i18n::set_locale(locale);
}

/// Translate a key in the current locale, returning the key when it is missing.
#[doc(hidden)]
pub fn translate(key: impl AsRef<str>) -> Cow<'static, str> {
    let key = key.as_ref();
    let locale = rust_i18n::locale();
    _rust_i18n_try_translate(locale.as_ref(), key).unwrap_or_else(|| Cow::Owned(key.to_owned()))
}

/// Translate a key and replace named `%{name}` patterns with formatted values.
#[doc(hidden)]
pub fn translate_with_args(
    key: impl AsRef<str>,
    names: &[&str],
    values: &[String],
) -> Cow<'static, str> {
    let translated = translate(key);
    Cow::Owned(rust_i18n::replace_patterns(&translated, names, values))
}

/// Translate English text that reaches the UI as data (a provider option's
/// description, a reason). `key`, or a numbered variant `key.1`, `key.2`, …
/// when several sources share one key, is used only while en.yml holds exactly
/// `english` there; otherwise `english` is shown as it is. Providers share
/// descriptor ids and values, so the English text is what tells them apart.
pub(crate) fn translate_english(key: &str, english: &str) -> Cow<'static, str> {
    let locale = rust_i18n::locale();
    std::iter::once(key.to_owned())
        .chain((1..).map(|variant| format!("{key}.{variant}")))
        .enumerate()
        .map_while(|(index, key)| {
            let source = _rust_i18n_try_translate(LANGUAGE_ENGLISH, &key);
            (index == 0 || source.is_some()).then_some((key, source))
        })
        .find(|(_, source)| source.as_deref() == Some(english))
        .and_then(|(key, _)| _rust_i18n_try_translate(locale.as_ref(), &key))
        .unwrap_or_else(|| Cow::Owned(english.to_owned()))
}

pub(crate) fn translate_permission_description(key: &str, english: &str) -> String {
    let translated = translate_english(key, english).into_owned();
    if translated != english {
        return translated;
    }
    let (profile, settings) = if let Some(rest) = english.strip_prefix("Profile ")
        && let Some((profile, settings)) = rest.rsplit_once(": ")
    {
        (Some(profile), settings)
    } else {
        (None, english)
    };
    let Some(settings) = settings.strip_suffix('.') else {
        return translated;
    };
    let fragments: Vec<_> = settings.split(", ").collect();
    if fragments.len() != 3 {
        return translated;
    }
    if fragments
        .iter()
        .zip(["sandbox", "approval", "reviewer"])
        .any(|(fragment, category)| {
            !(1..)
                .map_while(|variant| {
                    _rust_i18n_try_translate(
                        LANGUAGE_ENGLISH,
                        format!("permission.current.{category}.{variant}"),
                    )
                })
                .any(|source| source == *fragment)
        })
    {
        return translated;
    }
    let sandbox = translate_english("permission.current.sandbox", fragments[0]);
    let approval = translate_english("permission.current.approval", fragments[1]);
    let reviewer = translate_english("permission.current.reviewer", fragments[2]);
    let sentence = match profile {
        Some(profile) => crate::tr!(
            "permission.current.profile",
            profile = profile,
            sandbox = sandbox,
            approval = approval,
            reviewer = reviewer
        ),
        None => crate::tr!(
            "permission.current.summary",
            sandbox = sandbox,
            approval = approval,
            reviewer = reviewer
        ),
    };
    sentence.into_owned()
}

/// Translate a key using tcode-ui's embedded locale backend.
#[macro_export]
macro_rules! tr {
    ($key:expr $(,)?) => {{
        $crate::translate($key)
    }};
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        $crate::translate_with_args(
            $key,
            &[$(stringify!($name)),+],
            &[$(format!("{}", $value)),+],
        )
    }};
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn keys(yaml: &str) -> BTreeSet<String> {
        let mut stack: Vec<(usize, String)> = Vec::new();
        let mut keys = BTreeSet::new();
        for line in yaml
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let indent = line.len() - line.trim_start().len();
            let Some((name, value)) = line.trim().split_once(':') else {
                continue;
            };
            while stack.last().is_some_and(|(level, _)| *level >= indent) {
                stack.pop();
            }
            let mut path = stack
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>();
            path.push(name.trim());
            if value.trim().is_empty() {
                stack.push((indent, name.trim().to_owned()));
            } else {
                keys.insert(path.join("."));
            }
        }
        keys
    }

    #[test]
    fn locale_keys_match() {
        let en = keys(include_str!("../../../locales/en.yml"));
        let zh = keys(include_str!("../../../locales/zh-CN.yml"));
        assert_eq!(en, zh, "English and zh-CN locale keys differ");
    }

    #[test]
    fn published_permission_text_is_translated_by_its_english() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        set_locale(LANGUAGE_SIMPLIFIED_CHINESE);
        let mut sources = std::collections::BTreeMap::new();
        for provider in agent::ProviderKind::NATIVE {
            if let Some(notice) = agent::permission_notice(provider) {
                assert_ne!(translate_english("permission.notice", notice), notice);
            }
            let Some(agent::OptionDescriptor::Select {
                id, label, options, ..
            }) = agent::permission_control(provider)
            else {
                continue;
            };
            assert_ne!(
                translate_english(&format!("permission.controls.{id}"), &label),
                label
            );
            for option in options {
                let description = option.description.unwrap();
                let key = format!("permission.values.{id}.{}", option.value);
                let translated = translate_english(&key, &description).into_owned();
                assert_ne!(translated, description, "{provider:?} {}", option.value);
                let source = sources.entry(translated).or_insert(description.clone());
                assert_eq!(*source, description, "{provider:?} {}", option.value);
            }
        }
        for (key, english) in [
            ("permission.values.permissions.profile:team", "Team sandbox"),
            ("permission.values.permissions.ask", "Reworded upstream."),
            ("permission.unavailable", "Claude started in plan instead"),
        ] {
            assert_eq!(translate_english(key, english), english);
        }
        assert_eq!(
            translate_english(
                "permission.unavailable",
                "Not available: the session started in another mode"
            ),
            "不可用：会话以另一种模式启动"
        );
        assert_eq!(
            translate_english("permission.current.label", "Current permissions"),
            "当前权限"
        );
        for (english, expected) in [
            (
                "Profile my-team: workspace sandbox, asks on request, reviewed by you.",
                "配置 my-team：工作区沙箱，按请求询问，由你审批。",
            ),
            (
                "external sandbox, uses granular approval rules, reviewed by auto-review.",
                "外部沙箱，使用细分审批规则，由自动审查员审批。",
            ),
            (
                "read-only sandbox, asks for untrusted actions, reviewed by you.",
                "只读沙箱，对不受信任的操作询问，由你审批。",
            ),
            (
                "full access, never asks for approval, reviewed by auto-review.",
                "完全访问，从不请求审批，由自动审查员审批。",
            ),
        ] {
            assert_eq!(
                translate_permission_description(
                    "permission.values.permissions.effective",
                    english
                ),
                expected
            );
        }
        set_locale(LANGUAGE_ENGLISH);
    }

    #[test]
    fn explicit_overrides_win() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        for (platform, requested, expected) in [
            ("zh-TW", Some(LANGUAGE_ENGLISH), LANGUAGE_ENGLISH),
            (
                "en-US",
                Some(LANGUAGE_SIMPLIFIED_CHINESE),
                LANGUAGE_SIMPLIFIED_CHINESE,
            ),
            ("zh-Hans-CN", None, LANGUAGE_SIMPLIFIED_CHINESE),
            ("en-US", Some("unsupported"), LANGUAGE_ENGLISH),
        ] {
            set_platform_system_locale(Some(platform));
            assert_eq!(
                apply_locale(requested),
                expected,
                "{platform} / {requested:?}"
            );
            set_platform_system_locale(None);
        }
        set_locale(LANGUAGE_ENGLISH);
    }
}
