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

use super::policy::existing_path;
use super::{
    CommandRunner, CommandSpec, ToolPolicy, command_result, definition, ensure_success, invalid,
    parse_args,
};

#[derive(Clone, Copy)]
enum DockerAction {
    Ps,
    Images,
    Inspect,
    Logs,
    Stats,
    Version,
    Pull,
    Start,
    Stop,
    Restart,
    Remove,
    Build,
    Tag,
    Push,
    ComposeUp,
    ComposeDown,
    Exec,
    Run,
    Prune,
}

struct DockerTool {
    definition: ToolDefinition,
    action: DockerAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NameArgs {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    container: String,
    tail: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageArgs {
    image: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildArgs {
    path: String,
    tag: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TagArgs {
    image: String,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    container: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunArgs {
    image: String,
    name: Option<String>,
    command: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PruneArgs {
    #[serde(default)]
    volumes: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[allow(clippy::too_many_lines)]
pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "docker.ps",
            "Read the container list.",
            DockerAction::Ps,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.images",
            "Read the local image list.",
            DockerAction::Images,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.inspect",
            "Read container or image details.",
            DockerAction::Inspect,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.logs",
            "Read container logs.",
            DockerAction::Logs,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.stats",
            "Read container resource usage.",
            DockerAction::Stats,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.version",
            "Read Docker version and daemon information.",
            DockerAction::Version,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "docker.pull",
            "Pull a Docker image.",
            DockerAction::Pull,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.start",
            "Start a Docker container.",
            DockerAction::Start,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.stop",
            "Stop a Docker container.",
            DockerAction::Stop,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.restart",
            "Restart a Docker container.",
            DockerAction::Restart,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.rm",
            "Remove a Docker container.",
            DockerAction::Remove,
            PermissionLevel::Operator,
            ToolSideEffect::Destructive,
            true,
        ),
        (
            "docker.build",
            "Build a Docker image with Admin permission; a Dockerfile can execute arbitrary build instructions.",
            DockerAction::Build,
            PermissionLevel::Admin,
            ToolSideEffect::Stateful,
            false,
        ),
        (
            "docker.tag",
            "Add a tag to a Docker image.",
            DockerAction::Tag,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.push",
            "Push a Docker image.",
            DockerAction::Push,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "docker.compose_up",
            "Start a Docker Compose project from an allowlisted directory with Admin permission.",
            DockerAction::ComposeUp,
            PermissionLevel::Admin,
            ToolSideEffect::Stateful,
            false,
        ),
        (
            "docker.compose_down",
            "Stop a Docker Compose project from an allowlisted directory with Admin permission.",
            DockerAction::ComposeDown,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
        (
            "docker.exec",
            "Execute an arbitrary program in a container with Admin permission; hidden from the model by default.",
            DockerAction::Exec,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
        (
            "docker.run",
            "Create a container and run an optional program with Admin permission; hidden from the model by default.",
            DockerAction::Run,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
        (
            "docker.prune",
            "Prune unused Docker resources; requires Admin permission.",
            DockerAction::Prune,
            PermissionLevel::Admin,
            ToolSideEffect::Destructive,
            false,
        ),
    ]
    .into_iter()
    .map(
        |(name, description, action, permission, side_effect, model_visible)| {
            Arc::new(DockerTool {
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

fn schema_for(action: DockerAction) -> serde_json::Value {
    match action {
        DockerAction::Ps | DockerAction::Images | DockerAction::Version => {
            json!({"type":"object","additionalProperties":false})
        }
        DockerAction::Inspect
        | DockerAction::Start
        | DockerAction::Stop
        | DockerAction::Restart
        | DockerAction::Remove
        | DockerAction::Stats => {
            json!({"type":"object","required":["name"],"properties":{"name":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        DockerAction::Logs => {
            json!({"type":"object","required":["container"],"properties":{"container":{"type":"string","minLength":1},"tail":{"type":["integer","null"],"minimum":1,"maximum":5000}},"additionalProperties":false})
        }
        DockerAction::Pull | DockerAction::Push => {
            json!({"type":"object","required":["image"],"properties":{"image":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        DockerAction::Build => {
            json!({"type":"object","required":["path","tag"],"properties":{"path":{"type":"string","minLength":1},"tag":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        DockerAction::Tag => {
            json!({"type":"object","required":["image","target"],"properties":{"image":{"type":"string","minLength":1},"target":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        DockerAction::ComposeUp | DockerAction::ComposeDown => {
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        DockerAction::Exec => {
            json!({"type":"object","required":["container","program"],"properties":{"container":{"type":"string","minLength":1},"program":{"type":"string","minLength":1},"args":{"type":"array","items":{"type":"string"}}},"additionalProperties":false})
        }
        DockerAction::Run => {
            json!({"type":"object","required":["image"],"properties":{"image":{"type":"string","minLength":1},"name":{"type":["string","null"]},"command":{"type":["array","null"],"items":{"type":"string"}}},"additionalProperties":false})
        }
        DockerAction::Prune => {
            json!({"type":"object","properties":{"volumes":{"type":"boolean"}},"additionalProperties":false})
        }
    }
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl ToolExecutor for DockerTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            DockerAction::Ps => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.fixed(
                    vec!["ps".into(), "--all".into(), "--no-trunc".into()],
                    "Docker containers",
                    cancel,
                )
                .await
            }
            DockerAction::Images => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.fixed(
                    vec!["images".into(), "--no-trunc".into()],
                    "Docker images",
                    cancel,
                )
                .await
            }
            DockerAction::Version => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.fixed(vec!["version".into()], "Docker version", cancel)
                    .await
            }
            DockerAction::Inspect => {
                let args: NameArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.name)?;
                self.fixed(vec!["inspect".into(), args.name], "Docker inspect", cancel)
                    .await
            }
            DockerAction::Logs => {
                let args: LogsArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.container)?;
                if args.tail == Some(0) {
                    return Err(invalid("Docker log line count must be greater than zero"));
                }
                self.fixed(
                    vec![
                        "logs".into(),
                        "--tail".into(),
                        args.tail.unwrap_or(200).min(5_000).to_string(),
                        args.container,
                    ],
                    "Docker logs",
                    cancel,
                )
                .await
            }
            DockerAction::Stats => {
                let args: NameArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.name)?;
                self.fixed(
                    vec!["stats".into(), "--no-stream".into(), args.name],
                    "Docker resources",
                    cancel,
                )
                .await
            }
            DockerAction::Pull | DockerAction::Push => {
                let args: ImageArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.image)?;
                self.policy.require_mutation()?;
                let action = if matches!(self.action, DockerAction::Pull) {
                    "pull"
                } else {
                    "push"
                };
                self.fixed_checked(
                    vec![action.into(), args.image],
                    "Docker image operation",
                    cancel,
                )
                .await
            }
            DockerAction::Start
            | DockerAction::Stop
            | DockerAction::Restart
            | DockerAction::Remove => {
                let args: NameArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.name)?;
                self.policy.require_mutation()?;
                let action = match self.action {
                    DockerAction::Start => "start",
                    DockerAction::Stop => "stop",
                    DockerAction::Restart => "restart",
                    _ => "rm",
                };
                self.fixed_checked(
                    vec![action.into(), args.name],
                    "Docker container operation",
                    cancel,
                )
                .await
            }
            DockerAction::Build => {
                let args: BuildArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.tag)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                let path = existing_path(&self.policy, &args.path)?;
                if !path.is_dir() {
                    return Err(invalid("Docker build path must be a directory"));
                }
                self.fixed_checked(
                    vec![
                        "build".into(),
                        "--tag".into(),
                        args.tag,
                        path.to_string_lossy().into_owned(),
                    ],
                    "Docker build",
                    cancel,
                )
                .await
            }
            DockerAction::Tag => {
                let args: TagArgs = parse_args(invocation.tool_call.arguments)?;
                validate_name(&args.image)?;
                validate_name(&args.target)?;
                self.policy.require_mutation()?;
                self.fixed_checked(
                    vec!["tag".into(), args.image, args.target],
                    "Docker tag",
                    cancel,
                )
                .await
            }
            DockerAction::ComposeUp | DockerAction::ComposeDown => {
                let args: ComposeArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                let path = existing_path(&self.policy, &args.path)?;
                if !path.is_dir() {
                    return Err(invalid("Compose project path must be a directory"));
                }
                let compose_file = find_compose_file(&self.policy, &path)?;
                let action = if matches!(self.action, DockerAction::ComposeUp) {
                    "up"
                } else {
                    "down"
                };
                let mut command = vec![
                    "compose".into(),
                    "--file".into(),
                    compose_file.to_string_lossy().into_owned(),
                    "--project-directory".into(),
                    path.to_string_lossy().into_owned(),
                    action.into(),
                ];
                if matches!(self.action, DockerAction::ComposeUp) {
                    command.push("-d".into());
                }
                self.fixed_checked(command, "Docker Compose", cancel).await
            }
            DockerAction::Exec => {
                let args: ExecArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                validate_name(&args.container)?;
                validate_program(&args.program)?;
                let mut command = vec!["exec".into(), args.container, args.program];
                command.extend(args.args);
                self.fixed_checked(command, "Docker exec", cancel).await
            }
            DockerAction::Run => {
                let args: RunArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                validate_name(&args.image)?;
                let mut command = vec!["run".into(), "--rm".into()];
                if let Some(name) = args.name {
                    validate_name(&name)?;
                    command.extend(["--name".into(), name]);
                }
                command.push(args.image);
                if let Some(program) = args.command {
                    if program.is_empty() {
                        return Err(invalid("docker.run command must not be empty"));
                    }
                    for item in &program {
                        validate_program(item)?;
                    }
                    command.extend(program);
                }
                self.fixed_checked(command, "Docker run", cancel).await
            }
            DockerAction::Prune => {
                let args: PruneArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_admin_commands()?;
                self.policy.require_mutation()?;
                let mut command = vec!["system".into(), "prune".into(), "-f".into()];
                if args.volumes {
                    command.push("--volumes".into());
                }
                self.fixed_checked(command, "Docker prune", cancel).await
            }
        }
    }
}

impl DockerTool {
    async fn fixed(
        &self,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.run(args, label, cancel, false).await
    }

    async fn fixed_checked(
        &self,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.run(args, label, cancel, true).await
    }

    async fn run(
        &self,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
        check: bool,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(
                CommandSpec {
                    program: "docker".into(),
                    args,
                    cwd: None,
                    stdin: None,
                    requires_sudo: false,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await?;
        if check {
            ensure_success(label, &output)?;
        }
        Ok(command_result(label, &output))
    }
}

fn validate_name(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty()
        || value.starts_with('-')
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid("Invalid Docker name or image reference"));
    }
    Ok(())
}

fn validate_program(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(invalid(
            "Docker program argument must not be empty or contain control characters",
        ));
    }
    Ok(())
}

fn find_compose_file(
    policy: &ToolPolicy,
    project_directory: &std::path::Path,
) -> Result<std::path::PathBuf, ToolError> {
    for name in [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yaml",
        "docker-compose.yml",
    ] {
        let candidate = project_directory.join(name);
        if candidate.is_file() {
            return existing_path(policy, &candidate.to_string_lossy());
        }
    }
    Err(ToolError::new(
        koi_core::domain::ToolErrorKind::TargetUnavailable,
        format!(
            "Compose project directory has no supported configuration file: {}",
            project_directory.display()
        ),
        false,
    ))
}
