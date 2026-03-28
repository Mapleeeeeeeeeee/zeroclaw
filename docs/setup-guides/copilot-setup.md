# GitHub Copilot Provider Setup

ZeroClaw supports GitHub Copilot as a provider through the Copilot Chat API.
Authentication uses GitHub's OAuth device code flow (the same mechanism used by the VS Code Copilot extension), so no manual token extraction is required.

## Prerequisites

- An active **GitHub Copilot** subscription (Individual, Business, or Enterprise).
- A GitHub account linked to the subscription.
- Internet access to `github.com` and `api.githubcopilot.com`.

## Quick Start

```bash
zeroclaw onboard \
  --provider "copilot"
```

No API key is needed during onboarding. On first use, ZeroClaw will prompt you to authenticate via the GitHub device flow (see [Authentication](#authentication) below).

If you already have a GitHub personal access token with Copilot scope, you can pass it directly:

```bash
zeroclaw onboard \
  --provider "copilot" \
  --api-key "ghp_yourGitHubToken"
```

## Manual Configuration

Edit `~/.zeroclaw/config.toml`:

```toml
default_provider = "copilot"
default_model = "gpt-4o"
default_temperature = 0.7
```

The `api_key` field is optional. If omitted, ZeroClaw will use the device flow to authenticate on first run. To skip the interactive prompt, set the key explicitly:

```toml
api_key = "ghp_yourGitHubToken"
default_provider = "copilot"
default_model = "gpt-4o"
```

## Authentication

### Device Code Flow (default)

When no token is configured, ZeroClaw runs GitHub's device code flow automatically:

1. ZeroClaw prints a one-time code and a URL (`https://github.com/login/device`).
2. Open the URL in a browser, enter the code, and authorize.
3. ZeroClaw receives an OAuth token and exchanges it for a short-lived Copilot API key.
4. Both tokens are cached to `~/.config/zeroclaw/copilot/` with owner-only permissions (`0600`).

Cached tokens are refreshed automatically when they expire. You do not need to re-authenticate unless you revoke access.

### Pre-supplied GitHub Token

Set the `GITHUB_TOKEN` environment variable or put the token in `api_key` in your config. The token must belong to an account with an active Copilot subscription. ZeroClaw will exchange it for Copilot API keys automatically.

```bash
export GITHUB_TOKEN="ghp_yourToken"
```

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `GITHUB_TOKEN` | No | GitHub personal access token. Skips device flow if set. |

No other environment variables are needed. ZeroClaw handles Copilot API key exchange internally.

## Provider Aliases

ZeroClaw recognizes these aliases for the Copilot provider:

| Alias | Canonical Name |
|-------|----------------|
| `copilot` | `copilot` |
| `github-copilot` | `copilot` |

## Proxy Support

If your network requires an HTTP proxy, configure it under the `[proxy]` section in `config.toml`. The service key for Copilot proxy settings is `provider.copilot`.

## Verify Setup

```bash
# Test agent directly
echo "Hello" | zeroclaw agent

# Check status
zeroclaw status
```

On the first run, you will see the device flow prompt if no token is cached. After authenticating, subsequent runs use the cached token.

## Troubleshooting

### "Ensure your GitHub account has an active Copilot subscription"

**Symptom:** 401 or 403 error when exchanging the GitHub token for a Copilot API key.

**Solution:**
- Verify your GitHub account has an active Copilot subscription at [github.com/settings/copilot](https://github.com/settings/copilot).
- If using `GITHUB_TOKEN`, confirm the token belongs to the subscribed account.
- ZeroClaw automatically clears the cached access token on 401/403, so the next run will re-trigger the device flow.

### Device Flow Times Out

**Symptom:** "Timed out waiting for GitHub authorization" after 15 minutes.

**Solution:**
- Ensure you opened the correct URL (`https://github.com/login/device`) and entered the code before it expired.
- Check that your browser is logged in to the correct GitHub account.
- Retry by running the command again; a new code will be generated.

### Cached Token Issues

**Symptom:** Stale or corrupted token causing repeated failures.

**Solution:**
- Delete the cached tokens:
```bash
rm -rf ~/.config/zeroclaw/copilot/
```
- Run the command again to re-authenticate.

### Network / Proxy Errors

**Symptom:** Connection timeouts or TLS errors.

**Solution:**
- Ensure `github.com` and `api.githubcopilot.com` are reachable.
- If behind a proxy, configure `[proxy]` in `config.toml`.

## Important Notes

- The Copilot Chat API is a private, undocumented GitHub API. ZeroClaw uses the same OAuth client ID and editor headers as VS Code and other third-party integrations (LiteLLM, Codex CLI, etc.). GitHub could change or revoke this at any time, which would break all third-party integrations simultaneously.
- Copilot tokens are cached with owner-only file permissions (`0600`) for security.
- ZeroClaw supports native tool calling through the Copilot provider.

## Related Documentation

- [ZeroClaw README](../README.md)
- [Providers Reference](../reference/api/providers-reference.md)
- [Custom Provider Endpoints](../contributing/custom-providers.md)
- [Contributing Guide](../../CONTRIBUTING.md)
