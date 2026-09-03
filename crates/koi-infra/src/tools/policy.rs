use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use koi_core::domain::{ToolError, ToolErrorKind};
use serde::Deserialize;

/// Runtime safety policy shared by built-in tools.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ToolPolicy {
    /// Filesystem roots that structured file, Git, archive and compose tools
    /// may access. An empty list fails closed.
    pub allowed_roots: Vec<PathBuf>,
    /// Service units that service tools may address.
    pub allowed_services: BTreeSet<String>,
    /// Host names allowed for HTTP tools. An empty list fails closed.
    pub allowed_http_hosts: BTreeSet<String>,
    /// Host names allowed for active network probes. An empty list fails closed.
    pub allowed_network_hosts: BTreeSet<String>,
    /// Database names or local database paths allowed for read-only tools.
    pub allowed_database_targets: BTreeSet<String>,
    /// Whether Operator-level mutating tools are enabled.
    pub allow_mutating_tools: bool,
    /// Whether Admin-only arbitrary process tools are enabled.
    pub allow_admin_commands: bool,
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
            allowed_roots: Vec::new(),
            allowed_services: BTreeSet::new(),
            allowed_http_hosts: BTreeSet::new(),
            allowed_network_hosts: BTreeSet::new(),
            allowed_database_targets: BTreeSet::new(),
            allow_mutating_tools: false,
            allow_admin_commands: false,
            use_sudo: true,
            environment_allowlist: default_environment_allowlist(),
            allow_private_http_addresses: false,
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

impl ToolPolicy {
    /// A convenient explicitly-scoped policy for local development and tests.
    #[must_use]
    pub fn development(root: impl Into<PathBuf>) -> Self {
        let mut policy = Self::default();
        policy.allowed_roots.push(root.into());
        policy.allow_mutating_tools = true;
        policy.allow_admin_commands = true;
        policy
    }

    #[must_use]
    pub fn with_allowed_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.allowed_roots.push(root.into());
        self
    }

    #[must_use]
    pub fn with_allowed_service(mut self, service: impl Into<String>) -> Self {
        self.allowed_services.insert(service.into());
        self
    }

    #[must_use]
    pub fn with_allowed_http_host(mut self, host: impl Into<String>) -> Self {
        self.allowed_http_hosts
            .insert(host.into().to_ascii_lowercase());
        self
    }

    #[must_use]
    pub fn with_allowed_network_host(mut self, host: impl Into<String>) -> Self {
        self.allowed_network_hosts
            .insert(host.into().to_ascii_lowercase());
        self
    }

    #[must_use]
    pub fn with_allowed_database_target(mut self, target: impl Into<String>) -> Self {
        self.allowed_database_targets.insert(target.into());
        self
    }

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

    pub(crate) fn require_mutation(&self) -> Result<(), ToolError> {
        if self.allow_mutating_tools {
            Ok(())
        } else {
            Err(ToolError::new(
                ToolErrorKind::ExecutionFailed,
                "写操作已被当前工具策略禁用",
                false,
            ))
        }
    }

    pub(crate) fn require_admin_commands(&self) -> Result<(), ToolError> {
        if self.allow_admin_commands {
            Ok(())
        } else {
            Err(ToolError::new(
                ToolErrorKind::ExecutionFailed,
                "Admin 任意命令工具未在当前策略中启用",
                false,
            ))
        }
    }

    pub(crate) fn require_service(&self, service: &str) -> Result<(), ToolError> {
        if service.trim().is_empty()
            || service.starts_with('-')
            || service.len() > 256
            || service
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            || !self.allowed_services.contains(service)
        {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!("服务未被策略允许：{service}"),
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn require_http_host(&self, host: Option<&str>) -> Result<(), ToolError> {
        let Some(host) = host.map(str::to_ascii_lowercase) else {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "URL 缺少主机名",
                false,
            ));
        };
        if !self.allowed_http_hosts.contains(&host) {
            return Err(ToolError::new(
                ToolErrorKind::TargetUnavailable,
                format!("HTTP 主机未被策略允许：{host}"),
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn require_network_host(&self, host: &str) -> Result<(), ToolError> {
        let normalized = host.to_ascii_lowercase();
        if !self.allowed_network_hosts.contains(&normalized) {
            return Err(ToolError::new(
                ToolErrorKind::TargetUnavailable,
                format!("网络探测目标未被策略允许：{host}"),
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn require_database_target(&self, target: &str) -> Result<(), ToolError> {
        if target.trim().is_empty()
            || target
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            || !self.allowed_database_targets.contains(target)
        {
            return Err(ToolError::new(
                ToolErrorKind::TargetUnavailable,
                format!("数据库目标未被策略允许：{target}"),
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
                format!("超时时间必须位于 1 到 {} 毫秒之间", self.max_timeout_ms),
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
                    format!("无法取得当前目录：{error}"),
                    false,
                )
            })
    }
}

fn allowed_roots(policy: &ToolPolicy) -> Vec<PathBuf> {
    policy
        .allowed_roots
        .iter()
        .filter_map(|root| {
            let absolute = absolute_path(root).ok()?;
            Some(std::fs::canonicalize(&absolute).unwrap_or(absolute))
        })
        .collect()
}

fn ensure_within(policy: &ToolPolicy, path: &Path) -> Result<(), ToolError> {
    let roots = allowed_roots(policy);
    if roots.is_empty()
        || !roots
            .iter()
            .any(|root| path == root || path.starts_with(root))
    {
        return Err(ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("路径不在允许范围内：{}", path.display()),
            false,
        ));
    }
    Ok(())
}

pub(crate) fn existing_path(policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::InvalidArguments,
            "路径不能为空",
            false,
        ));
    }
    let absolute = absolute_path(Path::new(raw))?;
    let canonical = std::fs::canonicalize(&absolute).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("路径不可用：{}：{error}", absolute.display()),
            false,
        )
    })?;
    ensure_within(policy, &canonical)?;
    Ok(canonical)
}

pub(crate) fn new_path(policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::InvalidArguments,
            "路径不能为空",
            false,
        ));
    }
    let absolute = absolute_path(Path::new(raw))?;
    let parent = absolute.parent().ok_or_else(|| {
        ToolError::new(ToolErrorKind::InvalidArguments, "目标路径没有父目录", false)
    })?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("目标父目录不可用：{}：{error}", parent.display()),
            false,
        )
    })?;
    let candidate = canonical_parent.join(absolute.file_name().ok_or_else(|| {
        ToolError::new(ToolErrorKind::InvalidArguments, "目标路径缺少文件名", false)
    })?);
    ensure_within(policy, &candidate)?;
    Ok(candidate)
}

pub(crate) fn existing_entry(policy: &ToolPolicy, raw: &str) -> Result<PathBuf, ToolError> {
    let path = new_path(policy, raw)?;
    std::fs::symlink_metadata(&path).map_err(|error| {
        ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("路径不可用：{}：{error}", path.display()),
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
            format!("路径参数必须是仓库内相对路径：{raw}"),
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
                format!("仓库路径越界：{raw}"),
                false,
            ));
        }
    }
    Ok(raw.to_owned())
}
