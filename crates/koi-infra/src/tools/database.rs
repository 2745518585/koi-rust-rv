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
enum DatabaseAction {
    Status,
    QueryReadonly,
}

struct DatabaseTool {
    definition: ToolDefinition,
    action: DatabaseAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetArgs {
    engine: String,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    engine: String,
    target: String,
    query: String,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "database.status",
            "检查 allowlist 数据库目标是否可用。",
            DatabaseAction::Status,
        ),
        (
            "database.query_readonly",
            "执行受限的数据库只读查询。",
            DatabaseAction::QueryReadonly,
        ),
    ]
    .into_iter()
    .map(|(name, description, action)| {
        let schema = if matches!(action, DatabaseAction::Status) {
            json!({"type":"object","required":["engine","target"],"properties":{"engine":{"type":"string","enum":["postgres","mysql","sqlite","redis","mongodb"]},"target":{"type":"string","minLength":1}},"additionalProperties":false})
        } else {
            json!({"type":"object","required":["engine","target","query"],"properties":{"engine":{"type":"string","enum":["postgres","mysql","sqlite"]},"target":{"type":"string","minLength":1},"query":{"type":"string","minLength":1}},"additionalProperties":false})
        };
        Arc::new(DatabaseTool {
            definition: definition(
                name,
                description,
                schema,
                PermissionLevel::User,
                ToolSideEffect::ReadOnly,
                60_000,
                true,
            ),
            action,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[async_trait]
impl ToolExecutor for DatabaseTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            DatabaseAction::Status => {
                let args: TargetArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_database_target(&args.target)?;
                validate_target_credentials(&args.engine, &args.target)?;
                let (program, command) = status_command(&args.engine, &args.target, &self.policy)?;
                let output = self
                    .runner
                    .run(
                        CommandSpec {
                            program,
                            args: command,
                            cwd: None,
                            stdin: None,
                            requires_sudo: false,
                        },
                        self.definition.timeout_ms,
                        cancel,
                    )
                    .await?;
                Ok(command_result("数据库状态", &output))
            }
            DatabaseAction::QueryReadonly => {
                let args: QueryArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_database_target(&args.target)?;
                validate_target_credentials(&args.engine, &args.target)?;
                validate_readonly_query(&args.engine, &args.query)?;
                let (program, command, cwd) =
                    query_command(&args.engine, &args.target, &args.query, &self.policy)?;
                let output = self
                    .runner
                    .run(
                        CommandSpec {
                            program,
                            args: command,
                            cwd,
                            stdin: None,
                            requires_sudo: false,
                        },
                        self.definition.timeout_ms,
                        cancel,
                    )
                    .await?;
                ensure_success("数据库只读查询", &output)?;
                Ok(command_result("数据库只读查询", &output))
            }
        }
    }
}

fn status_command(
    engine: &str,
    target: &str,
    policy: &ToolPolicy,
) -> Result<(String, Vec<String>), ToolError> {
    match engine {
        "postgres" => Ok(("pg_isready".into(), vec!["--dbname".into(), target.into()])),
        "mysql" => Ok((
            "mysqladmin".into(),
            vec![
                "--no-defaults".into(),
                "ping".into(),
                "--database".into(),
                target.into(),
            ],
        )),
        "redis" => Ok(("redis-cli".into(), redis_status_args(target))),
        "mongodb" => Ok((
            "mongosh".into(),
            vec![
                target.into(),
                "--quiet".into(),
                "--eval".into(),
                "db.runCommand({ping:1})".into(),
            ],
        )),
        "sqlite" => {
            let path = existing_path(policy, target)?;
            Ok((
                "sqlite3".into(),
                vec![
                    "-readonly".into(),
                    path.to_string_lossy().into_owned(),
                    "PRAGMA quick_check;".into(),
                ],
            ))
        }
        _ => Err(invalid("不支持的数据库引擎")),
    }
}

fn query_command(
    engine: &str,
    target: &str,
    query: &str,
    policy: &ToolPolicy,
) -> Result<(String, Vec<String>, Option<std::path::PathBuf>), ToolError> {
    match engine {
        "postgres" => Ok((
            "psql".into(),
            vec![
                "--no-psqlrc".into(),
                "--set=ON_ERROR_STOP=1".into(),
                "--tuples-only".into(),
                "--no-align".into(),
                "--dbname".into(),
                target.into(),
                "--command".into(),
                readonly_postgres_query(query),
            ],
            None,
        )),
        "mysql" => Ok((
            "mysql".into(),
            vec![
                "--no-defaults".into(),
                "--batch".into(),
                "--raw".into(),
                "--skip-column-names".into(),
                "--database".into(),
                target.into(),
                "--execute".into(),
                readonly_mysql_query(query),
            ],
            None,
        )),
        "sqlite" => {
            let path = existing_path(policy, target)?;
            Ok((
                "sqlite3".into(),
                vec![
                    "-readonly".into(),
                    path.to_string_lossy().into_owned(),
                    query.into(),
                ],
                None,
            ))
        }
        _ => Err(invalid("只读查询只支持 postgres、mysql 或 sqlite")),
    }
}

fn validate_readonly_query(engine: &str, query: &str) -> Result<(), ToolError> {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.len() > 32_000 || trimmed.chars().any(char::is_control) {
        return Err(invalid(
            "查询不能为空、不能超过 32000 字节且不能包含控制字符",
        ));
    }
    let without_trailing_semicolon = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if without_trailing_semicolon.contains(';') {
        return Err(invalid("只读查询不允许包含多条 SQL 语句"));
    }
    let upper = without_trailing_semicolon.to_ascii_uppercase();
    let allowed = match engine {
        "postgres" | "mysql" => [
            "SELECT ",
            "WITH ",
            "SHOW ",
            "DESCRIBE ",
            "DESC ",
            "EXPLAIN ",
        ]
        .as_slice(),
        "sqlite" => ["SELECT ", "WITH ", "EXPLAIN ", "PRAGMA "].as_slice(),
        _ => return Err(invalid("不支持的数据库引擎")),
    };
    if !allowed.iter().any(|prefix| upper.starts_with(prefix)) {
        return Err(invalid(
            "只读查询必须以 SELECT、WITH、SHOW、DESCRIBE、EXPLAIN 或 PRAGMA 开头",
        ));
    }
    let forbidden = [
        "INSERT ",
        "UPDATE ",
        "DELETE ",
        "DROP ",
        "ALTER ",
        "CREATE ",
        "REPLACE ",
        "TRUNCATE ",
        "GRANT ",
        "REVOKE ",
        "ATTACH ",
        "DETACH ",
        "VACUUM ",
        "PRAGMA JOURNAL_MODE",
        "PRAGMA WAL_CHECKPOINT",
    ];
    if forbidden.iter().any(|keyword| upper.contains(keyword)) {
        return Err(invalid("查询包含可能改变数据或数据库状态的关键字"));
    }
    if let Some(pragma) = upper.strip_prefix("PRAGMA ") {
        validate_readonly_pragma(pragma)?;
    }
    Ok(())
}

fn validate_target_credentials(engine: &str, target: &str) -> Result<(), ToolError> {
    let lower = target.to_ascii_lowercase();
    if target.trim().is_empty()
        || target.starts_with('-')
        || target.len() > 4_096
        || target
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid(format!("{engine} 数据库目标格式无效")));
    }
    if [
        "password=",
        "passwd=",
        "token=",
        "secret=",
        "api_key=",
        "apikey=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return Err(invalid(format!(
            "{engine} 数据库目标不能包含内嵌凭据或令牌"
        )));
    }
    if let Ok(url) = reqwest::Url::parse(target)
        && (!url.username().is_empty() || url.password().is_some())
    {
        return Err(invalid("数据库目标 URL 不能包含用户名或密码"));
    }
    Ok(())
}

fn redis_status_args(target: &str) -> Vec<String> {
    if target.starts_with("redis://") || target.starts_with("rediss://") {
        vec!["--url".into(), target.into(), "--raw".into(), "ping".into()]
    } else if let Some((host, port)) = target.rsplit_once(':')
        && port.parse::<u16>().is_ok()
        && !host.is_empty()
    {
        vec![
            "--raw".into(),
            "-h".into(),
            host.into(),
            "-p".into(),
            port.into(),
            "ping".into(),
        ]
    } else {
        vec!["--raw".into(), "-h".into(), target.into(), "ping".into()]
    }
}

fn readonly_postgres_query(query: &str) -> String {
    format!("BEGIN READ ONLY; {}; ROLLBACK;", query_body(query))
}

fn readonly_mysql_query(query: &str) -> String {
    format!(
        "START TRANSACTION READ ONLY; {}; ROLLBACK;",
        query_body(query)
    )
}

fn query_body(query: &str) -> &str {
    query
        .trim()
        .strip_suffix(';')
        .unwrap_or(query.trim())
        .trim()
}

fn validate_readonly_pragma(pragma: &str) -> Result<(), ToolError> {
    if pragma.contains('=') {
        return Err(invalid("只读查询不允许修改 SQLite PRAGMA 设置"));
    }
    let name = pragma
        .split(['(', ' ', '\t'])
        .next()
        .unwrap_or_default()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim_matches(['`', '"', '[', ']']);
    let allowed = [
        "APPLICATION_ID",
        "COLLATION_LIST",
        "COMPILE_OPTIONS",
        "DATABASE_LIST",
        "DATA_VERSION",
        "FOREIGN_KEY_LIST",
        "FREELIST_COUNT",
        "FUNCTION_LIST",
        "INDEX_INFO",
        "INDEX_LIST",
        "INDEX_XINFO",
        "INTEGRITY_CHECK",
        "MODULE_LIST",
        "PAGE_COUNT",
        "PAGE_SIZE",
        "QUICK_CHECK",
        "SCHEMA_VERSION",
        "TABLE_INFO",
        "TABLE_XINFO",
        "USER_VERSION",
    ];
    if allowed.contains(&name) {
        Ok(())
    } else {
        Err(invalid(format!(
            "SQLite PRAGMA 不在只读 allowlist 中：{name}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        readonly_mysql_query, readonly_postgres_query, redis_status_args, validate_readonly_query,
        validate_target_credentials,
    };

    #[test]
    fn readonly_query_policy_accepts_select_and_rejects_mutations() {
        assert!(validate_readonly_query("postgres", "SELECT 1;").is_ok());
        assert!(
            validate_readonly_query("postgres", "WITH rows AS (SELECT 1) SELECT * FROM rows")
                .is_ok()
        );
        assert!(validate_readonly_query("postgres", "UPDATE services SET state = 'up'").is_err());
        assert!(validate_readonly_query("postgres", "SELECT 1; DELETE FROM services").is_err());
        assert!(validate_readonly_query("sqlite", "PRAGMA journal_mode = WAL").is_err());
        assert!(validate_readonly_query("sqlite", "PRAGMA table_info('services')").is_ok());
        assert!(validate_readonly_query("sqlite", "PRAGMA writable_schema").is_err());
    }

    #[test]
    fn database_targets_do_not_accept_embedded_credentials() {
        assert!(validate_target_credentials("postgres", "postgres://user:pass@db/app").is_err());
        assert!(validate_target_credentials("mongodb", "mongodb://user:pass@db/app").is_err());
        assert!(validate_target_credentials("redis", "redis://:secret@cache").is_err());
        assert!(validate_target_credentials("postgres", "postgres://db/app").is_ok());
    }

    #[test]
    fn cli_queries_are_wrapped_in_read_only_transactions() {
        assert_eq!(
            readonly_postgres_query("SELECT 1;"),
            "BEGIN READ ONLY; SELECT 1; ROLLBACK;"
        );
        assert_eq!(
            readonly_mysql_query("SELECT 1"),
            "START TRANSACTION READ ONLY; SELECT 1; ROLLBACK;"
        );
        assert_eq!(
            redis_status_args("redis://cache:6379"),
            ["--url", "redis://cache:6379", "--raw", "ping"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
    }
}
