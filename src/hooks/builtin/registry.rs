//! Builtin hook registration dispatcher.
//!
//! Two registration patterns coexist here:
//!
//! - `command_logger` and `webhook_audit` read their config from typed fields
//!   directly on `BuiltinHooksConfig` (these are inherited from upstream).
//! - Fork-specific custom hooks (`water_reminder`, `release_monitor`,
//!   `news_monitor`, `email_monitor`) read from
//!   `BuiltinHooksConfig.extra` via `#[serde(flatten)]`. This keeps custom
//!   hooks' config schema out of `src/config/schema.rs`, reducing merge
//!   conflicts when syncing upstream.
//!
//! Adding a new custom hook: create the file under `src/hooks/builtin/`,
//! define a `pub(super) const SECTION_NAME`, implement `HookHandler`, write
//! a `pub fn register(...)` that reads from `extra`, and add one line to
//! `register_builtin_hooks` below.

use std::sync::Arc;

use crate::config::schema::Config;
use crate::hooks::HookRunner;
use crate::providers::Provider;

/// The canonical list of recognised custom-hook section names.
///
/// This is the single source of truth used both by `register_builtin_hooks`
/// (to warn on unknown keys) and by the unit test below (to verify that every
/// hook module's `SECTION_NAME` constant is represented here).
///
/// **Maintenance rule**: when you add a new hook module, add its section name
/// string here AND add a corresponding `assert!(KNOWN_SECTIONS.contains(…))`
/// line in the test below.  The test will fail until both are done, which
/// prevents typo warnings from firing in production for your new hook.
pub(super) const KNOWN_SECTIONS: &[&str] = &[
    "water_reminder",
    "release_monitor",
    "news_monitor",
    "email_monitor",
];

pub fn register_builtin_hooks(
    runner: &mut HookRunner,
    config: &Config,
    provider: &Arc<dyn Provider>,
    model: &str,
) {
    super::command_logger::register(runner, config, provider, model);
    super::webhook_audit::register(runner, config, provider, model);
    super::water_reminder::register(runner, config, provider, model);
    super::release_monitor::register(runner, config, provider, model);
    super::news_monitor::register(runner, config, provider, model);
    super::email_monitor::register(runner, config, provider, model);

    for key in config.hooks.builtin.extra.keys() {
        if !KNOWN_SECTIONS.contains(&key.as_str()) {
            tracing::warn!("hooks.builtin.{key}: unrecognised section (typo?) — ignored");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KNOWN_SECTIONS;

    /// Verifies that every hook module's `SECTION_NAME` constant is present in
    /// `KNOWN_SECTIONS`.
    ///
    /// This test acts as a **reminder mechanism**: if a developer renames a
    /// `SECTION_NAME` constant or adds a new hook but forgets to update
    /// `KNOWN_SECTIONS`, this test will break.  Fix it by updating both
    /// `KNOWN_SECTIONS` and the assertions here.
    #[test]
    fn known_sections_match_hook_constants() {
        assert!(
            KNOWN_SECTIONS.contains(&super::super::water_reminder::SECTION_NAME),
            "KNOWN_SECTIONS must include water_reminder::SECTION_NAME"
        );
        assert!(
            KNOWN_SECTIONS.contains(&super::super::release_monitor::SECTION_NAME),
            "KNOWN_SECTIONS must include release_monitor::SECTION_NAME"
        );
        assert!(
            KNOWN_SECTIONS.contains(&super::super::news_monitor::SECTION_NAME),
            "KNOWN_SECTIONS must include news_monitor::SECTION_NAME"
        );
        assert!(
            KNOWN_SECTIONS.contains(&super::super::email_monitor::SECTION_NAME),
            "KNOWN_SECTIONS must include email_monitor::SECTION_NAME"
        );
        assert_eq!(
            KNOWN_SECTIONS.len(),
            4,
            "update KNOWN_SECTIONS and this test when adding new custom hooks"
        );
    }
}
