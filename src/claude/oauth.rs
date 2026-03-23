//! OAuth PKCE authorization code flow for Claude AI.
//!
//! Generates a PKCE code_verifier/code_challenge, builds the authorization URL,
//! and exchanges the authorization code for tokens via the token endpoint.
//! Uses `reqwest` with headers matching the Claude CLI (reverse-engineered via mitmproxy).

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::error::AppError;

// --- OAuth constants ---

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// PKCE code verifier length (RFC 7636 recommends 43-128).
const CODE_VERIFIER_LEN: usize = 64;

/// Characters allowed in a PKCE code_verifier (RFC 7636 Section 4.1).
const PKCE_CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Maximum retry attempts for token exchange (429 rate limiting).
const MAX_TOKEN_RETRIES: u32 = 5;

/// Base delay between retries in seconds (exponential: 5, 10, 20, 40, 80).
const RETRY_BASE_DELAY_SECS: u64 = 5;

/// PKCE code verifier + challenge pair, plus OAuth state parameter.
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    /// Random state for CSRF protection (also used in callback URL).
    pub state: String,
}

/// Generate a PKCE code_verifier and its S256 code_challenge.
/// P4: reads random bytes from /dev/urandom via tokio async file IO.
pub async fn generate_pkce() -> Result<PkceChallenge, AppError> {
    use tokio::io::AsyncReadExt;

    // Read random bytes for both verifier and state from /dev/urandom.
    let mut buf = vec![0u8; CODE_VERIFIER_LEN + 32];
    let mut file = tokio::fs::File::open("/dev/urandom")
        .await
        .map_err(|e| AppError::claude(&format!("failed to open /dev/urandom: {e}")))?;
    file.read_exact(&mut buf)
        .await
        .map_err(|e| AppError::claude(&format!("failed to read /dev/urandom: {e}")))?;

    // Map first CODE_VERIFIER_LEN bytes to PKCE-safe characters
    let verifier: String = buf[..CODE_VERIFIER_LEN]
        .iter()
        .map(|&b| PKCE_CHARSET[(b as usize) % PKCE_CHARSET.len()] as char)
        .collect();

    // Use remaining 32 bytes for state (base64url encoded)
    let state = URL_SAFE_NO_PAD.encode(&buf[CODE_VERIFIER_LEN..]);

    // S256: base64url(sha256(verifier))
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    Ok(PkceChallenge {
        verifier,
        challenge,
        state,
    })
}

/// Build the full OAuth authorization URL with PKCE challenge and state.
#[inline]
pub fn build_authorize_url(code_challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?\
         code=true\
         &client_id={CLIENT_ID}\
         &response_type=code\
         &redirect_uri={redirect}\
         &scope={scopes}\
         &code_challenge={code_challenge}\
         &code_challenge_method=S256\
         &state={state}",
        redirect = urlencoded(REDIRECT_URI),
        scopes = urlencoded(SCOPES),
    )
}

/// Exchange authorization code for tokens via the token endpoint.
/// Matches the Claude CLI's exact request format (reverse-engineered via mitmproxy):
/// `User-Agent: axios/1.13.6`, `Content-Type: application/json`, JSON body.
/// Retries on 429 with exponential backoff.
pub async fn exchange_code(
    code: &str,
    code_verifier: &str,
    state: &str,
) -> Result<serde_json::Value, AppError> {
    let json_body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "client_id": CLIENT_ID,
        "code_verifier": code_verifier,
        "state": state,
    });

    let client = reqwest::Client::builder()
        .user_agent("axios/1.13.6")
        .build()
        .map_err(|e| AppError::claude(&format!("failed to build HTTP client: {e}")))?;

    for attempt in 0..=MAX_TOKEN_RETRIES {
        let resp = client
            .post(TOKEN_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/plain, */*")
            .json(&json_body)
            .send()
            .await
            .map_err(|e| AppError::claude(&format!("token exchange request failed: {e}")))?;

        let http_status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();

        if http_status == 429 && attempt < MAX_TOKEN_RETRIES {
            let delay = RETRY_BASE_DELAY_SECS * (1 << attempt);
            tracing::warn!(attempt, delay_secs = delay, "token endpoint rate limited, retrying");
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            continue;
        }

        if http_status != 200 {
            tracing::error!(http_status, body = %body, "token exchange error response");
            let msg = serde_json::from_str::<serde_json::Value>(body.trim())
                .ok()
                .and_then(|v| {
                    // Try .error.message (Anthropic format) then .error (string)
                    v.get("error")
                        .and_then(|e| e.get("message").and_then(|m| m.as_str()).map(String::from))
                        .or_else(|| v.get("error").and_then(|e| e.as_str()).map(String::from))
                })
                .unwrap_or_else(|| format!("HTTP {http_status}: {body}"));
            return Err(AppError::claude(&format!(
                "token exchange failed: {msg}",
            )));
        }

        let token_response: serde_json::Value = serde_json::from_str(body.trim())
            .map_err(|e| AppError::claude(&format!("invalid token response JSON: {e}")))?;

        return Ok(token_response);
    }

    Err(AppError::claude("token exchange failed after max retries"))
}

/// Resolve the credentials file path.
/// Uses $CLAUDE_CONFIG_DIR if set, otherwise ~/.claude/.credentials.json
pub fn credentials_path() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
                .join(".claude")
        })
        .join(".credentials.json")
}

/// Check if the current token in credentials file is still valid (not expired).
pub async fn is_token_valid() -> bool {
    let path = credentials_path();
    tokio::fs::read_to_string(&path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("claudeAiOauth")?.get("expiresAt")?.as_i64())
        .is_some_and(|exp| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            exp > now_ms
        })
}

/// Build the credentials JSON from a token response and write it to disk.
/// P4: async file write via tokio. Sets file mode to 0600.
pub async fn write_credentials(token_response: &serde_json::Value) -> Result<(), AppError> {
    let access_token = token_response
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::claude("token response missing access_token"))?;

    let refresh_token = token_response
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::claude("token response missing refresh_token"))?;

    let expires_in = token_response
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    let expires_at = chrono::Utc::now().timestamp_millis() + (expires_in * 1000);

    // Parse scopes from space-separated string or use defaults
    let scope_str = token_response
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or(SCOPES);
    let scopes: Vec<&str> = scope_str.split_whitespace().collect();

    let credentials = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at,
            "scopes": scopes,
        }
    });

    let path = credentials_path();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AppError::claude(&format!("failed to create config dir: {e}")))?;
    }

    let json = serde_json::to_string_pretty(&credentials)?;
    tokio::fs::write(&path, json.as_bytes())
        .await
        .map_err(|e| AppError::claude(&format!("failed to write credentials: {e}")))?;

    // Set file permissions to 0600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path, perms)
            .await
            .map_err(|e| AppError::claude(&format!("failed to set credentials permissions: {e}")))?;
    }

    tracing::info!(path = %path.display(), "credentials written");
    Ok(())
}

/// Minimal percent-encoding for URL query parameter values.
/// Only encodes characters that are unsafe in query strings.
#[inline]
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX_UPPER[(b >> 4) as usize] as char);
                out.push(HEX_UPPER[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoded_preserves_safe_chars() {
        assert_eq!(urlencoded("hello-world_123"), "hello-world_123");
    }

    #[test]
    fn urlencoded_encodes_spaces_and_colons() {
        assert_eq!(urlencoded("a b:c"), "a%20b%3Ac");
    }

    #[test]
    fn urlencoded_redirect_uri() {
        let encoded = urlencoded(REDIRECT_URI);
        assert_eq!(encoded, "https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback");
    }

    #[tokio::test]
    async fn generate_pkce_valid_length_and_chars() {
        let pkce = generate_pkce().await.unwrap();
        assert_eq!(pkce.verifier.len(), CODE_VERIFIER_LEN);
        assert!(pkce.verifier.chars().all(|c| PKCE_CHARSET.contains(&(c as u8))));
        // Challenge should be non-empty base64url
        assert!(!pkce.challenge.is_empty());
        assert!(pkce.challenge.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn build_authorize_url_contains_required_params() {
        let url = build_authorize_url("test_challenge", "test_state");
        assert!(url.starts_with(AUTHORIZE_URL));
        assert!(url.contains("client_id="));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=test_challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope="));
        assert!(url.contains("state=test_state"));
    }

    #[test]
    fn pkce_challenge_matches_known_vector() {
        // RFC 7636 Appendix B test vector
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(hash);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
