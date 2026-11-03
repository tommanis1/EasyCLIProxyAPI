// Longest prefixes come first. Keep legacy files in their existing layout;
// a native v8 file always reads the new spelling before a legacy fallback.
const PATHS: &[(&str, &str)] = &[
    (
        "client/codex/optimize-multi-agent-v2",
        "codex/optimize-multi-agent-v2",
    ),
    (
        "client/codex/enable-apply-patch",
        "codex/enable-apply-patch",
    ),
    (
        "oauth/providers/claude/disable-claude-cloak-mode",
        "disable-claude-cloak-mode",
    ),
    ("oauth/providers/claude/claude-code", "claude-code"),
    (
        "oauth/providers/claude/header-defaults",
        "claude-header-defaults",
    ),
    (
        "oauth/providers/codex/header-defaults",
        "codex-header-defaults",
    ),
    (
        "oauth/providers/antigravity/antigravity-credits",
        "quota-exceeded/antigravity-credits",
    ),
    (
        "oauth/providers/antigravity/signature-cache-enabled",
        "antigravity-signature-cache-enabled",
    ),
    (
        "oauth/providers/antigravity/signature-bypass-strict",
        "antigravity-signature-bypass-strict",
    ),
    ("oauth/providers/aistudio/ws-auth", "ws-auth"),
    ("oauth/providers/codex", "codex"),
    ("oauth/providers/claude", "claude"),
    ("oauth/providers/antigravity", "antigravity"),
    ("oauth/providers/devin", "devin"),
    ("oauth/providers/xai", "xai"),
    ("server/host", "host"),
    ("server/port", "port"),
    ("server/trusted-proxies", "trusted-proxies"),
    ("server/discovery", "discovery"),
    ("server/tls", "tls"),
    ("server/commercial-mode", "commercial-mode"),
    ("server/github-token", "github-token"),
    ("management", "remote-management"),
    ("credentials/concurrency", "credential-concurrency"),
    ("credentials/in-flight", "credential-in-flight"),
    ("routing/force-model-prefix", "force-model-prefix"),
    ("routing/retry/request-retry", "request-retry"),
    (
        "routing/retry/max-retry-credentials",
        "max-retry-credentials",
    ),
    ("routing/retry/max-retry-interval", "max-retry-interval"),
    ("routing/cooldown/disable-cooling", "disable-cooling"),
    (
        "routing/cooldown/save-cooldown-status",
        "save-cooldown-status",
    ),
    (
        "routing/cooldown/transient-error-cooldown-seconds",
        "transient-error-cooldown-seconds",
    ),
    ("requests/proxy-url", "proxy-url"),
    ("requests/passthrough-headers", "passthrough-headers"),
    (
        "requests/nonstream-keepalive-interval",
        "nonstream-keepalive-interval",
    ),
    ("requests/streaming", "streaming"),
    ("requests/payload", "payload"),
    ("oauth/auth-dir", "auth-dir"),
    (
        "oauth/auth-auto-refresh-workers",
        "auth-auto-refresh-workers",
    ),
    ("oauth/model-alias", "oauth-model-alias"),
    ("oauth/settings", "oauth-settings"),
    ("oauth/excluded-models", "oauth-excluded-models"),
    ("oauth/request-scoped-errors", "oauth-request-scoped-errors"),
    (
        "multimedia/disable-image-generation",
        "disable-image-generation",
    ),
    (
        "multimedia/gpt-image-2-base-model",
        "gpt-image-2-base-model",
    ),
    (
        "multimedia/video-result-auth-cache-ttl",
        "video-result-auth-cache-ttl",
    ),
    ("observability/logs/debug", "debug"),
    ("observability/logs/logging-to-file", "logging-to-file"),
    (
        "observability/logs/logs-max-total-size-mb",
        "logs-max-total-size-mb",
    ),
    (
        "observability/logs/error-logs-max-files",
        "error-logs-max-files",
    ),
    ("observability/logs/request-log", "request-log"),
    (
        "observability/usage/usage-statistics-enabled",
        "usage-statistics-enabled",
    ),
    (
        "observability/usage/redis-usage-queue-retention-seconds",
        "redis-usage-queue-retention-seconds",
    ),
    ("observability/pprof", "pprof"),
];

pub(crate) fn legacy_extended_config_paths() -> Vec<(Vec<&'static str>, Vec<&'static str>)> {
    PATHS
        .iter()
        .map(|(native, legacy)| {
            (
                legacy.split('/').collect::<Vec<_>>(),
                native.split('/').collect::<Vec<_>>(),
            )
        })
        .collect()
}

// Only template-backed settings are writable. Existing GUI-owned settings and
// system metadata cannot be changed by this command, even via a parent object.
