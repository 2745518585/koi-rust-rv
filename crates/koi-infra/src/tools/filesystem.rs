use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

use super::policy::{existing_entry, existing_path, new_path};
use super::{ToolPolicy, blocking, definition, invalid, parse_args};

#[derive(Clone, Copy)]
enum FileAction {
    List,
    Stat,
    Read,
    Find,
    Search,
    Mkdir,
    Write,
    Copy,
    Move,
    Delete,
}

struct FileTool {
    definition: ToolDefinition,
    action: FileAction,
    policy: ToolPolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: String,
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    path: String,
    name: Option<String>,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    path: String,
    pattern: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
    #[serde(default)]
    append: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyMoveArgs {
    source: String,
    destination: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn tools(policy: &ToolPolicy) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "fs.list",
            "List directory contents within the allowed roots.",
            FileAction::List,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "fs.stat",
            "Read file or directory metadata within the allowed roots.",
            FileAction::Stat,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "fs.read",
            "Read a file within the allowed roots.",
            FileAction::Read,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "fs.find",
            "Recursively find files within the allowed roots.",
            FileAction::Find,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "fs.search",
            "Search file contents within the allowed roots.",
            FileAction::Search,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "fs.mkdir",
            "Create a directory within the allowed roots.",
            FileAction::Mkdir,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "fs.write",
            "Write a file within the allowed roots.",
            FileAction::Write,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "fs.copy",
            "Copy a file within the allowed roots.",
            FileAction::Copy,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "fs.move",
            "Move a file within the allowed roots.",
            FileAction::Move,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "fs.delete",
            "Delete a file or directory within the allowed roots.",
            FileAction::Delete,
            PermissionLevel::Operator,
            ToolSideEffect::Destructive,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(name, description, action, permission, side_effect, model_visible)| {
            Arc::new(FileTool {
                definition: definition(
                    name,
                    description,
                    schema_for(action),
                    permission,
                    side_effect,
                    30_000,
                    model_visible,
                ),
                action,
                policy: policy.clone(),
            }) as Arc<dyn ToolExecutor>
        },
    )
    .collect()
}

fn schema_for(action: FileAction) -> serde_json::Value {
    match action {
        FileAction::List => {
            json!({"type":"object","properties":{"path":{"type":"string"},"max_entries":{"type":["integer","null"],"minimum":1}},"required":["path"],"additionalProperties":false})
        }
        FileAction::Stat | FileAction::Mkdir => {
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false})
        }
        FileAction::Read => {
            json!({"type":"object","properties":{"path":{"type":"string"},"max_bytes":{"type":["integer","null"],"minimum":1}},"required":["path"],"additionalProperties":false})
        }
        FileAction::Find => {
            json!({"type":"object","properties":{"path":{"type":"string"},"name":{"type":["string","null"]},"max_results":{"type":["integer","null"],"minimum":1}},"required":["path"],"additionalProperties":false})
        }
        FileAction::Search => {
            json!({"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string","minLength":1},"max_results":{"type":["integer","null"],"minimum":1}},"required":["path","pattern"],"additionalProperties":false})
        }
        FileAction::Write => {
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"},"append":{"type":"boolean"}},"required":["path","content"],"additionalProperties":false})
        }
        FileAction::Copy | FileAction::Move => {
            json!({"type":"object","properties":{"source":{"type":"string"},"destination":{"type":"string"},"overwrite":{"type":"boolean"}},"required":["source","destination"],"additionalProperties":false})
        }
        FileAction::Delete => {
            json!({"type":"object","properties":{"path":{"type":"string"},"recursive":{"type":"boolean"}},"required":["path"],"additionalProperties":false})
        }
    }
}

#[async_trait]
impl ToolExecutor for FileTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            FileAction::List => self.list(parse_args(invocation.tool_call.arguments)?).await,
            FileAction::Stat => self.stat(parse_args(invocation.tool_call.arguments)?).await,
            FileAction::Read => self.read(parse_args(invocation.tool_call.arguments)?).await,
            FileAction::Find => self.find(parse_args(invocation.tool_call.arguments)?).await,
            FileAction::Search => {
                self.search(parse_args(invocation.tool_call.arguments)?)
                    .await
            }
            FileAction::Mkdir => {
                self.mkdir(parse_args(invocation.tool_call.arguments)?)
                    .await
            }
            FileAction::Write => {
                self.write(parse_args(invocation.tool_call.arguments)?)
                    .await
            }
            FileAction::Copy => {
                self.copy_move(parse_args(invocation.tool_call.arguments)?, false)
                    .await
            }
            FileAction::Move => {
                self.copy_move(parse_args(invocation.tool_call.arguments)?, true)
                    .await
            }
            FileAction::Delete => {
                self.delete(parse_args(invocation.tool_call.arguments)?)
                    .await
            }
        }
    }
}

impl FileTool {
    async fn list(&self, args: ListArgs) -> Result<ToolResult, ToolError> {
        let path = existing_path(&self.policy, &args.path)?;
        let max_entries = positive_limit(args.max_entries, 200, 2_000, "max_entries")?;
        blocking(move || {
            let metadata = fs::metadata(&path).map_err(|error| invalid(error.to_string()))?;
            if !metadata.is_dir() {
                return Err(invalid("The fs.list target must be a directory"));
            }
            let mut entries = Vec::new();
            let mut truncated = false;
            for entry in fs::read_dir(&path).map_err(|error| invalid(error.to_string()))? {
                let entry = entry.map_err(|error| invalid(error.to_string()))?;
                if entries.len() >= max_entries {
                    truncated = true;
                    break;
                }
                let file_type = entry.file_type().map_err(|error| invalid(error.to_string()))?;
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": entry.path().to_string_lossy(),
                    "kind": if file_type.is_dir() { "directory" } else if file_type.is_symlink() { "symlink" } else { "file" },
                }));
            }
            Ok(ToolResult {
                summary: format!("Listed {} entries: {}", entries.len(), path.display()),
                data: json!({"path": path, "entries": entries}),
                truncated,
            })
        })
        .await
    }

    async fn stat(&self, args: PathArgs) -> Result<ToolResult, ToolError> {
        let path = existing_entry(&self.policy, &args.path)?;
        blocking(move || {
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| invalid(error.to_string()))?;
            let kind = if metadata.is_dir() {
                "directory"
            } else if metadata.file_type().is_symlink() {
                "symlink"
            } else {
                "file"
            };
            Ok(ToolResult {
                summary: format!("Read file metadata: {}", path.display()),
                data: json!({
                    "path": path,
                    "kind": kind,
                    "size_bytes": metadata.len(),
                    "readonly": metadata.permissions().readonly(),
                }),
                truncated: false,
            })
        })
        .await
    }

    async fn read(&self, args: ReadArgs) -> Result<ToolResult, ToolError> {
        let path = existing_path(&self.policy, &args.path)?;
        let limit = positive_limit(
            args.max_bytes,
            self.policy.max_file_bytes,
            self.policy.max_file_bytes,
            "max_bytes",
        )?;
        blocking(move || {
            let metadata = fs::metadata(&path).map_err(|error| invalid(error.to_string()))?;
            if !metadata.is_file() {
                return Err(invalid("The fs.read target must be a file"));
            }
            let (bytes, truncated) = read_limited_file(&path, limit)?;
            let content = String::from_utf8_lossy(&bytes).into_owned();
            Ok(ToolResult {
                summary: format!("Read file: {}", path.display()),
                data: json!({"path": path, "content": content, "size_bytes": bytes.len()}),
                truncated,
            })
        })
        .await
    }

    async fn find(&self, args: FindArgs) -> Result<ToolResult, ToolError> {
        let root = existing_path(&self.policy, &args.path)?;
        let name = args.name.unwrap_or_default();
        if name.chars().any(char::is_control) || name.len() > 4_096 {
            return Err(invalid(
                "File name filter must not contain control characters or exceed 4096 bytes",
            ));
        }
        let max_results = positive_limit(args.max_results, 200, 2_000, "max_results")?;
        let max_scanned_paths = self.policy.max_scanned_paths.max(1);
        blocking(move || {
            let mut files = Vec::new();
            let scan_truncated = collect_paths(&root, &mut files, max_scanned_paths)?;
            let mut matching = files.into_iter().filter(|path| {
                name.is_empty()
                    || path
                        .file_name()
                        .is_some_and(|file| file.to_string_lossy().contains(&name))
            });
            let results: Vec<String> = matching
                .by_ref()
                .take(max_results)
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            let truncated = scan_truncated || matching.next().is_some();
            Ok(ToolResult {
                summary: format!("Found {} paths", results.len()),
                data: json!({"root": root, "results": results}),
                truncated,
            })
        })
        .await
    }

    async fn search(&self, args: SearchArgs) -> Result<ToolResult, ToolError> {
        if args.pattern.is_empty() {
            return Err(invalid("Search pattern must not be empty"));
        }
        if args.pattern.chars().any(char::is_control) || args.pattern.len() > 4_096 {
            return Err(invalid(
                "Search pattern must not contain control characters or exceed 4096 bytes",
            ));
        }
        let root = existing_path(&self.policy, &args.path)?;
        let pattern = args.pattern;
        let max_results = positive_limit(args.max_results, 100, 1_000, "max_results")?;
        let max_file_bytes = self.policy.max_file_bytes;
        let max_scanned_paths = self.policy.max_scanned_paths.max(1);
        blocking(move || {
            let mut files = Vec::new();
            let scan_truncated = collect_paths(&root, &mut files, max_scanned_paths)?;
            let mut results = Vec::new();
            let mut truncated = scan_truncated;
            for path in files {
                if results.len() >= max_results {
                    truncated = true;
                    break;
                }
                let metadata = fs::metadata(&path).map_err(|error| invalid(error.to_string()))?;
                if !metadata.is_file() || metadata.len() > max_file_bytes as u64 {
                    continue;
                }
                let content = fs::read_to_string(&path).unwrap_or_default();
                for (line_number, line) in content.lines().enumerate() {
                    if results.len() >= max_results {
                        truncated = true;
                        break;
                    }
                    if line.contains(&pattern) {
                        results.push(json!({
                            "path": path,
                            "line": line_number + 1,
                            "content": line,
                        }));
                    }
                }
            }
            Ok(ToolResult {
                summary: format!("Found {} matches", results.len()),
                data: json!({"root": root, "pattern": pattern, "results": results}),
                truncated,
            })
        })
        .await
    }

    async fn mkdir(&self, args: PathArgs) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        let path = new_path(&self.policy, &args.path)?;
        blocking(move || {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(invalid("Cannot create a directory through a symbolic link"));
            }
            fs::create_dir_all(&path)
                .map_err(|error| invalid(format!("Failed to create directory: {error}")))?;
            Ok(ToolResult {
                summary: format!("Created directory: {}", path.display()),
                data: json!({"path": path}),
                truncated: false,
            })
        })
        .await
    }

    async fn write(&self, args: WriteArgs) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        if args.content.len() > self.policy.max_file_bytes {
            return Err(invalid(format!(
                "File content exceeds the {} byte limit. The limit is controlled by [security].max_file_bytes in agent.toml",
                self.policy.max_file_bytes
            )));
        }
        let path = new_path(&self.policy, &args.path)?;
        let append = args.append;
        let content = args.content;
        blocking(move || {
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(invalid("Cannot write a file through a symbolic link"));
            }
            let mut options = OpenOptions::new();
            options.create(true).write(true);
            if append {
                options.append(true);
            } else {
                options.truncate(true);
            }
            let mut file = options
                .open(&path)
                .map_err(|error| invalid(format!("Failed to open target file: {error}")))?;
            file.write_all(content.as_bytes())
                .map_err(|error| invalid(format!("Failed to write file: {error}")))?;
            Ok(ToolResult {
                summary: format!("Wrote file: {}", path.display()),
                data: json!({"path": path, "bytes_written": content.len(), "append": append}),
                truncated: false,
            })
        })
        .await
    }

    async fn copy_move(
        &self,
        args: CopyMoveArgs,
        move_file: bool,
    ) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        let source = existing_entry(&self.policy, &args.source)?;
        let destination = new_path(&self.policy, &args.destination)?;
        let overwrite = args.overwrite;
        blocking(move || {
            let source_metadata = fs::symlink_metadata(&source)
                .map_err(|error| invalid(format!("Failed to read source file: {error}")))?;
            if source_metadata.file_type().is_symlink() {
                return Err(invalid("Copying or moving symbolic links is not supported"));
            }
            if !source_metadata.is_file() {
                return Err(invalid(
                    "Copy and move currently support regular files only",
                ));
            }
            if fs::symlink_metadata(&destination)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(invalid("Cannot overwrite a symbolic link target"));
            }
            if destination.exists() {
                if !overwrite {
                    return Err(invalid(
                        "Destination already exists; overwrite is not enabled",
                    ));
                }
                if destination.is_dir() {
                    return Err(invalid("Cannot overwrite a destination directory"));
                }
                fs::remove_file(&destination).map_err(|error| invalid(error.to_string()))?;
            }
            if move_file {
                fs::rename(&source, &destination)
                    .map_err(|error| invalid(format!("Failed to move file: {error}")))?;
            } else {
                fs::copy(&source, &destination)
                    .map_err(|error| invalid(format!("Failed to copy file: {error}")))?;
            }
            Ok(ToolResult {
                summary: format!(
                    "{} completed: {} -> {}",
                    if move_file { "Move" } else { "Copy" },
                    source.display(),
                    destination.display()
                ),
                data: json!({"source": source, "destination": destination}),
                truncated: false,
            })
        })
        .await
    }

    async fn delete(&self, args: DeleteArgs) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        let path = existing_entry(&self.policy, &args.path)?;
        let recursive = args.recursive;
        if self
            .policy
            .allowed_roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|canonical| canonical == path))
        {
            return Err(invalid(
                "Configured allowed root directories cannot be deleted",
            ));
        }
        blocking(move || {
            let metadata =
                fs::symlink_metadata(&path).map_err(|error| invalid(error.to_string()))?;
            if metadata.is_dir() {
                if !recursive {
                    return Err(invalid("Deleting a directory requires recursive=true"));
                }
                fs::remove_dir_all(&path)
                    .map_err(|error| invalid(format!("Failed to delete directory: {error}")))?;
            } else {
                fs::remove_file(&path)
                    .map_err(|error| invalid(format!("Failed to delete file: {error}")))?;
            }
            Ok(ToolResult {
                summary: format!("Deleted: {}", path.display()),
                data: json!({"path": path, "recursive": recursive}),
                truncated: false,
            })
        })
        .await
    }
}

fn positive_limit(
    value: Option<usize>,
    default: usize,
    maximum: usize,
    name: &str,
) -> Result<usize, ToolError> {
    let limit = value.unwrap_or(default);
    if limit == 0 {
        return Err(invalid(format!("{name} must be greater than zero")));
    }
    Ok(limit.min(maximum))
}

fn read_limited_file(path: &Path, limit: usize) -> Result<(Vec<u8>, bool), ToolError> {
    let mut file = File::open(path).map_err(|error| invalid(error.to_string()))?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| invalid(error.to_string()))?;
    let truncated = bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
    }
    Ok((bytes, truncated))
}

fn collect_paths(
    path: &Path,
    output: &mut Vec<PathBuf>,
    max_scanned_paths: usize,
) -> Result<bool, ToolError> {
    let mut pending = vec![path.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(current) = pending.pop() {
        scanned = scanned.saturating_add(1);
        if scanned > max_scanned_paths {
            return Ok(true);
        }
        let file_type = fs::symlink_metadata(&current)
            .map_err(|error| invalid(format!("Failed to read path: {error}")))?
            .file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_file() {
            output.push(current);
            continue;
        }
        if file_type.is_dir() {
            for entry in fs::read_dir(&current).map_err(|error| invalid(error.to_string()))? {
                let entry = entry.map_err(|error| invalid(error.to_string()))?;
                if !entry
                    .file_type()
                    .map_err(|error| invalid(error.to_string()))?
                    .is_symlink()
                {
                    if pending.len() >= max_scanned_paths {
                        return Ok(true);
                    }
                    pending.push(entry.path());
                }
            }
        }
    }
    Ok(false)
}
