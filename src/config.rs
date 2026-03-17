use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use smallvec::SmallVec;

// Raw TOML deserialization target (String-based for serde compat)

#[derive(Debug, Deserialize)]
struct RawAppConfig {
    discord: RawDiscordConfig,
    claude: RawClaudeConfig,
    database: RawDatabaseConfig,
    auth: RawAuthConfig,
    #[serde(default)]
    logging: RawLoggingConfig,
}

#[derive(Debug, Deserialize)]
struct RawDiscordConfig {
    token: String,
    guild_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawClaudeConfig {
    #[serde(default = "default_binary")]
    binary: String,
    default_cwd: String,
    #[serde(default)]
    projects: HashMap<String, RawProjectConfig>,
    #[serde(default = "default_allowed_tools")]
    allowed_tools: Vec<String>,
    #[serde(default = "default_max_sessions")]
    max_sessions: usize,
    #[serde(default = "default_timeout")]
    session_timeout_minutes: u64,
    system_prompt: Option<String>,
    #[serde(default)]
    dangerously_skip_permissions: bool,
    #[serde(default)]
    use_worktrees: bool,
}

#[derive(Debug, Deserialize)]
struct RawProjectConfig {
    cwd: String,
    allowed_tools: Option<Vec<String>>,
    use_worktrees: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawAuthConfig {
    allowed_users: Vec<u64>,
    #[serde(default)]
    allowed_roles: Vec<u64>,
    #[serde(default)]
    admins: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct RawDatabaseConfig {
    url: String,
}

#[derive(Debug, Deserialize)]
struct RawLoggingConfig {
    #[serde(default = "default_level")]
    level: String,
    #[serde(default = "default_format")]
    format: String,
}

impl Default for RawLoggingConfig {
    fn default() -> Self {
        Self {
            level: default_level(),
            format: default_format(),
        }
    }
}

fn default_binary() -> String {
    "claude".into()
}
fn default_allowed_tools() -> Vec<String> {
    ["Bash", "Read", "Write", "Edit", "Glob", "Grep"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}
const fn default_max_sessions() -> usize {
    3
}
const fn default_timeout() -> u64 {
    30
}
fn default_level() -> String {
    "info".into()
}
fn default_format() -> String {
    "pretty".into()
}

// Validated, Arc<str>-backed config (P1)

#[derive(Debug)]
pub struct AppConfig {
    pub discord: DiscordConfig,
    pub claude: ClaudeConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug)]
pub struct DiscordConfig {
    pub token: Arc<str>,
    pub guild_id: Option<u64>,
}

#[derive(Debug)]
pub struct ClaudeConfig {
    pub binary: Arc<str>,
    pub default_cwd: Arc<str>,
    pub projects: HashMap<Arc<str>, ProjectConfig>,
    pub allowed_tools: SmallVec<[Arc<str>; 8]>,
    pub max_sessions: usize,
    pub session_timeout_minutes: u64,
    pub system_prompt: Option<Arc<str>>,
    pub dangerously_skip_permissions: bool,
    pub use_worktrees: bool,
}

#[derive(Debug)]
pub struct ProjectConfig {
    pub cwd: Arc<str>,
    pub allowed_tools: Option<SmallVec<[Arc<str>; 8]>>,
    pub use_worktrees: Option<bool>,
}

#[derive(Debug)]
pub struct AuthConfig {
    pub allowed_users: SmallVec<[u64; 4]>,
    pub allowed_roles: SmallVec<[u64; 4]>,
    pub admins: SmallVec<[u64; 4]>,
}

#[derive(Debug)]
pub struct DatabaseConfig {
    pub url: Arc<str>,
}

#[derive(Debug)]
pub struct LoggingConfig {
    pub level: Arc<str>,
    pub format: Arc<str>,
}

impl ClaudeConfig {
    /// P1: Resolve tools for a project.
    pub fn resolve_tools<'a>(&'a self, project: Option<&str>) -> Cow<'a, [Arc<str>]> {
        project
            .and_then(|p| self.projects.get(p))
            .and_then(|pc| pc.allowed_tools.as_ref())
            .map(|tools| Cow::Owned(tools.to_vec()))
            .unwrap_or(Cow::Borrowed(self.allowed_tools.as_slice()))
    }

    /// Resolve whether worktrees are enabled for a project.
    /// Per-project override wins, falls back to global setting.
    #[inline]
    pub fn resolve_worktrees(&self, project: Option<&str>) -> bool {
        project
            .and_then(|p| self.projects.get(p))
            .and_then(|pc| pc.use_worktrees)
            .unwrap_or(self.use_worktrees)
    }

    /// P1/P4: Resolve cwd. Config lookup → sibling directory → error if named project not found.
    /// Returns Cow::Borrowed for config hits, Cow::Owned for discovered sibling paths.
    pub async fn resolve_cwd(
        &self,
        project: Option<&str>,
    ) -> Result<Cow<'_, str>, crate::error::AppError> {
        let Some(name) = project else {
            return Ok(Cow::Borrowed(self.default_cwd.as_ref()));
        };

        // 1. Exact match in [claude.projects] config
        if let Some(pc) = self.projects.get(name) {
            return Ok(Cow::Borrowed(pc.cwd.as_ref()));
        }

        // 2. Try as sibling directory of default_cwd
        if let Some(parent) = Path::new(self.default_cwd.as_ref()).parent() {
            let candidate = parent.join(name);
            if tokio::fs::metadata(&candidate)
                .await
                .is_ok_and(|m| m.is_dir())
            {
                return Ok(Cow::Owned(candidate.to_string_lossy().into_owned()));
            }
        }

        Err(crate::error::AppError::config(&format!(
            "project '{name}' not found in config or as sibling of default_cwd"
        )))
    }
}

impl AppConfig {
    /// P4: reads config file via tokio::fs
    pub async fn from_file(path: &str) -> Result<Self, crate::error::AppError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| crate::error::AppError::config(&format!("reading {path}: {e}")))?;
        Self::parse(&content)
    }

    pub fn parse(content: &str) -> Result<Self, crate::error::AppError> {
        let raw: RawAppConfig =
            toml::from_str(content).map_err(|e| crate::error::AppError::config(&e.to_string()))?;

        Ok(AppConfig {
            discord: DiscordConfig {
                token: Arc::from(raw.discord.token.as_str()),
                guild_id: raw.discord.guild_id,
            },
            claude: ClaudeConfig {
                binary: Arc::from(raw.claude.binary.as_str()),
                default_cwd: Arc::from(raw.claude.default_cwd.as_str()),
                projects: raw
                    .claude
                    .projects
                    .into_iter()
                    .map(|(k, v)| {
                        let pc = ProjectConfig {
                            cwd: Arc::from(v.cwd.as_str()),
                            allowed_tools: v
                                .allowed_tools
                                .map(|tools| tools.iter().map(|s| Arc::from(s.as_str())).collect()),
                            use_worktrees: v.use_worktrees,
                        };
                        (Arc::from(k.as_str()), pc)
                    })
                    .collect(),
                allowed_tools: raw
                    .claude
                    .allowed_tools
                    .iter()
                    .map(|s| Arc::from(s.as_str()))
                    .collect(),
                max_sessions: raw.claude.max_sessions,
                session_timeout_minutes: raw.claude.session_timeout_minutes,
                system_prompt: raw.claude.system_prompt.map(|s| Arc::from(s.as_str())),
                dangerously_skip_permissions: raw.claude.dangerously_skip_permissions,
                use_worktrees: raw.claude.use_worktrees,
            },
            database: DatabaseConfig {
                url: Arc::from(raw.database.url.as_str()),
            },
            auth: AuthConfig {
                allowed_users: raw.auth.allowed_users.into_iter().collect(),
                allowed_roles: raw.auth.allowed_roles.into_iter().collect(),
                admins: raw.auth.admins.into_iter().collect(),
            },
            logging: LoggingConfig {
                level: Arc::from(raw.logging.level.as_str()),
                format: Arc::from(raw.logging.format.as_str()),
            },
        })
    }
}
