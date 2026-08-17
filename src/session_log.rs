// SessionEvent 追加日志骨架（Wave 1, todo 6）。
// Append-only session event log + derive_messages projection.
//
// 与现有 ContextHook history capture 并存，不替换：run_autonomous 在每个
// model-visible 事件点向 SessionLog 追加一条事件，同时 ContextHook 照常
// 捕获完整 history。derive_messages 从事件序列重建 Vec<Message>，用于后续
// 对照与（后续 todo）compaction 走 log。Wave 1 只建基础设施 + 投影，不改变
// model-visible 行为——SessionLog 是旁路追加，不进入 rig 的 history 通路。
use std::path::{Path, PathBuf};

use anyhow::Result;
use rig_core::OneOrMany;
use rig_core::completion::Message;
use rig_core::completion::message::AssistantContent;
use serde::{Deserialize, Serialize};

/// 一个 model-visible / 元数据事件的不可变记录。追加即不可变（append-only）。
/// An immutable record of one model-visible or metadata event. Append-only.
///
/// 变体分两类：
/// - **message-bearing**（`UserMessage` / `AssistantMessage` / `ToolCall` /
///   `ToolResult`）：`derive_messages` 将其投影为 `Message`。
/// - **metadata**（`AssistantChunk` / `RequestHeader` / `RequestContext`）：
///   流式增量或请求级元数据，`derive_messages` 跳过（不进入重建的 history）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// 用户输入消息（一次完整 user turn 的内容）。
    UserMessage { content: String },
    /// 助手流式文本增量（transient；重建 history 时跳过，最终内容由 `AssistantMessage` 承载）。
    AssistantChunk { content: String },
    /// 助手一轮的最终完整文本输出。
    AssistantMessage { content: String },
    /// 助手发起的工具调用。`id` 用于与对应 `ToolResult` 关联。
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// 工具执行结果。`call_id` 引用发起它的 `ToolCall.id`。
    ToolResult {
        id: String,
        call_id: Option<String>,
        content: String,
    },
    /// 请求级元数据：发往 API 的模型名与当前轮次。
    RequestHeader { model: String, turn: usize },
    /// 请求级元数据：进入 API 调用时的历史快照统计。
    RequestContext {
        history_len: usize,
        estimated_tokens: usize,
    },
}

/// Append-only 事件日志，持久化为 `<dir>/sessions/<id>.jsonl`（每行一个 JSON 事件）。
/// Append-only event log, persisted as `<dir>/sessions/<id>.jsonl` (one JSON event per line).
///
/// `append` 同时追加到内存 `Vec` 与磁盘 JSONL。读取用 `load_events`（按行解析，
/// 畸形行返回 `Err`，不 panic）。
pub struct SessionLog {
    session_id: String,
    events: Vec<SessionEvent>,
    dir: PathBuf,
}

impl SessionLog {
    /// 创建日志。不触碰磁盘（目录/文件在首次 `append` 时按需创建）。
    pub fn new(session_id: String, dir: PathBuf) -> Self {
        Self {
            session_id,
            events: Vec::new(),
            dir,
        }
    }

    /// 追加一个事件：写入内存 Vec 并向 JSONL 文件追加一行。
    pub fn append(&mut self, event: SessionEvent) -> Result<()> {
        let line = serde_json::to_string(&event)?;
        if let Some(parent) = self.path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?;
        writeln!(f, "{line}")?;
        self.events.push(event);
        Ok(())
    }

    /// 持久化文件路径：`<dir>/sessions/<id>.jsonl`。
    pub fn path(&self) -> PathBuf {
        self.dir
            .join("sessions")
            .join(format!("{}.jsonl", self.session_id))
    }

    /// 从 log 重建 model-visible `Vec<Message>`：message-bearing 变体投影为
    /// `Message`，metadata 变体跳过。输出应与 ContextHook 捕获的 history 一致
    /// （对照测试验证）。
    #[allow(dead_code)]
    pub fn derive_messages(&self) -> Vec<Message> {
        self.events.iter().filter_map(project_message).collect()
    }
}

/// 从 JSONL 文件按行解析事件。畸形行 → `Err`，不 panic。
/// Parse events from a JSONL file line by line. A malformed line yields `Err`, never panics.
#[allow(dead_code)]
pub fn load_events(path: &Path) -> Result<Vec<SessionEvent>> {
    let raw = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: SessionEvent = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("malformed session log line: {e}"))?;
        out.push(event);
    }
    Ok(out)
}

// derive_messages 的内部分支辅助：把单个事件投影为 0 或 1 条 Message。
#[allow(dead_code)]
fn project_message(event: &SessionEvent) -> Option<Message> {
    match event {
        SessionEvent::UserMessage { content } => Some(Message::user(content.clone())),
        SessionEvent::AssistantMessage { content } => Some(Message::assistant(content.clone())),
        SessionEvent::ToolCall {
            id,
            name,
            arguments,
        } => Some(Message::Assistant {
            id: Some(id.clone()),
            content: OneOrMany::one(AssistantContent::tool_call(
                id.clone(),
                name.clone(),
                arguments.clone(),
            )),
        }),
        SessionEvent::ToolResult {
            id,
            call_id,
            content,
        } => Some(Message::tool_result_with_call_id(
            id.clone(),
            call_id.clone(),
            content.clone(),
        )),
        // 流式增量与请求级元数据不参与 history 重建。
        SessionEvent::AssistantChunk { .. }
        | SessionEvent::RequestHeader { .. }
        | SessionEvent::RequestContext { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录 helper：每个 test 独立的 sessions 目录，避免交叉污染。
    fn temp_dir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("moye-task6-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn append_writes_jsonl() {
        // Given: an empty SessionLog in a temp dir.
        let dir = temp_dir("append_jsonl");
        let mut log = SessionLog::new("sess-1".to_string(), dir.clone());

        // When: appending two events.
        log.append(SessionEvent::UserMessage {
            content: "hello".to_string(),
        })
        .expect("append user_message");
        log.append(SessionEvent::AssistantMessage {
            content: "hi there".to_string(),
        })
        .expect("append assistant_message");

        // Then: the JSONL file exists and contains exactly 2 lines, each
        // deserializing back to the appended event, in order.
        let path = log.path();
        assert!(path.exists(), "jsonl file should exist at {:?}", path);

        let loaded = load_events(&path).expect("load_events");
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded[0],
            SessionEvent::UserMessage {
                content: "hello".to_string()
            }
        );
        assert_eq!(
            loaded[1],
            SessionEvent::AssistantMessage {
                content: "hi there".to_string()
            }
        );
    }

    #[test]
    fn derive_messages_matches_simple_history() {
        // 对照测试（简单情形）：derive_messages 输出应与 model-visible history 一致。
        let dir = temp_dir("derive_simple");
        let mut log = SessionLog::new("sess-simple".to_string(), dir);
        log.append(SessionEvent::UserMessage {
            content: "hello".to_string(),
        })
        .expect("append");
        log.append(SessionEvent::AssistantMessage {
            content: "hi there".to_string(),
        })
        .expect("append");

        let expected = vec![Message::user("hello"), Message::assistant("hi there")];
        assert_eq!(log.derive_messages(), expected);
    }

    #[test]
    fn derive_messages_matches_tool_round_trip_history() {
        // 对照测试（含工具调用）：derive_messages 重建的 user→assistant(toolcall)
        // →user(toolresult)→assistant 序列应与等价 history 逐条相等。
        let dir = temp_dir("derive_tool");
        let mut log = SessionLog::new("sess-tool".to_string(), dir);
        log.append(SessionEvent::UserMessage {
            content: "read the file".to_string(),
        })
        .expect("append user");
        log.append(SessionEvent::ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path":"x"}),
        })
        .expect("append tool_call");
        log.append(SessionEvent::ToolResult {
            id: "r1".to_string(),
            call_id: Some("c1".to_string()),
            content: "file contents".to_string(),
        })
        .expect("append tool_result");
        log.append(SessionEvent::AssistantMessage {
            content: "done".to_string(),
        })
        .expect("append assistant");

        let expected = vec![
            Message::user("read the file"),
            // Assistant message carrying the tool call; id mirrors the call id.
            Message::Assistant {
                id: Some("c1".to_string()),
                content: OneOrMany::one(AssistantContent::tool_call(
                    "c1",
                    "read_file",
                    serde_json::json!({"path":"x"}),
                )),
            },
            // User message carrying the tool result, correlated by call_id.
            Message::tool_result_with_call_id("r1", Some("c1".to_string()), "file contents"),
            Message::assistant("done"),
        ];
        assert_eq!(log.derive_messages(), expected);
    }

    #[test]
    fn derive_messages_skips_metadata_events() {
        // metadata 变体（chunk/header/context）不应产生 Message。
        let dir = temp_dir("derive_meta");
        let mut log = SessionLog::new("sess-meta".to_string(), dir);
        log.append(SessionEvent::RequestHeader {
            model: "test-model".to_string(),
            turn: 0,
        })
        .expect("append header");
        log.append(SessionEvent::AssistantChunk {
            content: "partial".to_string(),
        })
        .expect("append chunk");
        log.append(SessionEvent::RequestContext {
            history_len: 0,
            estimated_tokens: 4,
        })
        .expect("append context");
        assert!(log.derive_messages().is_empty());
    }

    #[test]
    fn malformed_input_parse_does_not_panic() {
        // 对抗类 malformed_input：畸形 JSON 行必须让 load_events 返回 Err，不 panic。
        let dir = temp_dir("malformed");
        let mut log = SessionLog::new("sess-bad".to_string(), dir.clone());
        // Append one valid event so the file exists.
        log.append(SessionEvent::UserMessage {
            content: "ok".to_string(),
        })
        .expect("append");
        // Corrupt the file by appending a malformed line.
        use std::io::Write;
        let path = log.path();
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open for append");
            writeln!(f, "{{ this is not valid json").expect("write malformed");
        }
        // load_events must error, not panic.
        let result = std::panic::catch_unwind(|| load_events(&path));
        assert!(
            result.is_ok(),
            "load_events must not panic on malformed JSON"
        );
        assert!(
            result.unwrap().is_err(),
            "load_events must return Err on a malformed line"
        );
    }
}
