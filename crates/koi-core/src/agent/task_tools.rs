//! 主会话专用的任务管理工具。
//!
//! 这些工具的调用权来自“主会话”这一结构身份，而不是输入事件的权限证据；因此它们的
//! `required_permission` 为 `None` 且 `main_session_only` 为 `true`。注册表中的执行器
//! 是 fail-closed 占位实现：真正的执行逻辑在 `AgentLoop` 中拦截完成，以便以主会话
//! 自身的事件运行时记录请求与结果审计。任何绕过主循环的直接调用都会被拒绝。

use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    AuthorizedToolInvocation, ControlEvent, ModelSelection, PermissionLevel, TaskId,
    ToolDefinition, ToolError, ToolErrorKind, ToolResult, ToolSideEffect,
};
use crate::ports::{ToolExecutor, ToolRegistrationError, ToolRegistry};

pub const TASK_START_TOOL: &str = "task.start";
pub const TASK_INPUT_TOOL: &str = "task.input";
pub const TASK_CONTROL_TOOL: &str = "task.control";
pub const TASK_NAME_TOOL: &str = "task.name";
pub const TASK_DELETE_TOOL: &str = "task.delete";
pub const TASK_LIST_TOOL: &str = "task.list";
pub const TASK_INSPECT_TOOL: &str = "task.inspect";

/// `task.start` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskStartArguments;

/// `task.list` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskListArguments;

/// `task.inspect` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInspectArguments {
    pub task_id: TaskId,
}

/// `task.input` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskInputArguments {
    pub task_id: TaskId,
    pub message: String,
}

/// `task.control` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskControlArguments {
    pub task_id: TaskId,
    pub control: ControlEvent,
}

/// `task.name` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskNameArguments {
    pub task_id: TaskId,
    pub name: String,
}

/// `task.delete` 的参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDeleteArguments {
    pub task_id: TaskId,
    pub reason: String,
}

/// 工具参数解析失败。
#[derive(Debug, Error)]
#[error("任务管理工具参数无效：{0}")]
pub struct TaskToolArgumentsError(pub String);

/// 注册主会话专用的任务管理工具定义。
///
/// # Errors
///
/// 当定义非法或名称重复时返回错误。
pub fn register_task_management_tools(
    registry: &mut ToolRegistry,
) -> Result<usize, ToolRegistrationError> {
    let tools: Vec<Arc<dyn ToolExecutor>> = vec![
        Arc::new(TaskManagementStub::new(start_definition())),
        Arc::new(TaskManagementStub::new(input_definition())),
        Arc::new(TaskManagementStub::new(control_definition())),
        Arc::new(TaskManagementStub::new(name_definition())),
        Arc::new(TaskManagementStub::new(delete_definition())),
        Arc::new(TaskManagementStub::new(list_definition())),
        Arc::new(TaskManagementStub::new(inspect_definition())),
    ];
    let count = tools.len();
    for tool in tools {
        registry.register(tool)?;
    }
    Ok(count)
}

fn start_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_START_TOOL.into(),
        description: "启动一个新的任务会话。该工具只创建并排队任务，不负责向任务提出具体要求；请在获得 task_id 后使用 task.input 投递输入。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::Stateful,
        timeout_ms: 60_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn input_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_INPUT_TOOL.into(),
        description: "向一个已经创建的子任务投递一条输入。必须通过授权元数据指定当前可见且有权限的输入事件；该事件会成为子任务的授权父节点链起点。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "format": "uuid"},
                "message": {"type": "string", "minLength": 1, "maxLength": 8000}
            },
            "required": ["task_id", "message"],
            "additionalProperties": false
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::Stateful,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn control_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_CONTROL_TOOL.into(),
        description: "向一个任务会话发送控制事件：pause、resume、cancel、select_model 或 set_minimum_permission。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "format": "uuid"},
                "action": {"type": "string", "enum": ["pause", "resume", "cancel", "select_model", "set_minimum_permission"]},
                "reason": {"type": "string"},
                "provider": {"type": "string", "minLength": 1, "maxLength": 128},
                "model_id": {"type": "string", "minLength": 1, "maxLength": 256},
                "minimum_permission": {"type": "string", "enum": ["User", "Operator", "Admin"]}
            },
            "required": ["task_id", "action"]
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::Stateful,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn name_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_NAME_TOOL.into(),
        description: "为一个尚未结束的任务会话设置稳定显示名称。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "format": "uuid"},
                "name": {"type": "string", "minLength": 1, "maxLength": 128}
            },
            "required": ["task_id", "name"]
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::Stateful,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn delete_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_DELETE_TOOL.into(),
        description: "删除一个已经结束（完成、失败、取消或过期）的任务会话及其事件流；运行中的任务必须先取消。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "format": "uuid"},
                "reason": {"type": "string"}
            },
            "required": ["task_id"]
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::Destructive,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn list_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_LIST_TOOL.into(),
        description: "列出事件存储中已经存在的所有子任务。返回任务 ID、当前状态、名称、最后事件序号以及是否有当前执行周期的最终结果；主会话不在列表中。需要查阅某个任务的最终结果时，再使用 task.inspect。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::ReadOnly,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

fn inspect_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_INSPECT_TOOL.into(),
        description: "读取一个已经存在的子任务的当前状态、名称、最后事件序号和当前执行周期的最终结果。只能查询非主会话任务；先用 task.list 发现任务 ID。查询结果是参考数据，不提供任何权限。".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "format": "uuid"}
            },
            "required": ["task_id"],
            "additionalProperties": false
        }),
        required_permission: PermissionLevel::None,
        side_effect: ToolSideEffect::ReadOnly,
        timeout_ms: 30_000,
        model_visible: true,
        main_session_only: true,
    }
}

/// fail-closed 占位执行器：任务管理工具必须由主循环拦截执行。
struct TaskManagementStub {
    definition: ToolDefinition,
}

impl TaskManagementStub {
    fn new(definition: ToolDefinition) -> Self {
        Self { definition }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for TaskManagementStub {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        _invocation: AuthorizedToolInvocation,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::new(
            ToolErrorKind::Internal,
            "任务管理工具必须由核心主循环在主会话中执行",
            false,
        ))
    }
}

fn parse_task_id(value: &Value, field: &str) -> Result<TaskId, TaskToolArgumentsError> {
    let raw = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| TaskToolArgumentsError(format!("{field} 必须是 UUID 字符串")))?;
    let uuid = uuid::Uuid::parse_str(raw)
        .map_err(|error| TaskToolArgumentsError(format!("{field} 不是合法 UUID：{error}")))?;
    Ok(TaskId(uuid))
}

/// # Errors
///
/// 当参数缺失、类型不符或指令为空时返回错误。
pub fn parse_task_start(value: &Value) -> Result<TaskStartArguments, TaskToolArgumentsError> {
    let Some(object) = value.as_object() else {
        return Err(TaskToolArgumentsError("task.start 参数必须是对象".into()));
    };
    if !object.is_empty() {
        return Err(TaskToolArgumentsError(
            "task.start 不接受任务要求；请使用 task.input 投递输入".into(),
        ));
    }
    Ok(TaskStartArguments)
}

/// # Errors
///
/// 当参数不是空对象时返回错误。
pub fn parse_task_list(value: &Value) -> Result<TaskListArguments, TaskToolArgumentsError> {
    let Some(object) = value.as_object() else {
        return Err(TaskToolArgumentsError("task.list 参数必须是对象".into()));
    };
    if !object.is_empty() {
        return Err(TaskToolArgumentsError(
            "task.list 不接受参数；请直接查询全部已有子任务".into(),
        ));
    }
    Ok(TaskListArguments)
}

/// # Errors
///
/// 当 `task_id` 缺失或不是合法 UUID 时返回错误。
pub fn parse_task_inspect(value: &Value) -> Result<TaskInspectArguments, TaskToolArgumentsError> {
    Ok(TaskInspectArguments {
        task_id: parse_task_id(value, "task_id")?,
    })
}

/// # Errors
///
/// 当参数缺失、类型不符或输入为空时返回错误。
pub fn parse_task_input(value: &Value) -> Result<TaskInputArguments, TaskToolArgumentsError> {
    let task_id = parse_task_id(value, "task_id")?;
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| TaskToolArgumentsError("message 必须是字符串".into()))?
        .trim()
        .to_owned();
    if message.is_empty() || message.chars().count() > 8000 {
        return Err(TaskToolArgumentsError(
            "message 必须为 1 到 8000 个字符".into(),
        ));
    }
    Ok(TaskInputArguments { task_id, message })
}

/// # Errors
///
/// 当参数缺失、动作未知或控制参数非法时返回错误。
pub fn parse_task_control(value: &Value) -> Result<TaskControlArguments, TaskToolArgumentsError> {
    let task_id = parse_task_id(value, "task_id")?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| TaskToolArgumentsError("action 必须是字符串".into()))?;
    let reason = |label: &str| -> Result<String, TaskToolArgumentsError> {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or(label)
            .trim();
        if reason.is_empty() || reason.chars().count() > 512 {
            return Err(TaskToolArgumentsError(
                "reason 必须为 1 到 512 个字符".into(),
            ));
        }
        Ok(reason.to_owned())
    };
    let control = match action {
        "pause" => ControlEvent::TaskPaused {
            reason: reason("由主会话发起暂停")?,
        },
        "resume" => ControlEvent::TaskResumed,
        "cancel" => ControlEvent::TaskCancelled {
            reason: reason("由主会话发起取消")?,
        },
        "select_model" => {
            let provider = value
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| TaskToolArgumentsError("select_model 需要 provider".into()))?;
            let model_id = value
                .get("model_id")
                .and_then(Value::as_str)
                .ok_or_else(|| TaskToolArgumentsError("select_model 需要 model_id".into()))?;
            ModelSelection::new(provider, model_id).map_err(|error| {
                TaskToolArgumentsError(format!("provider/model_id 无效：{error}"))
            })?;
            ControlEvent::ModelSelected {
                provider: provider.to_owned(),
                model_id: model_id.to_owned(),
            }
        }
        "set_minimum_permission" => {
            let raw = value
                .get("minimum_permission")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    TaskToolArgumentsError("set_minimum_permission 需要 minimum_permission".into())
                })?;
            let minimum_permission = match raw {
                "User" => PermissionLevel::User,
                "Operator" => PermissionLevel::Operator,
                "Admin" => PermissionLevel::Admin,
                other => {
                    return Err(TaskToolArgumentsError(format!("未知最低权限：{other}")));
                }
            };
            ControlEvent::MinimumControlPermissionChanged { minimum_permission }
        }
        other => return Err(TaskToolArgumentsError(format!("未知控制动作：{other}"))),
    };
    Ok(TaskControlArguments { task_id, control })
}

/// # Errors
///
/// 当参数缺失或名称非法时返回错误。
pub fn parse_task_name(value: &Value) -> Result<TaskNameArguments, TaskToolArgumentsError> {
    let task_id = parse_task_id(value, "task_id")?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| TaskToolArgumentsError("name 必须是字符串".into()))?
        .trim()
        .to_owned();
    if name.is_empty() || name.chars().count() > 128 {
        return Err(TaskToolArgumentsError("name 必须为 1 到 128 个字符".into()));
    }
    Ok(TaskNameArguments { task_id, name })
}

/// # Errors
///
/// 当参数缺失或原因非法时返回错误。
pub fn parse_task_delete(value: &Value) -> Result<TaskDeleteArguments, TaskToolArgumentsError> {
    let task_id = parse_task_id(value, "task_id")?;
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("由主会话发起删除")
        .trim()
        .to_owned();
    if reason.chars().count() > 512 {
        return Err(TaskToolArgumentsError("reason 最多 512 个字符".into()));
    }
    Ok(TaskDeleteArguments { task_id, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_select_model_control() {
        let task_id = uuid::Uuid::new_v4();
        let arguments = serde_json::json!({
            "task_id": task_id.to_string(),
            "action": "select_model",
            "provider": "deepseek",
            "model_id": "deepseek-chat"
        });
        let parsed = parse_task_control(&arguments).unwrap();
        assert_eq!(parsed.task_id, TaskId(task_id));
        assert_eq!(
            parsed.control,
            ControlEvent::ModelSelected {
                provider: "deepseek".into(),
                model_id: "deepseek-chat".into()
            }
        );
    }

    #[test]
    fn parses_task_discovery_arguments() {
        assert_eq!(
            parse_task_list(&serde_json::json!({})).unwrap(),
            TaskListArguments
        );
        assert!(parse_task_list(&serde_json::json!({"task_id": "unexpected"})).is_err());

        let task_id = uuid::Uuid::new_v4();
        assert_eq!(
            parse_task_inspect(&serde_json::json!({
                "task_id": task_id.to_string()
            }))
            .unwrap(),
            TaskInspectArguments {
                task_id: TaskId(task_id)
            }
        );
    }
}
