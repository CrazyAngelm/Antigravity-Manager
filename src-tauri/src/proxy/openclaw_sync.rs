//! OpenClaw auth-profiles sync.
//!
//! Automatically exports AM accounts to OpenClaw's `auth-profiles.json`,
//! enabling the `google-antigravity` provider without manual OAuth login.
//!
//! Target files:
//! - `~/.openclaw/agents/main/agent/auth-profiles.json` — OAuth credentials
//! - `~/.openclaw/openclaw.json` — plugin enable + auth profile references

use serde_json::Value;
use std::fs;
use std::path::PathBuf;

/// OpenClaw state directory name
const OPENCLAW_DIR: &str = ".openclaw";
/// Relative path from state dir to agent auth-profiles
const AUTH_PROFILES_REL: &str = "agents/main/agent";
const AUTH_PROFILES_FILENAME: &str = "auth-profiles.json";
/// Main config file
const OPENCLAW_CONFIG_FILENAME: &str = "openclaw.json";
/// Provider and plugin IDs
const PROVIDER_ID: &str = "google-antigravity";
const PLUGIN_ID: &str = "google-antigravity-auth";

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn get_openclaw_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(OPENCLAW_DIR))
}

fn get_auth_profiles_path() -> Option<PathBuf> {
    get_openclaw_dir().map(|d| d.join(AUTH_PROFILES_REL).join(AUTH_PROFILES_FILENAME))
}

fn get_openclaw_config_path() -> Option<PathBuf> {
    get_openclaw_dir().map(|d| d.join(OPENCLAW_CONFIG_FILENAME))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sync all non-disabled AM accounts into OpenClaw's auth-profiles and config.
/// This is safe to call repeatedly; it merges without data loss.
pub fn sync_openclaw_accounts() {
    if let Err(e) = sync_openclaw_accounts_inner() {
        tracing::warn!("[OpenClaw Sync] Failed: {}", e);
    }
}

fn sync_openclaw_accounts_inner() -> Result<(), String> {
    let openclaw_dir = get_openclaw_dir()
        .ok_or_else(|| "Cannot resolve home directory".to_string())?;

    // Skip if OpenClaw is not installed (no ~/.openclaw directory)
    if !openclaw_dir.exists() {
        tracing::debug!("[OpenClaw Sync] ~/.openclaw not found, skipping");
        return Ok(());
    }

    let accounts = crate::modules::account::list_accounts()
        .map_err(|e| format!("Failed to list accounts: {}", e))?;

    // Filter to enabled accounts only
    let active_accounts: Vec<_> = accounts
        .iter()
        .filter(|acc| !acc.disabled && !acc.proxy_disabled)
        .collect();

    tracing::info!(
        "[OpenClaw Sync] Syncing {} active accounts (of {} total)",
        active_accounts.len(),
        accounts.len()
    );

    // 1. Write auth-profiles.json
    sync_auth_profiles(&active_accounts)?;

    // 2. Update openclaw.json (enable plugin + add auth profile refs)
    sync_openclaw_config(&active_accounts)?;

    tracing::info!("[OpenClaw Sync] Sync complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Auth profiles file
// ---------------------------------------------------------------------------

fn sync_auth_profiles(accounts: &[&crate::models::Account]) -> Result<(), String> {
    let auth_path = get_auth_profiles_path()
        .ok_or_else(|| "Cannot resolve auth-profiles path".to_string())?;

    // Ensure directory exists
    if let Some(parent) = auth_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create auth-profiles directory: {}", e))?;
    }

    // Read existing store (preserve non-antigravity profiles)
    let mut store: Value = if auth_path.exists() {
        fs::read_to_string(&auth_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({ "version": 1, "profiles": {} }))
    } else {
        serde_json::json!({ "version": 1, "profiles": {} })
    };

    // Ensure version and profiles exist
    store["version"] = serde_json::json!(1);
    if !store.get("profiles").map_or(false, |v| v.is_object()) {
        store["profiles"] = serde_json::json!({});
    }

    let profiles = store.get_mut("profiles")
        .and_then(|p| p.as_object_mut())
        .ok_or_else(|| "Failed to access profiles object".to_string())?;

    // Remove all existing google-antigravity profiles (full replace)
    let existing_keys: Vec<String> = profiles
        .keys()
        .filter(|k| k.starts_with("google-antigravity:"))
        .cloned()
        .collect();
    for key in existing_keys {
        profiles.remove(&key);
    }

    // Build profile order list for round-robin
    let mut order: Vec<String> = Vec::new();

    // Insert new profiles from AM accounts
    for acc in accounts {
        let profile_id = format!("{}:{}", PROVIDER_ID, acc.email);
        let project_id = acc.token.project_id.clone()
            .unwrap_or_else(|| "rising-fact-p41fc".to_string());

        let credential = serde_json::json!({
            "type": "oauth",
            "provider": PROVIDER_ID,
            "access": "",
            "refresh": acc.token.refresh_token,
            "expires": 0,
            "email": acc.email,
            "projectId": project_id
        });

        profiles.insert(profile_id.clone(), credential);
        order.push(profile_id);
    }

    // Set rotation order if multiple accounts
    if order.len() > 1 {
        if !store.get("order").map_or(false, |v| v.is_object()) {
            store["order"] = serde_json::json!({});
        }
        if let Some(order_map) = store.get_mut("order").and_then(|o| o.as_object_mut()) {
            order_map.insert(PROVIDER_ID.to_string(), serde_json::json!(order));
        }
    }

    // Atomic write
    let tmp_path = auth_path.with_extension("tmp");
    fs::write(&tmp_path, serde_json::to_string_pretty(&store).unwrap())
        .map_err(|e| format!("Failed to write auth-profiles temp: {}", e))?;
    fs::rename(&tmp_path, &auth_path)
        .map_err(|e| format!("Failed to rename auth-profiles: {}", e))?;

    tracing::debug!(
        "[OpenClaw Sync] Wrote {} profiles to {:?}",
        accounts.len(),
        auth_path
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// OpenClaw config (openclaw.json)
// ---------------------------------------------------------------------------

fn sync_openclaw_config(accounts: &[&crate::models::Account]) -> Result<(), String> {
    let config_path = get_openclaw_config_path()
        .ok_or_else(|| "Cannot resolve openclaw.json path".to_string())?;

    if !config_path.exists() {
        tracing::debug!("[OpenClaw Sync] openclaw.json not found, skipping config sync");
        return Ok(());
    }

    let mut config: Value = fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let mut changed = false;

    // 1. Enable the google-antigravity-auth plugin
    changed |= ensure_plugin_enabled(&mut config);

    // 2. Add auth profile references for each account
    changed |= ensure_auth_profile_refs(&mut config, accounts);

    if !changed {
        tracing::debug!("[OpenClaw Sync] openclaw.json already up to date");
        return Ok(());
    }

    // Atomic write
    let tmp_path = config_path.with_extension("tmp");
    fs::write(&tmp_path, serde_json::to_string_pretty(&config).unwrap())
        .map_err(|e| format!("Failed to write openclaw.json temp: {}", e))?;
    fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("Failed to rename openclaw.json: {}", e))?;

    tracing::debug!("[OpenClaw Sync] Updated openclaw.json");
    Ok(())
}

/// Ensure `plugins.entries.google-antigravity-auth.enabled = true`
fn ensure_plugin_enabled(config: &mut Value) -> bool {
    let plugins = config
        .as_object_mut()
        .and_then(|c| {
            if !c.contains_key("plugins") {
                c.insert("plugins".to_string(), serde_json::json!({}));
            }
            c.get_mut("plugins").and_then(|p| p.as_object_mut())
        });
    let plugins = match plugins {
        Some(p) => p,
        None => return false,
    };

    if !plugins.contains_key("entries") {
        plugins.insert("entries".to_string(), serde_json::json!({}));
    }
    let entries = match plugins.get_mut("entries").and_then(|e| e.as_object_mut()) {
        Some(e) => e,
        None => return false,
    };

    let already_enabled = entries
        .get(PLUGIN_ID)
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if already_enabled {
        return false;
    }

    entries.insert(
        PLUGIN_ID.to_string(),
        serde_json::json!({ "enabled": true }),
    );
    true
}

/// Ensure `auth.profiles` contains references for each AM account
fn ensure_auth_profile_refs(
    config: &mut Value,
    accounts: &[&crate::models::Account],
) -> bool {
    let config_obj = match config.as_object_mut() {
        Some(c) => c,
        None => return false,
    };

    if !config_obj.contains_key("auth") {
        config_obj.insert("auth".to_string(), serde_json::json!({}));
    }
    let auth = match config_obj.get_mut("auth").and_then(|a| a.as_object_mut()) {
        Some(a) => a,
        None => return false,
    };

    if !auth.contains_key("profiles") {
        auth.insert("profiles".to_string(), serde_json::json!({}));
    }
    let profiles = match auth.get_mut("profiles").and_then(|p| p.as_object_mut()) {
        Some(p) => p,
        None => return false,
    };

    let mut changed = false;

    // Remove stale antigravity profile refs (accounts no longer in AM)
    let am_profile_ids: std::collections::HashSet<String> = accounts
        .iter()
        .map(|acc| format!("{}:{}", PROVIDER_ID, acc.email))
        .collect();
    let existing_ag_keys: Vec<String> = profiles
        .keys()
        .filter(|k| k.starts_with("google-antigravity:"))
        .cloned()
        .collect();
    for key in &existing_ag_keys {
        if !am_profile_ids.contains(key) {
            profiles.remove(key);
            changed = true;
        }
    }

    // Add missing profile refs
    for acc in accounts {
        let profile_id = format!("{}:{}", PROVIDER_ID, acc.email);
        if !profiles.contains_key(&profile_id) {
            profiles.insert(
                profile_id,
                serde_json::json!({
                    "provider": PROVIDER_ID,
                    "mode": "oauth"
                }),
            );
            changed = true;
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_plugin_enabled_on_empty_config() {
        let mut config = serde_json::json!({});
        let changed = ensure_plugin_enabled(&mut config);
        assert!(changed);
        let enabled = config["plugins"]["entries"][PLUGIN_ID]["enabled"].as_bool();
        assert_eq!(enabled, Some(true));
    }

    #[test]
    fn test_ensure_plugin_enabled_already_enabled() {
        let mut config = serde_json::json!({
            "plugins": {
                "entries": {
                    "google-antigravity-auth": { "enabled": true }
                }
            }
        });
        let changed = ensure_plugin_enabled(&mut config);
        assert!(!changed, "should not change if already enabled");
    }

    #[test]
    fn test_ensure_plugin_enabled_preserves_other_plugins() {
        let mut config = serde_json::json!({
            "plugins": {
                "entries": {
                    "telegram": { "enabled": true }
                }
            }
        });
        let changed = ensure_plugin_enabled(&mut config);
        assert!(changed);
        assert_eq!(
            config["plugins"]["entries"]["telegram"]["enabled"].as_bool(),
            Some(true),
            "existing plugins should be preserved"
        );
        assert_eq!(
            config["plugins"]["entries"][PLUGIN_ID]["enabled"].as_bool(),
            Some(true),
        );
    }
}
