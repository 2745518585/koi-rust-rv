use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use koi_core::domain::{ToolError, ToolErrorKind};

/// 内置工具的固定运行时边界。
///
/// 权限是否足够由核心在调用工具前裁决；这里不再包含可配置的目标白名单、写操作开关
/// 或管理员命令开关。保留的大小、超时和参数格式检查只用于保证进程与内存可控。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct ToolPolicy {
    /// Whether structured mutating commands should invoke `sudo -n` on Unix.
    pub use_sudo: bool,
    /// Environment variable names that may be inherited by child commands.
    /// Values are still taken from the current process environment.
    pub environment_allowlist: BTreeSet<String>,
    /// Whether HTTP tools may connect to private, loopback or link-local
    /// addresses after resolving an allowlisted host.
    pub allow_private_http_addresses: bool,
    /// Maximum captured stdout/stderr per command or HTTP response.
    pub max_output_bytes: usize,
    /// Maximum command stdin or HTTP request body size.
    pub max_input_bytes: usize,
    /// Maximum serialized HTTP request header size.
    pub max_http_header_bytes: usize,
    /// Maximum number of filesystem entries inspected by recursive tools.
    pub max_scanned_paths: usize,
    /// Maximum file or crontab payload size.
    pub max_file_bytes: usize,
    /// Default timeout for tools without a more specific timeout.
    pub default_timeout_ms: u64,
    /// Absolute upper bound for caller-selected timeouts.
    pub max_timeout_ms: u64,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            use_sudo: true,
            environment_allowlist: default_environment_allowlist(),
            allow_private_http_addresses: true,
            max_output_bytes: 64 * 1024,
            max_input_bytes: 1024 * 1024,
            max_http_header_bytes: 16 * 1024,
            max_scanned_paths: 100_000,
            max_file_bytes: 1024 * 1024,
            default_timeout_ms: 30_000,
            max_timeout_ms: 10 * 60 * 1000,
        }
    }
}

#[allow(clippy::unused_self)]
impl ToolPolicy {
    #[must_use]
    pub fn with_allowed_environment(mut self, name: impl Into<String>) -> Self {
        self.environment_allowlist.insert(name.into());
        self
    }

    #[must_use]
    pub fn with_private_http_addresses(mut self, allowed: bool) -> Self {
        self.allow_private_http_addresses = allowed;
        self
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn require_mutation(&self) -> Result<(), ToolError> {
        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn require_admin_commands(&self) -> Result<(), ToolError> {
        Ok(())
    }

    pub(crate) fn require_service(&self, service: &str) -> Result<(), ToolError> {
        if service.trim().is_empty()
            || service.starts_with('-')
            || service.len() > 256
            || service
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!("Invalid service name: {service}"),
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn require_http_host(&self, host: Option<&str>) -> Result<(), ToolError> {
        let Some(_host) = host else {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "URL is missing a host name",
                false,
            ));
        };
        Ok(())
    }

    pub(crate) fn require_network_host(&self, host: &str) -> Result<(), ToolError> {
        if host.trim().is_empty() || host.chars().any(char::is_control) {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "Network probe target must not be empty or contain control characters",
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn require_database_target(&self, target: &str) -> Result<(), ToolError> {
        if target.trim().is_empty() || target.chars().any(char::is_control) {
            return Err(ToolError::new(
                ToolErrorKind::TargetUnavailable,
                "Database target must not be empty or contain control characters",
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn timeout(
        &self,
        requested: Option<u64>,
        definition: u64,
    ) -> Result<u64, ToolError> {
        let timeout = requested.unwrap_or(definition);
        if timeout == 0 || timeout > self.max_timeout_ms {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!(
                    "Timeout must be between 1 and {} milliseconds.",
                    self.max_timeout_ms
                ),
                false,
            ));
        }
        Ok(timeout)
    }
}

fn default_environment_allowlist() -> BTreeSet<String> {
    [
        "HOME",
        "LANG",
        "LC_ALL",
        "LOGNAME",
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "USER",
        "USERNAME",
        "USERPROFILE",
        "WINDIR",
        "XDG_RUNTIME_DIR",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

pub(crate) fn absolute_path(path: &Path) -> Result<PathBuf, ToolError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|error| {
                ToolError::new(
                    ToolErrorKind::TargetUnavailable,
                    format!("Unable to resolve the current directory: {error}"),
                    false,
                )
            })
    }
}

pub(crate) fn existing_path(_policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::InvalidArguments,
            "Path must not be empty",
            false,
        ));
    }
    let absolute = absolute_path(Path::new(raw))?;
    let canonical = std::fs::canonicalize(&absolute).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("Path is unavailable: {}: {error}", absolute.display()),
            false,
        )
    })?;
    Ok(canonical)
}

pub(crate) fn new_path(_policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::InvalidArguments,
            "Path must not be empty",
            false,
        ));
    }
    let absolute = absolute_path(Path::new(raw))?;
    let parent = absolute.parent().ok_or_else(|| {
        ToolError::new(
            ToolErrorKind::InvalidArguments,
            "Target path has no parent directory",
            false,
        )
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!(
                "Target parent directory is unavailable: {}: {error}",
                parent.display()
            ),
            false,
        )
    })?;
    let candidate = canonical_parent.join(absolute.file_name().ok_or_else(|| {
        ToolError::new(
            ToolErrorKind::InvalidArguments,
            "Target path is missing a file name",
            false,
        )
    })?);
    Ok(candidate)
}

pub(crate) fn existing_entry(policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    let path = new_path(policy, raw)?;
    std::fs::symlink_metadata(&path).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("Path is unavailable: {}: {error}", path.display()),
            false,
        )
    })?;
    Ok(path)
}

pub(crate) fn relative_arg(repo: &Path, raw: &str) -> Result<String, ToolError> {
    let path = Path::new(raw);
    if raw.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(ToolError::new(
            ToolErrorKind::InvalidArguments,
            format!("Path argument must be relative to the repository: {raw}"),
            false,
        ));
    }
    let candidate = repo.join(path);
    if candidate.exists() {
        let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
            ToolError::new(ToolErrorKind::TargetUnavailable, error.to_string(), false)
        })?;
        if canonical != repo && !canonical.starts_with(repo) {
            return Err(ToolError::new(
                ToolErrorKind::TargetUnavailable,
                format!("Repository path escapes the repository root: {raw}"),
                false,
            ));
        }
    }
    Ok(raw.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{ToolPolicy, existing_path};

    #[test]
    fn runtime_limits_do_not_restrict_authorized_targets() {
        let policy = ToolPolicy::default();
        assert!(policy.require_mutation().is_ok());
        assert!(policy.require_admin_commands().is_ok());
        assert!(policy.require_http_host(Some("example.com")).is_ok());
        assert!(policy.require_network_host("example.com").is_ok());
        assert!(existing_path(&policy, ".").is_ok());
    }
}
