//! Persisted application settings presentation with core-owned semantics.

pub use crate::{LANGUAGE_ENGLISH, LANGUAGE_SIMPLIFIED_CHINESE, apply_locale};

#[cfg(test)]
static TEST_LOCALE_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestLocaleGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestLocaleGuard {
    pub(crate) fn acquire() -> Self {
        let lock = TEST_LOCALE_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_locale(crate::LANGUAGE_ENGLISH);
        Self { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for TestLocaleGuard {
    fn drop(&mut self) {
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }
}

/// Draws the window in Simplified Chinese, switches to English with the two
/// calls the Settings language row makes, and checks that `shown` read `key`
/// in each language. The caller holds a [`TestLocaleGuard`] and builds what it
/// tests while Simplified Chinese is current, as a restart into it would.
#[cfg(test)]
pub(crate) fn assert_follows_language_switch(
    cx: &mut gpui::VisualTestContext,
    key: &str,
    shown: impl Fn(&mut gpui::VisualTestContext) -> String,
) {
    apply_locale(Some(LANGUAGE_SIMPLIFIED_CHINESE));
    cx.update(|window, cx| {
        _ = window.draw(cx);
    });
    let chinese = crate::tr!(key).into_owned();
    assert_eq!(shown(cx), chinese, "{key} before the switch");

    cx.update(|window, cx| {
        apply_locale(Some(LANGUAGE_ENGLISH));
        window.refresh();
        _ = window.draw(cx);
    });
    let english = crate::tr!(key).into_owned();
    assert_ne!(english, chinese, "{key} has its own Chinese text");
    assert_eq!(shown(cx), english, "{key} after the switch");
}

pub use tcode_core::settings::{
    BrowserSettings, ChildApprovalMode, EnvVar, ImageMode, OrchestrateChildModel,
    OrchestrateSettings, ProjectSort, ProviderSettings, Settings, ThemeMode, provider_label,
};
/// The six accent presets offered by the provider card.
pub const ACCENT_PRESETS: [&str; 6] = [
    "#2563eb", "#16a34a", "#ea580c", "#dc2626", "#7c3aed", "#0891b2",
];

/// A localized human label for the sidebar sort button's tooltip.
pub fn project_sort_label(sort: ProjectSort) -> String {
    match sort {
        ProjectSort::RecentActivity => crate::tr!("sidebar.sort_recent").into_owned(),
        ProjectSort::NameAsc => crate::tr!("sidebar.sort_name").into_owned(),
    }
}
