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

    let known: &[&str] = &[
        super::water_reminder::SECTION_NAME,
        super::release_monitor::SECTION_NAME,
        super::news_monitor::SECTION_NAME,
        super::email_monitor::SECTION_NAME,
    ];
    for key in config.hooks.builtin.extra.keys() {
        if !known.contains(&key.as_str()) {
            tracing::warn!("hooks.builtin.{key}: unrecognised section (typo?) — ignored");
        }
    }
}
