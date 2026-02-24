use super::traits::{Tool, ToolResult};
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone)]
struct TaskItem {
    id: u64,
    title: String,
    status: TaskStatus,
}

#[derive(Debug, Default)]
struct TaskPlanState {
    tasks: Vec<TaskItem>,
    next_id: u64,
}

impl TaskPlanState {
    fn ensure_next_id_initialized(&mut self) {
        if self.next_id == 0 {
            self.next_id = 1;
        }
    }

    fn clear(&mut self) {
        self.tasks.clear();
        self.next_id = 1;
    }

    fn set_tasks(&mut self, tasks: Vec<(String, TaskStatus)>) {
        self.clear();
        for (title, status) in tasks {
            let _ = self.add_task(title, status);
        }
    }

    fn add_task(&mut self, title: String, status: TaskStatus) -> u64 {
        self.ensure_next_id_initialized();
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.tasks.push(TaskItem { id, title, status });
        id
    }

    fn update_status(&mut self, task_id: u64, status: TaskStatus) -> bool {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == task_id) {
            task.status = status;
            return true;
        }
        false
    }

    fn snapshot_json(&self) -> Value {
        let mut pending = 0_u64;
        let mut in_progress = 0_u64;
        let mut completed = 0_u64;

        let tasks: Vec<Value> = self
            .tasks
            .iter()
            .map(|task| {
                match task.status {
                    TaskStatus::Pending => pending = pending.saturating_add(1),
                    TaskStatus::InProgress => in_progress = in_progress.saturating_add(1),
                    TaskStatus::Completed => completed = completed.saturating_add(1),
                }

                json!({
                    "id": task.id,
                    "title": task.title,
                    "status": task.status.as_str(),
                })
            })
            .collect();

        json!({
            "summary": {
                "total": self.tasks.len(),
                "pending": pending,
                "in_progress": in_progress,
                "completed": completed,
            },
            "tasks": tasks
        })
    }
}

pub struct TaskPlanTool {
    security: Arc<SecurityPolicy>,
    state: Mutex<TaskPlanState>,
}

impl TaskPlanTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            state: Mutex::new(TaskPlanState {
                tasks: Vec::new(),
                next_id: 1,
            }),
        }
    }

    fn fail(message: impl Into<String>) -> ToolResult {
        ToolResult {
            success: false,
            output: String::new(),
            error: Some(message.into()),
        }
    }

    fn success(payload: Value) -> ToolResult {
        match serde_json::to_string_pretty(&payload) {
            Ok(output) => ToolResult {
                success: true,
                output,
                error: None,
            },
            Err(error) => Self::fail(format!("Failed to serialize task plan response: {error}")),
        }
    }

    fn enforce_mutation(&self, operation_name: &str) -> Option<ToolResult> {
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, operation_name)
        {
            return Some(Self::fail(error));
        }
        None
    }

    fn parse_non_empty_title(value: &Value, field: &str) -> Result<String, ToolResult> {
        let title = value
            .as_str()
            .ok_or_else(|| Self::fail(format!("'{field}' must be a string")))?
            .trim()
            .to_string();

        if title.is_empty() {
            return Err(Self::fail(format!("'{field}' must not be empty")));
        }

        Ok(title)
    }

    fn parse_status_value(raw: &Value, field: &str) -> Result<TaskStatus, ToolResult> {
        let status_raw = raw
            .as_str()
            .ok_or_else(|| Self::fail(format!("'{field}' must be a string")))?;
        TaskStatus::parse(status_raw).ok_or_else(|| {
            Self::fail(format!(
                "Invalid status '{status_raw}'. Use pending|in_progress|completed"
            ))
        })
    }

    fn parse_create_tasks(args: &Value) -> Result<Vec<(String, TaskStatus)>, ToolResult> {
        let tasks_raw = args
            .get("tasks")
            .ok_or_else(|| Self::fail("Missing 'tasks' parameter for action 'create'"))?;

        let task_entries = tasks_raw
            .as_array()
            .ok_or_else(|| Self::fail("'tasks' must be an array"))?;

        if task_entries.is_empty() {
            return Err(Self::fail(
                "'tasks' must include at least one task for action 'create'",
            ));
        }

        let mut parsed = Vec::with_capacity(task_entries.len());
        for (index, task) in task_entries.iter().enumerate() {
            if let Some(title_raw) = task.as_str() {
                let title =
                    Self::parse_non_empty_title(&Value::String(title_raw.to_string()), "title")
                        .map_err(|_| {
                            Self::fail(format!(
                                "'tasks[{index}]' must not be an empty string title"
                            ))
                        })?;
                parsed.push((title, TaskStatus::Pending));
                continue;
            }

            let task_obj = task.as_object().ok_or_else(|| {
                Self::fail(format!(
                    "'tasks[{index}]' must be either a string title or object with title/status"
                ))
            })?;

            let title = task_obj
                .get("title")
                .ok_or_else(|| Self::fail(format!("Missing 'tasks[{index}].title'")))?;
            let title = Self::parse_non_empty_title(title, &format!("tasks[{index}].title"))?;

            let status = if let Some(status_raw) = task_obj.get("status") {
                Self::parse_status_value(status_raw, &format!("tasks[{index}].status"))?
            } else {
                TaskStatus::Pending
            };

            parsed.push((title, status));
        }

        Ok(parsed)
    }

    fn handle_create(&self, args: &Value) -> ToolResult {
        let parsed = match Self::parse_create_tasks(args) {
            Ok(parsed) => parsed,
            Err(error) => return error,
        };

        let mut state = self.state.lock();
        state.set_tasks(parsed);
        Self::success(json!({
            "message": "Task plan created",
            "plan": state.snapshot_json(),
        }))
    }

    fn handle_add(&self, args: &Value) -> ToolResult {
        let title = match args.get("title") {
            Some(title) => match Self::parse_non_empty_title(title, "title") {
                Ok(title) => title,
                Err(error) => return error,
            },
            None => return Self::fail("Missing 'title' parameter for action 'add'"),
        };

        let mut state = self.state.lock();
        let id = state.add_task(title, TaskStatus::Pending);
        Self::success(json!({
            "message": "Task added",
            "task_id": id,
            "plan": state.snapshot_json(),
        }))
    }

    fn handle_update(&self, args: &Value) -> ToolResult {
        let task_id = match args.get("task_id").and_then(Value::as_u64) {
            Some(task_id) => task_id,
            None => {
                return Self::fail("Missing or invalid 'task_id' parameter for action 'update'")
            }
        };

        let status = match args.get("status") {
            Some(status_raw) => match Self::parse_status_value(status_raw, "status") {
                Ok(status) => status,
                Err(error) => return error,
            },
            None => return Self::fail("Missing 'status' parameter for action 'update'"),
        };

        let mut state = self.state.lock();
        if !state.update_status(task_id, status) {
            return Self::fail(format!("Task with id {task_id} was not found"));
        }

        Self::success(json!({
            "message": "Task updated",
            "task_id": task_id,
            "status": status.as_str(),
            "plan": state.snapshot_json(),
        }))
    }

    fn handle_list(&self) -> ToolResult {
        let state = self.state.lock();
        Self::success(json!({
            "message": "Task plan listed",
            "plan": state.snapshot_json(),
        }))
    }

    fn handle_delete(&self) -> ToolResult {
        let mut state = self.state.lock();
        state.clear();
        Self::success(json!({
            "message": "Task plan cleared",
            "plan": state.snapshot_json(),
        }))
    }
}

#[async_trait]
impl Tool for TaskPlanTool {
    fn name(&self) -> &str {
        "task_plan"
    }

    fn description(&self) -> &str {
        "Session-scoped task checklist for multi-step work. Actions: create, add, update, list, delete."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "add", "update", "list", "delete"],
                    "description": "Checklist action to run."
                },
                "tasks": {
                    "type": "array",
                    "description": "For create: array of task titles or objects {title,status}. Replaces existing checklist.",
                    "items": {
                        "oneOf": [
                            {"type": "string"},
                            {
                                "type": "object",
                                "properties": {
                                    "title": {"type": "string"},
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"]
                                    }
                                },
                                "required": ["title"]
                            }
                        ]
                    }
                },
                "title": {
                    "type": "string",
                    "description": "For add: task title."
                },
                "task_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "For update: task id from list/create/add output."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "For update: new task status."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let action = match args.get("action").and_then(Value::as_str) {
            Some(action) if !action.trim().is_empty() => action.trim().to_ascii_lowercase(),
            Some(_) => return Ok(Self::fail("'action' must not be empty")),
            None => return Ok(Self::fail("Missing 'action' parameter")),
        };

        if !matches!(action.as_str(), "list") {
            let operation_name = format!("task_plan.{action}");
            if let Some(blocked) = self.enforce_mutation(&operation_name) {
                return Ok(blocked);
            }
        }

        let result = match action.as_str() {
            "create" => self.handle_create(&args),
            "add" => self.handle_add(&args),
            "update" => self.handle_update(&args),
            "list" => self.handle_list(),
            "delete" => self.handle_delete(),
            other => Self::fail(format!(
                "Unknown action '{other}'. Use create|add|update|list|delete"
            )),
        };

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn parse_output_json(result: &ToolResult) -> Value {
        serde_json::from_str(&result.output).unwrap_or_else(|_| json!({}))
    }

    fn tool_with_security(security: Arc<SecurityPolicy>) -> TaskPlanTool {
        TaskPlanTool::new(security)
    }

    #[test]
    fn name_and_schema() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        assert_eq!(tool.name(), "task_plan");
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "action");
        assert!(schema["properties"]["tasks"].is_object());
    }

    #[tokio::test]
    async fn create_replaces_existing_list() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let first = tool
            .execute(json!({"action":"create","tasks":["step one","step two"]}))
            .await
            .unwrap();
        assert!(first.success);

        let second = tool
            .execute(json!({"action":"create","tasks":["new only"]}))
            .await
            .unwrap();
        assert!(second.success);
        let payload = parse_output_json(&second);
        assert_eq!(payload["plan"]["summary"]["total"], 1);
        assert_eq!(payload["plan"]["tasks"][0]["title"], "new only");
    }

    #[tokio::test]
    async fn create_rejects_empty_tasks() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let result = tool
            .execute(json!({"action":"create","tasks":[]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("at least one"));
    }

    #[tokio::test]
    async fn add_appends_pending_task() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let result = tool
            .execute(json!({"action":"add","title":"write tests"}))
            .await
            .unwrap();
        assert!(result.success);

        let listed = tool.execute(json!({"action":"list"})).await.unwrap();
        let payload = parse_output_json(&listed);
        assert_eq!(payload["plan"]["summary"]["total"], 1);
        assert_eq!(payload["plan"]["tasks"][0]["status"], "pending");
    }

    #[tokio::test]
    async fn update_changes_status() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let created = tool
            .execute(json!({"action":"create","tasks":["api wiring"]}))
            .await
            .unwrap();
        let task_id = parse_output_json(&created)["plan"]["tasks"][0]["id"]
            .as_u64()
            .unwrap_or(0);
        assert!(task_id > 0);

        let updated = tool
            .execute(json!({"action":"update","task_id":task_id,"status":"in_progress"}))
            .await
            .unwrap();
        assert!(updated.success);
        let payload = parse_output_json(&updated);
        assert_eq!(payload["status"], "in_progress");
    }

    #[tokio::test]
    async fn list_returns_summary_counts() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let _ = tool
            .execute(json!({"action":"create","tasks":[
                {"title":"one","status":"pending"},
                {"title":"two","status":"in_progress"},
                {"title":"three","status":"completed"}
            ]}))
            .await
            .unwrap();

        let listed = tool.execute(json!({"action":"list"})).await.unwrap();
        assert!(listed.success);
        let payload = parse_output_json(&listed);
        assert_eq!(payload["plan"]["summary"]["total"], 3);
        assert_eq!(payload["plan"]["summary"]["pending"], 1);
        assert_eq!(payload["plan"]["summary"]["in_progress"], 1);
        assert_eq!(payload["plan"]["summary"]["completed"], 1);
    }

    #[tokio::test]
    async fn delete_clears_tasks() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let _ = tool
            .execute(json!({"action":"create","tasks":["a","b"]}))
            .await
            .unwrap();

        let deleted = tool.execute(json!({"action":"delete"})).await.unwrap();
        assert!(deleted.success);
        let payload = parse_output_json(&deleted);
        assert_eq!(payload["plan"]["summary"]["total"], 0);
    }

    #[tokio::test]
    async fn readonly_blocks_all_mutation_actions() {
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = tool_with_security(readonly);

        let mutation_calls = [
            json!({"action":"create","tasks":["x"]}),
            json!({"action":"add","title":"x"}),
            json!({"action":"update","task_id":1,"status":"completed"}),
            json!({"action":"delete"}),
        ];

        for args in mutation_calls {
            let result = tool.execute(args).await.unwrap();
            assert!(!result.success);
            assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
        }
    }

    #[tokio::test]
    async fn list_allowed_in_readonly_mode() {
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = tool_with_security(readonly);
        let result = tool.execute(json!({"action":"list"})).await.unwrap();
        assert!(result.success);
        let payload = parse_output_json(&result);
        assert_eq!(payload["plan"]["summary"]["total"], 0);
    }

    #[tokio::test]
    async fn mutation_blocked_when_rate_limited() {
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = tool_with_security(limited);
        let result = tool
            .execute(json!({"action":"add","title":"should fail"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let result = tool.execute(json!({"action":"archive"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Unknown action"));
    }

    #[tokio::test]
    async fn missing_action_rejected() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let result = tool.execute(json!({"title":"x"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Missing 'action'"));
    }

    #[tokio::test]
    async fn add_requires_non_empty_title() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let result = tool
            .execute(json!({"action":"add","title":"   "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("must not be empty"));
    }

    #[tokio::test]
    async fn update_requires_task_id_and_status() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let missing_id = tool
            .execute(json!({"action":"update","status":"completed"}))
            .await
            .unwrap();
        assert!(!missing_id.success);
        assert!(missing_id
            .error
            .as_deref()
            .unwrap_or("")
            .contains("task_id"));

        let missing_status = tool
            .execute(json!({"action":"update","task_id":1}))
            .await
            .unwrap();
        assert!(!missing_status.success);
        assert!(missing_status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("status"));
    }

    #[tokio::test]
    async fn update_rejects_invalid_status_value() {
        let tool = tool_with_security(Arc::new(SecurityPolicy::default()));
        let _ = tool
            .execute(json!({"action":"create","tasks":["x"]}))
            .await
            .unwrap();
        let result = tool
            .execute(json!({"action":"update","task_id":1,"status":"done"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Invalid status"));
    }
}
