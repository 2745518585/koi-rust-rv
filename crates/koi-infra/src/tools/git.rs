use std::sync::Arc;

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizedToolInvocation, PermissionLevel, ToolDefinition, ToolError, ToolResult,
    ToolSideEffect,
};
use koi_core::ports::ToolExecutor;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::policy::{existing_path, new_path, relative_arg};
use super::{
    CommandRunner, CommandSpec, ToolPolicy, command_result, definition, ensure_success, invalid,
    parse_args,
};

#[derive(Clone, Copy)]
enum GitAction {
    Status,
    Log,
    Diff,
    Show,
    Branch,
    Remote,
    Clone,
    Fetch,
    Pull,
    Checkout,
    Add,
    Commit,
    Stash,
    Merge,
    Rebase,
    Push,
    Reset,
    Clean,
}

struct GitTool {
    definition: ToolDefinition,
    action: GitAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    path: String,
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    path: String,
    #[serde(default)]
    staged: bool,
    file: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArgs {
    path: String,
    revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloneArgs {
    url: String,
    destination: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutArgs {
    path: String,
    branch: String,
    #[serde(default)]
    create: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    path: String,
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitArgs {
    path: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StashArgs {
    path: String,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchArgs {
    path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushArgs {
    path: String,
    remote: Option<String>,
    branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetArgs {
    path: String,
    mode: Option<String>,
    revision: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanArgs {
    path: String,
    #[serde(default)]
    directories: bool,
    #[serde(default)]
    ignored: bool,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "git.status",
            "Read Git working tree status.",
            GitAction::Status,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.log",
            "Read Git commit history.",
            GitAction::Log,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.diff",
            "Read Git working tree differences.",
            GitAction::Diff,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.show",
            "Read Git commit or object details.",
            GitAction::Show,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.branch",
            "Read Git branch information.",
            GitAction::Branch,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.remote",
            "Read Git remote repository information.",
            GitAction::Remote,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "git.clone",
            "Clone a Git repository into an allowed directory.",
            GitAction::Clone,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.fetch",
            "Fetch updates from a Git remote repository.",
            GitAction::Fetch,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.pull",
            "Update a Git working tree in fast-forward-only mode.",
            GitAction::Pull,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.checkout",
            "Switch or create a Git branch.",
            GitAction::Checkout,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.add",
            "Add specified files to the Git staging area.",
            GitAction::Add,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.commit",
            "Create a Git commit.",
            GitAction::Commit,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.stash",
            "Stash Git working tree changes.",
            GitAction::Stash,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.merge",
            "Merge a specified Git branch.",
            GitAction::Merge,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.rebase",
            "Rebase a Git working tree.",
            GitAction::Rebase,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.push",
            "Push Git commits to a remote repository.",
            GitAction::Push,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "git.reset",
            "Execute Git reset; high-risk modes require Admin permission.",
            GitAction::Reset,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
        (
            "git.clean",
            "Clean untracked Git files; requires Admin permission.",
            GitAction::Clean,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
    ]
    .into_iter()
    .map(
        |(name, description, action, permission, side_effect, model_visible)| {
            Arc::new(GitTool {
                definition: definition(
                    name,
                    description,
                    schema_for(action),
                    permission,
                    side_effect,
                    120_000,
                    model_visible,
                ),
                action,
                policy: policy.clone(),
                runner: runner.clone(),
            }) as Arc<dyn ToolExecutor>
        },
    )
    .collect()
}

fn schema_for(action: GitAction) -> serde_json::Value {
    let repo = json!({"path":{"type":"string","minLength":1}});
    match action {
        GitAction::Status
        | GitAction::Branch
        | GitAction::Remote
        | GitAction::Fetch
        | GitAction::Pull => {
            json!({"type":"object","required":["path"],"properties":repo,"additionalProperties":false})
        }
        GitAction::Log => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"limit":{"type":["integer","null"],"minimum":1,"maximum":1000}},"additionalProperties":false})
        }
        GitAction::Diff => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"staged":{"type":"boolean"},"file":{"type":["string","null"]}},"additionalProperties":false})
        }
        GitAction::Show => {
            json!({"type":"object","required":["path","revision"],"properties":{"path":{"type":"string"},"revision":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        GitAction::Clone => {
            json!({"type":"object","required":["url","destination"],"properties":{"url":{"type":"string","minLength":1},"destination":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        GitAction::Checkout => {
            json!({"type":"object","required":["path","branch"],"properties":{"path":{"type":"string"},"branch":{"type":"string","minLength":1},"create":{"type":"boolean"}},"additionalProperties":false})
        }
        GitAction::Add => {
            json!({"type":"object","required":["path","files"],"properties":{"path":{"type":"string"},"files":{"type":"array","minItems":1,"items":{"type":"string"}}},"additionalProperties":false})
        }
        GitAction::Commit => {
            json!({"type":"object","required":["path","message"],"properties":{"path":{"type":"string"},"message":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        GitAction::Stash => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"message":{"type":["string","null"]}},"additionalProperties":false})
        }
        GitAction::Merge | GitAction::Rebase => {
            json!({"type":"object","required":["path","branch"],"properties":{"path":{"type":"string"},"branch":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        GitAction::Push => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"remote":{"type":["string","null"]},"branch":{"type":["string","null"]}},"additionalProperties":false})
        }
        GitAction::Reset => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"mode":{"type":["string","null"],"enum":["soft","mixed","hard",null]},"revision":{"type":["string","null"]}},"additionalProperties":false})
        }
        GitAction::Clean => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"},"directories":{"type":"boolean"},"ignored":{"type":"boolean"}},"additionalProperties":false})
        }
    }
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl ToolExecutor for GitTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            GitAction::Status => {
                self.repo_command(
                    invocation.tool_call.arguments,
                    vec!["status".into(), "--short".into(), "--branch".into()],
                    "Git status",
                    cancel,
                )
                .await
            }
            GitAction::Branch => {
                self.repo_command(
                    invocation.tool_call.arguments,
                    vec!["branch".into(), "--all".into(), "--no-color".into()],
                    "Git branches",
                    cancel,
                )
                .await
            }
            GitAction::Remote => {
                self.repo_command(
                    invocation.tool_call.arguments,
                    vec!["remote".into(), "-v".into()],
                    "Git remotes",
                    cancel,
                )
                .await
            }
            GitAction::Log => {
                let args: LogArgs = parse_args(invocation.tool_call.arguments)?;
                let repo = self.repo(&args.path)?;
                let limit = args.limit.unwrap_or(50).min(1_000);
                self.run_repo(
                    repo,
                    vec![
                        "log".into(),
                        "--oneline".into(),
                        "--decorate".into(),
                        "-n".into(),
                        limit.to_string(),
                    ],
                    "Git log",
                    cancel,
                )
                .await
            }
            GitAction::Diff => {
                let args: DiffArgs = parse_args(invocation.tool_call.arguments)?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["diff".into(), "--no-ext-diff".into()];
                if args.staged {
                    command.push("--cached".into());
                }
                command.push("--".into());
                if let Some(file) = args.file {
                    command.push(relative_arg(&repo, &file)?);
                }
                self.run_repo(repo, command, "Git diff", cancel).await
            }
            GitAction::Show => {
                let args: ShowArgs = parse_args(invocation.tool_call.arguments)?;
                validate_git_atom(&args.revision)?;
                let repo = self.repo(&args.path)?;
                self.run_repo(
                    repo,
                    vec![
                        "show".into(),
                        "--stat".into(),
                        "--oneline".into(),
                        args.revision,
                    ],
                    "Git object",
                    cancel,
                )
                .await
            }
            GitAction::Clone => {
                let args: CloneArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                validate_clone_source(&self.policy, &args.url)?;
                let destination = new_path(&self.policy, &args.destination)?;
                if std::fs::symlink_metadata(&destination)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(invalid(
                        "Cannot clone a Git repository to a symbolic link target",
                    ));
                }
                self.run(
                    CommandSpec {
                        program: "git".into(),
                        args: git_safety_args()
                            .into_iter()
                            .chain([
                                "clone".into(),
                                "--no-recurse-submodules".into(),
                                "--".into(),
                                args.url,
                                destination.to_string_lossy().into_owned(),
                            ])
                            .collect(),
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    "Git clone",
                    cancel,
                    true,
                )
                .await
            }
            GitAction::Fetch | GitAction::Pull => {
                let args: RepoArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let command = if matches!(self.action, GitAction::Fetch) {
                    vec!["fetch".into(), "--prune".into()]
                } else {
                    vec!["pull".into(), "--ff-only".into()]
                };
                self.run_repo_checked(
                    repo,
                    command,
                    if matches!(self.action, GitAction::Fetch) {
                        "Git fetch"
                    } else {
                        "Git pull"
                    },
                    cancel,
                )
                .await
            }
            GitAction::Checkout => {
                let args: CheckoutArgs = parse_args(invocation.tool_call.arguments)?;
                validate_git_atom(&args.branch)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["switch".into()];
                if args.create {
                    command.push("--create".into());
                }
                command.push(args.branch);
                self.run_repo_checked(repo, command, "Git branch switch", cancel)
                    .await
            }
            GitAction::Add => {
                let args: AddArgs = parse_args(invocation.tool_call.arguments)?;
                if args.files.is_empty() {
                    return Err(invalid("git.add requires at least one file"));
                }
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["add".into(), "--".into()];
                for file in args.files {
                    command.push(relative_arg(&repo, &file)?);
                }
                self.run_repo_checked(repo, command, "Git staging", cancel)
                    .await
            }
            GitAction::Commit => {
                let args: CommitArgs = parse_args(invocation.tool_call.arguments)?;
                validate_message(&args.message)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                self.run_repo_checked(
                    repo,
                    vec!["commit".into(), "-m".into(), args.message],
                    "Git commit",
                    cancel,
                )
                .await
            }
            GitAction::Stash => {
                let args: StashArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["stash".into(), "push".into()];
                if let Some(message) = args.message {
                    validate_message(&message)?;
                    command.extend(["-m".into(), message]);
                }
                self.run_repo_checked(repo, command, "Git stash", cancel)
                    .await
            }
            GitAction::Merge | GitAction::Rebase => {
                let args: BranchArgs = parse_args(invocation.tool_call.arguments)?;
                validate_git_atom(&args.branch)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let action = if matches!(self.action, GitAction::Merge) {
                    "merge"
                } else {
                    "rebase"
                };
                self.run_repo_checked(
                    repo,
                    vec![action.into(), args.branch],
                    "Git branch operation",
                    cancel,
                )
                .await
            }
            GitAction::Push => {
                let args: PushArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["push".into()];
                if let Some(remote) = args.remote {
                    validate_git_atom(&remote)?;
                    command.push(remote);
                }
                if let Some(branch) = args.branch {
                    validate_git_atom(&branch)?;
                    command.push(branch);
                }
                self.run_repo_checked(repo, command, "Git push", cancel)
                    .await
            }
            GitAction::Reset => {
                let args: ResetArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                let mode = args.mode.unwrap_or_else(|| "mixed".into());
                if !matches!(mode.as_str(), "soft" | "mixed" | "hard") {
                    return Err(invalid("git.reset mode must be soft, mixed, or hard"));
                }
                let revision = args.revision.unwrap_or_else(|| "HEAD".into());
                validate_git_atom(&revision)?;
                let repo = self.repo(&args.path)?;
                self.run_repo_checked(
                    repo,
                    vec!["reset".into(), format!("--{mode}"), revision],
                    "Git reset",
                    cancel,
                )
                .await
            }
            GitAction::Clean => {
                let args: CleanArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                let repo = self.repo(&args.path)?;
                let mut command = vec!["clean".into(), "-f".into()];
                if args.directories {
                    command.push("-d".into());
                }
                if args.ignored {
                    command.push("-x".into());
                }
                self.run_repo_checked(repo, command, "Git clean", cancel)
                    .await
            }
        }
    }
}

impl GitTool {
    fn repo(&self, path: &str) -> Result<std::path::PathBuf, ToolError> {
        let repo = existing_path(&self.policy, path)?;
        if !repo.is_dir() {
            return Err(invalid("Git repository path must be a directory"));
        }
        Ok(repo)
    }

    async fn repo_command(
        &self,
        value: serde_json::Value,
        command: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args: RepoArgs = parse_args(value)?;
        let repo = self.repo(&args.path)?;
        self.run_repo(repo, command, label, cancel).await
    }

    async fn run_repo(
        &self,
        repo: std::path::PathBuf,
        command: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(
                CommandSpec {
                    program: "git".into(),
                    args: git_safety_args()
                        .into_iter()
                        .chain(["-C".into(), repo.to_string_lossy().into_owned()])
                        .chain(command)
                        .collect(),
                    cwd: None,
                    stdin: None,
                    requires_sudo: false,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await?;
        ensure_success(label, &output)?;
        Ok(command_result(label, &output))
    }

    async fn run_repo_checked(
        &self,
        repo: std::path::PathBuf,
        command: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.run_repo(repo, command, label, cancel).await
    }

    async fn run(
        &self,
        spec: CommandSpec,
        label: &str,
        cancel: CancellationToken,
        check: bool,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(spec, self.definition.timeout_ms, cancel)
            .await?;
        if check {
            ensure_success(label, &output)?;
        }
        Ok(command_result(label, &output))
    }
}

fn git_safety_args() -> Vec<String> {
    let hooks_path = format!("core.hooksPath={}", disabled_hooks_path());
    [
        "--no-optional-locks",
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "diff.external=",
        "-c",
        &hooks_path,
        "-c",
        "core.sshCommand=ssh",
        "-c",
        "core.gitProxy=",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[cfg(windows)]
fn disabled_hooks_path() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn disabled_hooks_path() -> &'static str {
    "/dev/null"
}

fn validate_git_atom(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid(
            "Git argument is empty, contains whitespace, or begins with an option prefix",
        ));
    }
    Ok(())
}

fn validate_message(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.len() > 4_000 || value.chars().any(char::is_control) {
        return Err(invalid(
            "Git commit message must not be empty, exceed 4000 bytes, or contain control characters",
        ));
    }
    Ok(())
}

fn validate_urlish(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty()
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid(
            "Git URL must not be empty or contain whitespace or control characters",
        ));
    }
    Ok(())
}

fn validate_clone_source(policy: &ToolPolicy, value: &str) -> Result<(), ToolError> {
    validate_urlish(value)?;
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("ext::") || lower.starts_with("fd::") {
        return Err(invalid("Git clone does not allow ext or fd protocols"));
    }
    if std::path::Path::new(value).exists() {
        let source = existing_path(policy, value)?;
        if !source.is_dir() {
            return Err(invalid("Local Git clone source must be a directory"));
        }
        return Ok(());
    }
    if let Ok(url) = reqwest::Url::parse(value) {
        if matches!(url.scheme(), "http" | "https")
            && (!url.username().is_empty() || url.password().is_some())
        {
            return Err(invalid(
                "Git HTTP URL must not include a user name or password",
            ));
        }
        if matches!(url.scheme(), "ssh" | "git+ssh") && url.password().is_some() {
            return Err(invalid("Git SSH URL must not include a password"));
        }
        if !matches!(url.scheme(), "http" | "https" | "git" | "ssh" | "git+ssh") {
            if url.scheme() == "file" {
                let path = url
                    .to_file_path()
                    .map_err(|()| invalid("Git file URL cannot be converted to a local path"))?;
                let source = existing_path(policy, &path.to_string_lossy())?;
                if !source.is_dir() {
                    return Err(invalid("Local Git clone source must be a directory"));
                }
                return Ok(());
            }
            return Err(invalid(format!(
                "Git URL scheme is not allowed: {}",
                url.scheme()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{git_safety_args, validate_clone_source, validate_urlish};
    use crate::tools::ToolPolicy;

    #[test]
    fn clone_source_rejects_dangerous_protocols_and_credentials() {
        let policy = ToolPolicy::default();
        assert!(validate_clone_source(&policy, "ext::sh -c whoami").is_err());
        assert!(validate_clone_source(&policy, "https://user:secret@example.com/repo").is_err());
        assert!(validate_urlish("git@github.com:org/repo").is_ok());
        assert!(validate_clone_source(&policy, "ssh://git@github.com/org/repo").is_ok());
    }

    #[test]
    fn git_tools_disable_optional_writes_and_external_helpers() {
        let args = git_safety_args();
        for value in [
            "--no-optional-locks",
            "protocol.ext.allow=never",
            "core.fsmonitor=false",
            "diff.external=",
            "core.sshCommand=ssh",
            "core.gitProxy=",
        ] {
            assert!(args.contains(&value.to_owned()));
        }
        assert!(
            args.iter()
                .any(|value| value.starts_with("core.hooksPath="))
        );
    }
}
