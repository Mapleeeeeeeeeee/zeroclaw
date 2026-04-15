pub mod command_logger;
pub mod webhook_audit;

pub use command_logger::CommandLoggerHook;
pub use webhook_audit::WebhookAuditHook;
pub mod email_monitor;
pub(super) mod env_loader;
pub mod news_monitor;
pub mod registry;
pub mod release_monitor;
pub(super) mod taiwan_calendar;
pub mod water_reminder;

/// Look up a custom hook section from `BuiltinHooksConfig.extra`, deserialise
/// it into `$cfg_ty`, and bail out early if the section is missing or
/// `enabled` is false.  On successful parse the identifier `$cfg` is bound
/// in the calling scope.
///
/// Usage:
/// ```ignore
/// crate::extra_hook_lookup!(config.hooks.builtin.extra, SECTION_NAME, MyHookConfig, cfg);
/// // `cfg: MyHookConfig` is now in scope, and enabled == true is guaranteed.
/// ```
#[macro_export]
macro_rules! extra_hook_lookup {
    ($extra:expr, $section:expr, $cfg_ty:ty, $cfg:ident) => {
        let Some(value) = $extra.get($section) else {
            return;
        };
        let $cfg: $cfg_ty = match value.clone().try_into() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("{}: invalid config: {e}", $section);
                return;
            }
        };
        if !$cfg.enabled {
            return;
        }
    };
}
