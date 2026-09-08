//! Local interactive administration console.
//!
//! The console is deliberately bound to the server process' terminal.  Commands are recorded
//! with `System` provenance where an event is appropriate, so local maintenance does not create
//! an unaudited side channel around the normal control flow.

use std::collections::HashSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

use koi_core::agent::{
    ContextAssembler, ControlExecutionRequest, ControlExecutor, DirectControlAuthority, TaskRuntime,
};
use koi_core::domain::{ControlEvent, ModelSelection, PermissionLevel, TaskId};
use koi_core::ports::EventStore;
use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::ModelProviderRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent_runtime::AgentSupervisor;

/// Returns whether the local console should start for the current invocation.
///
/// It starts automatically for an interactive terminal. `--console` forces it on and
/// `--no-console` disables it, which is useful for service managers and scripts.
pub fn console_enabled_from_args(args: impl IntoIterator<Item = String>) -> Result<bool, String> {
    let mut requested = None;
    for arg in args {
        match arg.as_str() {
            "--console" => requested = Some(true),
            "--no-console" => requested = Some(false),
            "--help" | "-h" => {
                return Err("用法：koi-server [--console | --no-console]\n\
                     --console    强制启用本地交互控制台\n\
                     --no-console 禁用本地交互控制台"
                    .into());
            }
            _ => return Err(format!("未知启动参数：{arg}")),
        }
    }
    Ok(requested.unwrap_or_else(|| io::stdin().is_terminal()))
}

/// Spawns the terminal reader without blocking the async server runtime.
pub fn spawn(
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    supervisor: Arc<AgentSupervisor>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        if let Err(error) = std::thread::Builder::new()
            .name("koi-console-input".into())
            .spawn(move || read_stdin(&sender))
        {
            tracing::error!(%error, "无法启动本地控制台输入线程");
            return;
        }
        let console = TerminalConsole::new(store, models, supervisor, shutdown.clone());

        println!("本地控制台已启用。输入 help 查看命令；日志会持续输出。\n");
        print_prompt();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                line = receiver.recv() => if let Some(line) = line {
                    match console.execute(&line).await {
                        Ok(ConsoleOutcome::Continue(message)) => {
                            if !message.is_empty() {
                                println!("{message}");
                            }
                            print_prompt();
                        }
                        Ok(ConsoleOutcome::Shutdown) => {
                            println!("正在关闭 koi-server…");
                            shutdown.cancel();
                        }
                        Err(error) => {
                            eprintln!("命令失败：{error}");
                            print_prompt();
                        }
                    }
                } else {
                    tracing::info!("本地控制台输入已关闭");
                    break;
                }
            }
        }
    })
}

fn read_stdin(sender: &mpsc::UnboundedSender<String>) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line {
            Ok(line) => {
                if sender.send(line).is_err() {
                    break;
                }
            }
            Err(error) => {
                eprintln!("读取本地控制台输入失败：{error}");
                break;
            }
        }
    }
}

fn print_prompt() {
    print!("koi> ");
    let _ = io::stdout().flush();
}

pub(crate) struct TerminalConsole {
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    supervisor: Arc<AgentSupervisor>,
    shutdown: CancellationToken,
}

pub(crate) enum ConsoleOutcome {
    Continue(String),
    Shutdown,
}

impl TerminalConsole {
    pub(crate) fn new(
        store: Arc<JsonlEventStore>,
        models: Arc<ModelProviderRegistry>,
        supervisor: Arc<AgentSupervisor>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            store,
            models,
            supervisor,
            shutdown,
        }
    }

    pub(crate) async fn execute(&self, raw: &str) -> Result<ConsoleOutcome, String> {
        let args = split_command_line(raw)?;
        let Some((command, args)) = args.split_first() else {
            return Ok(ConsoleOutcome::Continue(String::new()));
        };
        match command.to_ascii_lowercase().as_str() {
            "help" | "?" => Ok(ConsoleOutcome::Continue(help_text().into())),
            "tasks" | "list" => Ok(ConsoleOutcome::Continue(self.list_tasks().await?)),
            "status" => Ok(ConsoleOutcome::Continue(
                self.status(required_task(args)?).await?,
            )),
            "pause" => self.control_with_reason(args, "pause").await,
            "resume" => self.control_without_reason(args, "resume").await,
            "cancel" => self.control_with_reason(args, "cancel").await,
            "model" => self.model_command(args).await,
            "minimum" | "min-permission" => self.minimum_permission(args).await,
            "queue" => self.internal_lifecycle(args, "queue").await,
            "complete" => self.internal_lifecycle(args, "complete").await,
            "fail" => self.internal_lifecycle(args, "fail").await,
            "expire" => self.internal_lifecycle(args, "expire").await,
            "context" => self.context_command(args).await,
            "shutdown" | "exit" | "quit" => {
                self.shutdown.cancel();
                Ok(ConsoleOutcome::Shutdown)
            }
            _ => Err(format!("未知命令：{command}；输入 help 查看可用命令")),
        }
    }

    async fn list_tasks(&self) -> Result<String, String> {
        let task_ids = JsonlEventStore::list_task_ids(self.store.as_ref())
            .map_err(|error| error.to_string())?;
        if task_ids.is_empty() {
            return Ok("没有已持久化的任务。".into());
        }
        let mut rows = Vec::new();
        for task_id in task_ids {
            match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
                Ok(runtime) => rows.push(format!(
                    "{task_id}  {:<16?}  model={}  active={}",
                    runtime.projection().status,
                    runtime
                        .projection()
                        .selected_model
                        .as_ref()
                        .map_or_else(|| "default".to_owned(), ToString::to_string),
                    self.supervisor.is_task_active(task_id),
                )),
                Err(error) => rows.push(format!("{task_id}  <无法恢复：{error}>")),
            }
        }
        Ok(rows.join("\n"))
    }

    async fn status(&self, task_id: TaskId) -> Result<String, String> {
        let runtime = self.runtime(task_id).await?;
        let projection = runtime.projection();
        Ok(format!(
            "task={task_id}\nstatus={:?}\nminimum_permission={:?}\nmodel={}\nlast_sequence={}\nactive={}",
            projection.status,
            projection.minimum_control_permission,
            projection
                .selected_model
                .as_ref()
                .map_or_else(|| "default".to_owned(), ToString::to_string),
            projection.last_sequence,
            self.supervisor.is_task_active(task_id),
        ))
    }

    async fn control_with_reason(
        &self,
        args: &[String],
        action: &str,
    ) -> Result<ConsoleOutcome, String> {
        let task_id = required_task(args)?;
        let reason = optional_reason(&args[1..]);
        let event = match action {
            "pause" => ControlEvent::PauseRequested { reason },
            "cancel" => ControlEvent::TaskCancelled { reason },
            _ => return Err("内部控制命令无效".into()),
        };
        self.record_control(task_id, event).await?;
        Ok(ConsoleOutcome::Continue(format!(
            "已提交 {action} 控制：{task_id}"
        )))
    }

    async fn control_without_reason(
        &self,
        args: &[String],
        action: &str,
    ) -> Result<ConsoleOutcome, String> {
        let task_id = required_task(args)?;
        if args.len() > 1 {
            return Err(format!("{action} 只接受一个 task_id"));
        }
        self.record_control(task_id, ControlEvent::ResumeRequested)
            .await?;
        Ok(ConsoleOutcome::Continue(format!(
            "已提交 {action} 控制：{task_id}"
        )))
    }

    async fn model_command(&self, args: &[String]) -> Result<ConsoleOutcome, String> {
        if args
            .first()
            .is_some_and(|arg| arg.eq_ignore_ascii_case("reset"))
        {
            return self.reset_model_state(&args[1..]);
        }
        if args.len() != 3 {
            return Err("用法：model <task_id> <provider> <model_id>".into());
        }
        let task_id = parse_task_id(&args[0])?;
        let selection = ModelSelection::new(args[1].clone(), args[2].clone())
            .map_err(|error| format!("模型标识无效：{error}"))?;
        if !self.models.contains(&selection) {
            return Err(format!("未配置模型：{selection}"));
        }
        self.record_control(
            task_id,
            ControlEvent::ModelSelected {
                provider: selection.provider,
                model_id: selection.model_id,
            },
        )
        .await?;
        Ok(ConsoleOutcome::Continue(format!(
            "已切换任务模型：{task_id}"
        )))
    }

    async fn minimum_permission(&self, args: &[String]) -> Result<ConsoleOutcome, String> {
        if args.len() != 2 {
            return Err("用法：minimum <task_id> <User|Operator|Admin>".into());
        }
        let task_id = parse_task_id(&args[0])?;
        let minimum_permission = parse_permission(&args[1])?;
        self.record_control(
            task_id,
            ControlEvent::MinimumControlPermissionChanged { minimum_permission },
        )
        .await?;
        Ok(ConsoleOutcome::Continue(format!(
            "已更新最低控制权限：{task_id}"
        )))
    }

    async fn internal_lifecycle(
        &self,
        args: &[String],
        action: &str,
    ) -> Result<ConsoleOutcome, String> {
        let task_id = required_task(args)?;
        if self.supervisor.is_task_active(task_id) {
            return Err(
                "任务仍在执行；请先 pause 或 cancel，等待任务停止后再写入终结生命周期事件".into(),
            );
        }
        let reason = optional_reason(&args[1..]);
        let event = match action {
            "queue" if args.len() == 1 => ControlEvent::TaskQueued,
            "complete" => ControlEvent::TaskCompleted {
                response: (!args[1..].is_empty()).then_some(reason),
            },
            "fail" => ControlEvent::TaskFailed { reason },
            "expire" => ControlEvent::TaskExpired { reason },
            "queue" => return Err("用法：queue <task_id>".into()),
            _ => return Err("内部生命周期命令无效".into()),
        };
        self.record_control(task_id, event).await?;
        Ok(ConsoleOutcome::Continue(format!(
            "已记录 {action} 生命周期事件：{task_id}"
        )))
    }

    async fn context_command(&self, args: &[String]) -> Result<ConsoleOutcome, String> {
        let (target, all_events) = parse_context_clear_args(args)?;
        if all_events {
            if target.eq_ignore_ascii_case("all") {
                return self.clear_all_event_streams().await;
            }
            return Ok(ConsoleOutcome::Continue(
                self.clear_all_events(parse_task_id(&target)?).await?,
            ));
        }
        if target.eq_ignore_ascii_case("all") {
            let task_ids = JsonlEventStore::list_task_ids(self.store.as_ref())
                .map_err(|error| error.to_string())?;
            let mut results = Vec::new();
            for task_id in task_ids {
                match self.clear_context(task_id).await {
                    Ok(message) => results.push(message),
                    Err(error) => results.push(format!("{task_id}: 跳过（{error}）")),
                }
            }
            return Ok(ConsoleOutcome::Continue(results.join("\n")));
        }
        Ok(ConsoleOutcome::Continue(
            self.clear_context(parse_task_id(&target)?).await?,
        ))
    }

    async fn clear_all_event_streams(&self) -> Result<ConsoleOutcome, String> {
        let task_ids = JsonlEventStore::list_task_ids(self.store.as_ref())
            .map_err(|error| error.to_string())?;
        if task_ids.is_empty() {
            return Ok(ConsoleOutcome::Continue(
                "没有已持久化的任务事件流。".into(),
            ));
        }

        let mut results = Vec::new();
        for task_id in task_ids {
            match self.clear_all_events(task_id).await {
                Ok(message) => results.push(message),
                Err(error) => results.push(format!("{task_id}: 跳过（{error}）")),
            }
        }
        Ok(ConsoleOutcome::Continue(results.join("\n")))
    }

    async fn clear_all_events(&self, task_id: TaskId) -> Result<String, String> {
        let existing_events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| error.to_string())?;
        if existing_events.is_empty() {
            if task_id.is_main() {
                self.supervisor.reset_model_state(task_id);
                self.initialize_empty_main_session().await?;
                return Ok("主会话没有历史事件；已创建空会话骨架".into());
            }
            return Err("任务事件流不存在".into());
        }
        if self.supervisor.is_task_active(task_id) {
            return Err("任务正在执行；请先 pause 或 cancel，等待其停止".into());
        }
        // 先验证事件流可以恢复，避免把损坏的事件流直接当作可清理目标。
        let _runtime = self.runtime(task_id).await?;
        self.store
            .delete_task(task_id)
            .await
            .map_err(|error| error.to_string())?;
        self.supervisor.reset_model_state(task_id);

        if task_id.is_main() {
            self.initialize_empty_main_session().await?;
            tracing::warn!(
                target: "koi.audit",
                %task_id,
                "主会话历史事件已完全清理并重建空会话骨架"
            );
            Ok("主会话全部历史事件已删除，并已重建空会话骨架".into())
        } else {
            tracing::warn!(
                target: "koi.audit",
                %task_id,
                "子任务全部事件已删除，任务事件流已移除"
            );
            Ok(format!("子任务 {task_id} 的全部事件已删除；该任务已移除"))
        }
    }

    async fn initialize_empty_main_session(&self) -> Result<(), String> {
        let mut runtime = TaskRuntime::new(Arc::clone(&self.store), TaskId::MAIN);
        runtime
            .record(
                koi_core::domain::AgentEvent::control(ControlEvent::TaskCreated {
                    trigger_event_id: None,
                }),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        runtime
            .record(
                koi_core::domain::AgentEvent::control(ControlEvent::TaskQueued),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn clear_context(&self, task_id: TaskId) -> Result<String, String> {
        if self.supervisor.is_task_active(task_id) {
            return Err("任务正在执行；请先 pause 或 cancel，等待其停止".into());
        }
        let mut runtime = self.runtime(task_id).await?;
        let events = runtime
            .load_events()
            .await
            .map_err(|error| error.to_string())?;
        let context = ContextAssembler::from_events(
            task_id,
            &events,
            &HashSet::new(),
            runtime.projection().minimum_control_permission,
        )
        .map_err(|error| error.to_string())?;
        let mut dropped_context_event_ids = ContextAssembler::latest_compaction_coverage(&events)
            .into_iter()
            .collect::<Vec<_>>();
        dropped_context_event_ids.extend(context.into_iter().map(|item| item.event_id));
        dropped_context_event_ids.sort_by_key(ToString::to_string);
        dropped_context_event_ids.dedup();
        if dropped_context_event_ids.is_empty() {
            self.supervisor.reset_model_state(task_id);
            return Ok(format!(
                "{task_id}: 没有可清理的模型上下文；已重置模型续接状态"
            ));
        }
        ControlExecutor::execute(
            &mut runtime,
            ControlExecutionRequest {
                event: ControlEvent::ContextCompacted {
                    dropped_context_event_ids,
                    summary: "本地管理员已清理此前会话上下文；历史事件仍可在审计记录中查看。"
                        .into(),
                },
                authority: DirectControlAuthority::system(),
                causation_id: None,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        self.supervisor.reset_model_state(task_id);
        Ok(format!("{task_id}: 已清理模型上下文并重置模型续接状态"))
    }

    fn reset_model_state(&self, args: &[String]) -> Result<ConsoleOutcome, String> {
        if args.len() != 1 {
            return Err("用法：model reset <task_id|all>".into());
        }
        if args[0].eq_ignore_ascii_case("all") {
            let task_ids = JsonlEventStore::list_task_ids(self.store.as_ref())
                .map_err(|error| error.to_string())?;
            for task_id in &task_ids {
                self.supervisor.reset_model_state(*task_id);
            }
            return Ok(ConsoleOutcome::Continue(format!(
                "已重置 {} 个任务的模型续接状态",
                task_ids.len()
            )));
        }
        let task_id = parse_task_id(&args[0])?;
        self.supervisor.reset_model_state(task_id);
        Ok(ConsoleOutcome::Continue(format!(
            "已重置模型续接状态：{task_id}"
        )))
    }

    async fn record_control(&self, task_id: TaskId, event: ControlEvent) -> Result<(), String> {
        let mut runtime = self.runtime(task_id).await?;
        ControlExecutor::execute(
            &mut runtime,
            ControlExecutionRequest {
                event,
                authority: DirectControlAuthority::system(),
                causation_id: None,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn runtime(&self, task_id: TaskId) -> Result<TaskRuntime<Arc<JsonlEventStore>>, String> {
        TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| format!("无法恢复任务 {task_id}：{error}"))
    }
}

fn required_task(args: &[String]) -> Result<TaskId, String> {
    args.first()
        .ok_or_else(|| "缺少 task_id".to_owned())
        .and_then(|value| parse_task_id(value))
}

fn parse_context_clear_args(args: &[String]) -> Result<(String, bool), String> {
    if args
        .first()
        .is_none_or(|command| !command.eq_ignore_ascii_case("clear"))
    {
        return Err("用法：context clear <task_id|all> [--all-events]".into());
    }

    let mut target = None;
    let mut all_events = false;
    for argument in &args[1..] {
        if argument.eq_ignore_ascii_case("--all-events") {
            if all_events {
                return Err("--all-events 只能指定一次".into());
            }
            all_events = true;
        } else if target.replace(argument.clone()).is_some() {
            return Err("context clear 只能接受一个 task_id 或 all".into());
        }
    }

    target
        .map(|target| (target, all_events))
        .ok_or_else(|| "用法：context clear <task_id|all> [--all-events]".into())
}

fn parse_task_id(value: &str) -> Result<TaskId, String> {
    if value.eq_ignore_ascii_case("main") {
        return Ok(TaskId::MAIN);
    }
    Uuid::parse_str(value)
        .map(TaskId)
        .map_err(|_| format!("无效 task_id：{value}；请使用 main 或 UUID"))
}

fn parse_permission(value: &str) -> Result<PermissionLevel, String> {
    match value.to_ascii_lowercase().as_str() {
        "user" => Ok(PermissionLevel::User),
        "operator" => Ok(PermissionLevel::Operator),
        "admin" => Ok(PermissionLevel::Admin),
        _ => Err("权限必须为 User、Operator 或 Admin".into()),
    }
}

fn optional_reason(parts: &[String]) -> String {
    if parts.is_empty() {
        "由本地控制台发起".into()
    } else {
        parts.join(" ")
    }
}

fn split_command_line(raw: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in raw.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '\"' => quote = Some(character),
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if escaped {
        return Err("命令不能以未转义的反斜杠结尾".into());
    }
    if quote.is_some() {
        return Err("命令中的引号没有闭合".into());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn help_text() -> &'static str {
    "可用命令：\n\
tasks                              列出任务\n\
status <main|task_id>              查看任务状态\n\
pause <task_id> [reason]           请求暂停\n\
resume <task_id>                   恢复已暂停任务\n\
cancel <task_id> [reason]          取消任务\n\
model <task_id> <provider> <id>    切换到已配置模型\n\
minimum <task_id> <User|Operator|Admin>  修改最低控制权限\n\
queue <task_id>                    以系统身份重新入队（任务须空闲）\n\
complete|fail|expire <task_id> [reason]  写入内部生命周期事件（任务须空闲）\n\
context clear <task_id|all>        清理模型可见上下文，保留审计事件\n\
context clear <task_id|all> --all-events  删除全部历史事件（主会话会重建空骨架）\n\
model reset <task_id|all>          只重置模型供应商的续接状态\n\
shutdown                           优雅关闭服务\n\
提示：task_id 可用 main 表示主会话；含空格的原因请使用单引号或双引号。"
}

#[cfg(test)]
mod tests {
    use super::{
        console_enabled_from_args, parse_context_clear_args, parse_permission, parse_task_id,
        split_command_line,
    };
    use koi_core::domain::{PermissionLevel, TaskId};

    #[test]
    fn splits_quoted_console_arguments() {
        assert_eq!(
            split_command_line("pause main '等待外部确认' ").unwrap(),
            ["pause", "main", "等待外部确认"]
        );
    }

    #[test]
    fn parses_main_and_child_task_ids() {
        assert_eq!(parse_task_id("main").unwrap(), TaskId::MAIN);
        assert!(parse_task_id("not-a-task").is_err());
    }

    #[test]
    fn limits_console_permissions_to_external_levels() {
        assert_eq!(
            parse_permission("operator").unwrap(),
            PermissionLevel::Operator
        );
        assert!(parse_permission("system").is_err());
    }

    #[test]
    fn explicit_console_option_overrides_non_interactive_detection() {
        assert!(console_enabled_from_args(["--console".into()]).unwrap());
        assert!(!console_enabled_from_args(["--no-console".into()]).unwrap());
    }

    #[test]
    fn parses_context_clear_preserve_and_full_modes() {
        let preserve = ["clear", "main"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            parse_context_clear_args(&preserve).unwrap(),
            ("main".to_owned(), false)
        );

        let full = ["clear", "--all-events", "main"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            parse_context_clear_args(&full).unwrap(),
            ("main".to_owned(), true)
        );
    }

    #[test]
    fn rejects_ambiguous_context_clear_targets() {
        let args = ["clear", "main", "child"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(parse_context_clear_args(&args).is_err());
    }
}
