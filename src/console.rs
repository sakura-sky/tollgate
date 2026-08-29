// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Andrew Stevens

//! The read-only web console: a single self-contained HTML page (no external
//! assets, no build step) served by the gateway. It renders budgets, live
//! spend, and recent usage by polling the same key-authenticated JSON endpoints
//! an operator would call with curl.
//!
//! The page is intentionally observe-only. Administration at scale (multi-org,
//! RBAC/SSO, budget approvals, policy, audit export) is planned for a separate
//! Enterprise edition, not this open-source core.

/// The console HTML, embedded at compile time.
const CONSOLE_HTML: &str = include_str!("../assets/console.html");

/// Marker in the HTML replaced with a boot `<script>` that seeds page globals.
const BOOT_MARKER: &str = "<!--TOLLGATE_BOOT-->";

/// Render the console with an optional boot script injected. Pass an empty
/// string in production (the user pastes their key); the demo injects the
/// preloaded key so the page connects on load.
#[must_use]
pub fn render(boot_script: &str) -> String {
    CONSOLE_HTML.replace(BOOT_MARKER, boot_script)
}

/// Build a boot `<script>` that seeds the demo key and mode. The key is
/// JSON-encoded so it is safely quoted inside the script.
#[must_use]
pub fn demo_boot(plaintext_key: &str) -> String {
    let key_json = serde_json::to_string(plaintext_key).unwrap_or_else(|_| "\"\"".to_owned());
    format!("<script>window.__TOLLGATE__={{key:{key_json},mode:\"demo\"}};</script>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_injects_boot_script() {
        let out = render(&demo_boot("tgk_abc_def"));
        assert!(out.contains("window.__TOLLGATE__"));
        assert!(out.contains("tgk_abc_def"));
        assert!(!out.contains(BOOT_MARKER));
    }

    #[test]
    fn render_empty_leaves_no_marker_script() {
        let out = render("");
        // Marker is consumed; page falls back to live mode with no key.
        assert!(!out.contains(BOOT_MARKER));
        assert!(out.contains("Tollgate console"));
    }

    #[test]
    fn demo_boot_escapes_quotes() {
        // A key can only be hex+tag, but ensure JSON encoding is used regardless.
        let boot = demo_boot("a\"b");
        assert!(boot.contains("\"a\\\"b\""));
    }
}
