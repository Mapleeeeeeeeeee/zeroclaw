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
}
