//! Codex 会话日志使用追踪
//!
//! 从 ~/.codex/sessions/ 下的 JSONL 会话文件中提取精确 token 使用数据，
//! 替代原有的 state_5.sqlite 估算方案。
//!
//! ## 数据流
//! ```text
//! ~/.codex/sessions/YYYY/MM/DD/*.jsonl → 增量解析 → delta 计算 → 费用计算 → proxy_request_logs 表
//! ```
//!
//! ## 解析的事件类型
//! - `session_meta` → 提取唯一 thread_id（子代理的 session_id 指向父线程）
//! - `turn_context` / `event_msg` (type=thread_settings_applied) → 提取当前 model 和请求速度
//! - `event_msg` (type=token_count) → 提取累计 token 用量，计算 delta
//! - `token_usage_record` → 随后的 token_count last 为 0 时补记这次用量

use crate::codex_config::get_codex_config_dir;
use crate::codex_rollout_file;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::{CostCalculator, ModelPricing};
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    estimated_first_token_ms, estimated_latency_ms, metadata_modified_nanos,
    parse_timestamp_millis, update_sync_state, update_sync_state_on_conn, EstimatedTiming,
    SessionSyncResult,
};
use crate::services::sql_helpers::{INPUT_TOKEN_SEMANTICS_LEGACY, INPUT_TOKEN_SEMANTICS_TOTAL};
use crate::services::usage_stats::{
    find_model_pricing, has_suspected_codex_session_duplicate, should_skip_session_insert, DedupKey,
};
use chrono::{DateTime, Local, TimeZone, Utc};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufRead;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::SystemTime;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
};

const CODEX_THREAD_REQUEST_ID_PREFIX: &str = "codex_session:thread-v1";

/// Codex rollout 里记录的是请求时应用的 service tier，不保证等同于上游最终
/// served tier。订阅额度估算按官方 Speed 价表使用模型对应的 Fast 倍率；
/// 无法识别的旧日志保持 Unknown，并保守使用 Standard 倍率。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexServiceTier {
    Unknown,
    Standard,
    Fast,
}

impl CodexServiceTier {
    fn from_raw(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("priority" | "fast") => Self::Fast,
            Some("default" | "standard") => Self::Standard,
            _ => Self::Unknown,
        }
    }

    fn quota_cost_multiplier(self, model: &str) -> Decimal {
        let family = ["-minimal", "-low", "-medium", "-high", "-xhigh"]
            .into_iter()
            .find_map(|suffix| model.strip_suffix(suffix))
            .unwrap_or(model);
        match self {
            Self::Fast
                if matches!(
                    family,
                    "gpt-6-astra"
                        | "gpt-6-sol"
                        | "gpt-6-luna"
                        | "gpt-5.6"
                        | "gpt-5.6-sol"
                        | "gpt-5.6-terra"
                        | "gpt-5.6-luna"
                        | "gpt-5.5"
                ) =>
            {
                Decimal::new(25, 1)
            }
            Self::Fast if matches!(family, "gpt-5.4" | "gpt-5.4-mini") => Decimal::from(2),
            Self::Fast => Decimal::from(1),
            Self::Unknown | Self::Standard => Decimal::from(1),
        }
    }
}

/// 累计 token 用量（跟踪 total_token_usage 字段）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CumulativeTokens {
    input: u64,
    cached_input: u64,
    cache_write_input: u64,
    cache_write_reported: bool,
    output: u64,
}

/// 单次 API 调用的 token 增量
#[derive(Debug)]
struct DeltaTokens {
    input: u32,
    cached_input: u32,
    cache_write_input: u32,
    cache_write_reported: bool,
    output: u32,
}

impl DeltaTokens {
    fn is_zero(&self) -> bool {
        self.input == 0 && self.cached_input == 0 && self.cache_write_input == 0 && self.output == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenCountersSignature {
    input: Option<u64>,
    cached_input: Option<u64>,
    cache_write_input: Option<u64>,
    output: Option<u64>,
    reasoning_output: Option<u64>,
    total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenUsageSignature {
    total: Option<TokenCountersSignature>,
    last: Option<TokenCountersSignature>,
}

#[derive(Debug, Clone)]
struct TimestampedTokenSignature {
    timestamp: DateTime<Utc>,
    signature: TokenUsageSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParentFileStamp {
    modified_nanos: i64,
    size: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
}

impl ParentFileStamp {
    fn from_file(file: &fs::File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        #[cfg(windows)]
        let (volume_serial, file_id) = windows_file_identity(file)?;
        Some(Self {
            modified_nanos: metadata_modified_nanos(&metadata),
            size: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(windows)]
            volume_serial,
            #[cfg(windows)]
            file_id,
        })
    }
}

#[cfg(windows)]
fn windows_file_identity(file: &fs::File) -> Option<(u64, [u8; 16])> {
    let mut information = FILE_ID_INFO::default();
    // SAFETY: `file` owns a live handle for this call, and `information` is a
    // valid writable FILE_ID_INFO buffer of the size passed to Windows.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            std::ptr::addr_of_mut!(information).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0;
    succeeded.then_some((
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

#[derive(Debug)]
struct ParentTokenTimeline {
    events: Vec<TimestampedTokenSignature>,
    max_timestamp: Option<DateTime<Utc>>,
    has_token_without_timestamp: bool,
    high_water: Option<CumulativeTokens>,
}

impl ParentTokenTimeline {
    fn signatures_before(
        &self,
        parent_path: &Path,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<TokenUsageSignature>, String> {
        if self.has_token_without_timestamp {
            return Err(format!(
                "父 rollout {} 的 token_count 缺少有效 timestamp",
                parent_path.display()
            ));
        }
        if self
            .max_timestamp
            .is_none_or(|timestamp| timestamp < cutoff)
        {
            return Err(format!(
                "父 rollout {} 尚未写到 child fork 时刻",
                parent_path.display()
            ));
        }
        Ok(self.signatures_at(cutoff))
    }

    fn signatures_at(&self, cutoff: DateTime<Utc>) -> Vec<TokenUsageSignature> {
        self.events
            .iter()
            .filter(|event| event.timestamp <= cutoff)
            .map(|event| event.signature.clone())
            .collect()
    }
}

#[derive(Debug)]
struct CachedParentTimeline {
    stamp: ParentFileStamp,
    timeline: Arc<ParentTokenTimeline>,
}

#[derive(Debug)]
struct CachedReplayPrefix {
    modified: i64,
    size: u64,
    prefix: usize,
}

#[derive(Debug)]
struct ParsedTokenEvent {
    line_offset: i64,
    signature: TokenUsageSignature,
    delta: DeltaTokens,
    event_index: Option<u32>,
    /// 不占用 `token_count` 序号的用量。远程压缩只写 `token_usage_record` 时用它，
    /// 避免把后面每次请求的 `event_index` 都往后挤。
    request_suffix: Option<String>,
    model: String,
    service_tier: CodexServiceTier,
    timestamp: Option<String>,
    /// 按事件时间戳估出来的请求耗时（含首字等待）和首字时间；估不出来为 None。
    timing: Option<EstimatedTiming>,
}

/// 只看每行开头这么多字节来判断行的种类：时间戳、类型、角色都在行头，
/// 后面的正文（工具输出可能上百 KB）不用解析。
const LINE_HEAD_BYTES: usize = 512;

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 从行头里取 `"key":"value"` 的值。只用于时间戳、类型名、角色这类不含转义的短值。
fn head_str<'a>(head: &'a [u8], key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = find_bytes(head, needle.as_bytes())? + needle.len();
    let len = head[start..].iter().position(|b| *b == b'"')?;
    std::str::from_utf8(&head[start..start + len]).ok()
}

#[derive(Debug, PartialEq, Eq)]
enum TimingLine {
    /// 下一次请求只会在它之后发出：用户消息、`turn_context`
    Boundary,
    /// 工具结果，同样是下一次请求的起点
    ToolOutput,
    /// 模型输出的一项（思考、回复、工具调用），写在这一项生成完的时刻
    ModelOutput,
    /// `token_usage_record`：响应结束时写的用量记录（新版 Codex 才有）
    UsageRecord,
    /// 一轮开始（`task_started`）或被中断（`turn_aborted`）：没等到 `token_count`
    /// 的那次请求到此作废
    TurnReset,
    /// 新版 Codex 的 `item_completed`，而且是模型输出的思考或回复：带这一项开始
    /// 生成的时刻（`started_at_ms`，即上游开始输出这一项），一次响应里最早的那项就是首字
    OutputItemStarted(i64),
}

fn classify_timing_line(line: &str) -> Option<(i64, TimingLine)> {
    let bytes = line.as_bytes();
    let head = &bytes[..bytes.len().min(LINE_HEAD_BYTES)];
    let (envelope, payload) = match find_bytes(head, b"\"payload\":{") {
        Some(at) => head.split_at(at),
        None => (head, &[][..]),
    };
    let kind = match head_str(envelope, "type")? {
        "turn_context" => TimingLine::Boundary,
        "token_usage_record" => TimingLine::UsageRecord,
        "response_item" => match head_str(payload, "type")? {
            "message" => match head_str(payload, "role")? {
                "assistant" => TimingLine::ModelOutput,
                _ => TimingLine::Boundary,
            },
            "reasoning" => TimingLine::ModelOutput,
            item if item.ends_with("_output") => TimingLine::ToolOutput,
            item if item.ends_with("_call") => TimingLine::ModelOutput,
            _ => return None,
        },
        "event_msg" => match head_str(payload, "type")? {
            "task_started" | "turn_aborted" => TimingLine::TurnReset,
            "item_completed" => output_item_started_ms(line, payload)?,
            _ => return None,
        },
        _ => return None,
    };
    let timestamp_ms = head_str(envelope, "timestamp").and_then(parse_timestamp_millis)?;
    Some((timestamp_ms, kind))
}

/// `item_completed` 行里模型输出项（思考、回复）的开始时刻。工具执行、用户消息
/// 这类项不是模型输出，不看。`started_at_ms` 写在 `item` 之后、可能超出行头，
/// 要解析整行；这类行很短（实测中位数约 400 字节）。
fn output_item_started_ms(line: &str, payload_head: &[u8]) -> Option<TimingLine> {
    let item_at = find_bytes(payload_head, b"\"item\":{")?;
    match head_str(&payload_head[item_at..], "type")? {
        "Reasoning" | "AgentMessage" => {}
        _ => return None,
    }
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let started_at_ms = value.get("payload")?.get("started_at_ms")?.as_i64()?;
    Some(TimingLine::OutputItemStarted(started_at_ms))
}

/// 按事件顺序估每次请求的耗时。Codex 日志没有请求级计时，而且各版本的
/// 事件顺序不一样：
///
/// - 结束时刻：新版在响应结束时写 `token_usage_record`，直接用它。旧版只有
///   `token_count`，有的版本在响应结束时写，有的要等工具跑完才写；后一种
///   表现为「最后一个输出项之后先出现工具结果，`token_count` 紧跟着工具结果
///   写出」，这时改用最后一个输出项的时刻（略早于真正结束，速度会略偏高）。
/// - 开始时刻：看到这次请求第一个输出项时，在它之前最近的一个起点行
///   （上一次的 `token_count`、工具结果、用户消息、`turn_context`）。不直接取
///   结束前最近的起点行，是因为工具结果可能在响应还没结束时就写进来了。
///
/// 2025 年的旧布局把上一次响应的输出项补写在它的 `token_count` 之后（时间戳
/// 和那个 `token_count` 一样）。这些输出项不属于下一次请求，直接忽略；否则
/// 下一次请求的起点会被钉在上一次响应结束的时刻，用户隔很久才发下一条消息时
/// 算出来的耗时就长得离谱。
///
/// 首字时间：新版 Codex 给每个模型输出项记了开始生成的时刻（`item_completed` 的
/// `started_at_ms`），取这次请求里最早的一项减去起点。响应一开头就是工具调用、
/// 没有思考和回复项时推不出来。
///
/// 请求被中断或出错时等不到 `token_count`，它留下的起点要在下一轮开始时清掉，
/// 否则会被下一次请求沿用。只在 `task_started` / `turn_aborted` 上清，不在用户
/// 消息上清：Codex 会在响应中途插入 developer 消息，在那里清会把起点挪晚。
#[derive(Debug, Default)]
struct RequestTimer {
    last_boundary_ms: Option<i64>,
    last_token_count_ms: Option<i64>,
    request_start_ms: Option<i64>,
    last_model_output_ms: Option<i64>,
    /// 最后一个输出项之后出现的工具结果里最晚的一个；之后再有输出项就清空。
    tool_output_after_model_output_ms: Option<i64>,
    usage_record: Option<(i64, CumulativeTokens)>,
    /// 这次请求里最早开始生成的输出项的时刻，即首字。
    first_output_ms: Option<i64>,
}

/// 两行的时间戳相差不超过这个毫秒数，就算是同一批写出的（实测相差 0–1 毫秒，
/// 而一次真实的请求不可能这么快）。
const SAME_FLUSH_SLACK_MS: i64 = 100;

impl RequestTimer {
    fn observe_line(&mut self, line: &str) {
        let Some((timestamp_ms, kind)) = classify_timing_line(line) else {
            return;
        };
        match kind {
            TimingLine::Boundary => {
                self.last_boundary_ms = self.last_boundary_ms.max(Some(timestamp_ms));
            }
            TimingLine::ToolOutput => {
                self.last_boundary_ms = self.last_boundary_ms.max(Some(timestamp_ms));
                if self.last_model_output_ms.is_some() {
                    self.tool_output_after_model_output_ms = Some(timestamp_ms);
                }
            }
            TimingLine::ModelOutput => {
                let trails_token_count = self.request_start_ms.is_none()
                    && self
                        .last_token_count_ms
                        .is_some_and(|at| timestamp_ms - at <= SAME_FLUSH_SLACK_MS);
                if trails_token_count {
                    return;
                }
                if self.request_start_ms.is_none() {
                    self.request_start_ms = self.last_boundary_ms;
                }
                self.last_model_output_ms = Some(timestamp_ms);
                self.tool_output_after_model_output_ms = None;
            }
            TimingLine::UsageRecord => {
                // 行头只够判断种类，用量要解析整行；这类行很短
                let usage = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("payload")
                            .and_then(|payload| payload.get("usage"))
                            .and_then(parse_cumulative_tokens)
                    });
                self.usage_record = usage.map(|usage| (timestamp_ms, usage));
            }
            TimingLine::OutputItemStarted(started_at_ms) => {
                self.first_output_ms = Some(
                    self.first_output_ms
                        .map_or(started_at_ms, |at| at.min(started_at_ms)),
                );
            }
            TimingLine::TurnReset => {
                *self = RequestTimer {
                    last_boundary_ms: self.last_boundary_ms.max(Some(timestamp_ms)),
                    last_token_count_ms: self.last_token_count_ms,
                    ..RequestTimer::default()
                };
            }
        }
    }

    /// 遇到一次有用量的 `token_count`：结算这次请求的耗时和首字，并把它记成下一次
    /// 请求的起点。`last` 是这次请求自己的用量，用来确认 `token_usage_record`
    /// 说的是同一次请求。
    fn finish_request(
        &mut self,
        token_count_ms: Option<i64>,
        last: Option<&CumulativeTokens>,
    ) -> Option<EstimatedTiming> {
        let start_ms = self.request_start_ms.or(self.last_boundary_ms);
        let first_output_ms = self.first_output_ms;
        let record_end_ms = self
            .usage_record
            .take()
            .filter(|(_, usage)| {
                last.is_none_or(|last| {
                    usage.input == last.input
                        && usage.cached_input == last.cached_input
                        && usage.output == last.output
                })
            })
            .map(|(timestamp_ms, _)| timestamp_ms);
        let token_count_waited_for_tools =
            match (self.tool_output_after_model_output_ms, token_count_ms) {
                (Some(tool_output_ms), Some(token_count_ms)) => {
                    token_count_ms - tool_output_ms <= SAME_FLUSH_SLACK_MS
                }
                (Some(_), None) => true,
                (None, _) => false,
            };
        let end_ms = record_end_ms.or(if token_count_waited_for_tools {
            self.last_model_output_ms
        } else {
            token_count_ms
        });

        *self = RequestTimer {
            last_boundary_ms: self.last_boundary_ms.max(token_count_ms),
            last_token_count_ms: token_count_ms,
            ..RequestTimer::default()
        };
        let start_ms = start_ms?;
        let latency_ms = estimated_latency_ms(start_ms, end_ms?)?;
        Some(EstimatedTiming {
            latency_ms,
            first_token_ms: first_output_ms.and_then(|first_output_ms| {
                estimated_first_token_ms(start_ms, first_output_ms, latency_ms)
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParentResolution {
    None,
    Parent(String),
    Continuation {
        ancestor_parent_id: Option<String>,
        history_base_id: Option<String>,
    },
    Deferred(String),
}

#[derive(Debug)]
struct ParsedCodexFile {
    root_thread_id: Option<String>,
    /// root `session_meta` 的线程 ID（稳定逻辑线程）：单段文件名与文件名
    /// UUID 一致，revert/resume 的双段文件名对应前置 UUID。
    meta_thread_id: Option<String>,
    root_meta_seen: bool,
    root_timestamp: Option<DateTime<Utc>>,
    parent: ParentResolution,
    token_events: Vec<ParsedTokenEvent>,
    /// `token_usage_record` 后面紧跟 last 全 0 的 `token_count` 时补记的用量。
    usage_record_events: Vec<ParsedTokenEvent>,
    line_offset: i64,
    has_unterminated_tail: bool,
    /// Bytes actually read, including an incomplete final record. Persisted in
    /// `last_byte_offset` only to detect file changes, never used as a seek
    /// position: parsing restarts at the beginning and `line_offset` tracks
    /// consumed records so an incomplete tail can be retried after an append.
    observed_bytes: i64,
    has_billable_tokens: bool,
}

#[derive(Debug, Clone)]
struct StagedCodexRow {
    request_id: String,
    model: String,
    request_model: Option<String>,
    pricing_model: Option<String>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    input_token_semantics: i64,
    input_cost_usd: String,
    output_cost_usd: String,
    cache_read_cost_usd: String,
    cache_write_cost_usd: String,
    total_cost_usd: String,
    session_id: String,
    cost_multiplier: String,
    created_at: i64,
}

impl StagedCodexRow {
    fn local_date(&self) -> Option<String> {
        local_date_from_unix(self.created_at)
    }
}

#[derive(Debug, Clone)]
struct StagedCodexCursor {
    file_path: String,
    last_modified: i64,
    last_line_offset: i64,
    last_byte_offset: Option<i64>,
    last_synced_at: i64,
}

#[derive(Debug, Clone)]
struct LiveCodexRowIdentity {
    request_id: String,
    thread_id: Option<String>,
    local_date: Option<String>,
    total_cost_usd: String,
    created_at: i64,
}

#[derive(Debug, Default)]
struct CodexRebuildSourceAudit {
    /// Exact source path -> (thread UUID, preflight file stamp).
    sources: HashMap<String, (String, Option<ParentFileStamp>)>,
    /// Child thread UUID -> explicitly declared parent thread UUID.
    parents: HashMap<String, String>,
    blocked_threads: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexRebuildTimestampFingerprint {
    Missing,
    Parsed(DateTime<Utc>),
    Invalid(String),
}

impl CodexRebuildTimestampFingerprint {
    fn from_raw(value: Option<&str>) -> Self {
        match value {
            None => Self::Missing,
            Some(value) => DateTime::parse_from_rfc3339(value).map_or_else(
                |_| Self::Invalid(value.to_string()),
                |timestamp| Self::Parsed(timestamp.with_timezone(&Utc)),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRebuildEventFingerprint {
    signature: TokenUsageSignature,
    input: u32,
    cached_input: u32,
    cache_write_input: u32,
    cache_write_reported: bool,
    output: u32,
    event_index: Option<u32>,
    request_suffix: Option<String>,
    model: String,
    service_tier: CodexServiceTier,
    timestamp: CodexRebuildTimestampFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRebuildFileFingerprint {
    root_meta_seen: bool,
    meta_thread_id: Option<String>,
    root_timestamp: Option<DateTime<Utc>>,
    parent: ParentResolution,
    events: Vec<CodexRebuildEventFingerprint>,
}

impl CodexRebuildFileFingerprint {
    fn from_parsed(parsed: &ParsedCodexFile) -> Self {
        let mut events: Vec<_> = parsed
            .token_events
            .iter()
            .chain(parsed.usage_record_events.iter())
            .collect();
        events.sort_by_key(|event| event.line_offset);
        Self {
            root_meta_seen: parsed.root_meta_seen,
            meta_thread_id: parsed.meta_thread_id.clone(),
            root_timestamp: parsed.root_timestamp,
            parent: parsed.parent.clone(),
            events: events
                .into_iter()
                .map(|event| CodexRebuildEventFingerprint {
                    signature: event.signature.clone(),
                    input: event.delta.input,
                    cached_input: event.delta.cached_input,
                    cache_write_input: event.delta.cache_write_input,
                    cache_write_reported: event.delta.cache_write_reported,
                    output: event.delta.output,
                    event_index: event.event_index,
                    request_suffix: event.request_suffix.clone(),
                    model: event.model.clone(),
                    service_tier: event.service_tier,
                    timestamp: CodexRebuildTimestampFingerprint::from_raw(
                        event.timestamp.as_deref(),
                    ),
                })
                .collect(),
        }
    }

    fn is_prefix_compatible_with(&self, other: &Self) -> bool {
        self.root_meta_seen == other.root_meta_seen
            && self.meta_thread_id == other.meta_thread_id
            && self.root_timestamp == other.root_timestamp
            && self.parent == other.parent
            && self.events[..self.events.len().min(other.events.len())]
                == other.events[..self.events.len().min(other.events.len())]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingReason {
    MissingParent(String),
    Stable(String),
    Retryable {
        reason: String,
        parent_id: String,
        child_file: Option<ParentFileStamp>,
        // Sample before resolving the parent: a write during resolution must
        // invalidate this entry on the next pass, not be mistaken for the
        // already-observed state. None keeps metadata/open errors retryable.
        parent_files: Option<Vec<(PathBuf, ParentFileStamp)>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingEntry {
    modified: i64,
    size: u64,
    reason: PendingReason,
}

#[derive(Debug, Default)]
struct CodexReplayCaches {
    parent_timelines: HashMap<PathBuf, CachedParentTimeline>,
    replay_prefixes: HashMap<PathBuf, CachedReplayPrefix>,
    pending: HashMap<PathBuf, PendingEntry>,
}

static CODEX_REPLAY_CACHES: OnceLock<Mutex<CodexReplayCaches>> = OnceLock::new();

fn replay_caches() -> &'static Mutex<CodexReplayCaches> {
    CODEX_REPLAY_CACHES.get_or_init(|| Mutex::new(CodexReplayCaches::default()))
}

pub(crate) fn clear_codex_replay_caches() {
    // 清空即丢弃持锁 panic 时可能写了一半的内容，所以顺带解除中毒。
    let mut caches = replay_caches()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    *caches = CodexReplayCaches::default();
    replay_caches().clear_poison();
}

fn is_rollout_filename(file_name: &str) -> bool {
    let file_name = codex_rollout_file::logical_file_name(file_name);
    if !file_name.starts_with("rollout-") || !file_name.ends_with(".jsonl") {
        return false;
    }
    let stem = file_name.trim_end_matches(".jsonl");
    stem.get(stem.len().saturating_sub(36)..)
        .is_some_and(|candidate| uuid::Uuid::parse_str(candidate).is_ok())
}

fn non_empty_string(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn thread_id_from_filename(path: &Path) -> Option<String> {
    thread_id_from_rollout_filename(path.file_name()?.to_str()?)
}

fn thread_id_from_rollout_filename(file_name: &str) -> Option<String> {
    let file_name = codex_rollout_file::logical_file_name(file_name);
    let stem = file_name.strip_suffix(".jsonl")?;
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    normalize_thread_id(candidate)
}

fn thread_id_from_cursor_path(file_path: &str) -> Option<String> {
    let file_name = file_path.rsplit(['/', '\\']).next()?;
    thread_id_from_rollout_filename(file_name)
}

fn normalize_thread_id(value: &str) -> Option<String> {
    uuid::Uuid::parse_str(value)
        .ok()
        .map(|value| value.hyphenated().to_string())
}

fn thread_identity_from_request_id(request_id: &str) -> Option<(String, u32)> {
    let rest = request_id
        .strip_prefix(CODEX_THREAD_REQUEST_ID_PREFIX)?
        .strip_prefix(':')?;
    let (thread_id, event_index) = rest.rsplit_once(':')?;
    let thread_id = normalize_thread_id(thread_id)?;
    let event_index = event_index.parse::<u32>().ok().filter(|index| *index > 0)?;
    Some((thread_id, event_index))
}

fn local_date_from_unix(created_at: i64) -> Option<String> {
    Local
        .timestamp_opt(created_at, 0)
        .single()
        .map(|value| value.format("%Y-%m-%d").to_string())
}

/// 双段文件名（`rollout-…-<threadId>_<rolloutId>.jsonl`）里下划线前的线程
/// 本体 UUID；单段文件名返回 `None`。
///
/// `thread/revert` 为同一线程新建替换 rollout 时产生这种文件名（见
/// openai/codex#38127）：末段是新生成的 rollout ID，其后的 resume 继续
/// 向该文件追加。root meta 的 `id` 始终是原线程 ID，一致性校验需同时
/// 接受两个 UUID。
fn leading_thread_id_from_filename(path: &Path) -> Option<String> {
    let logical = codex_rollout_file::logical_path(path);
    let stem = logical.file_stem()?.to_str()?;
    let len = stem.len();
    // 布局尾部：…<uuidA>_<uuidB>。uuidB 占 36 字符，其前是 '_'（共 37）
    if !stem.get(len.checked_sub(37)?..)?.starts_with('_') {
        return None;
    }
    let candidate = stem.get(len.checked_sub(73)?..len.checked_sub(37)?)?;
    uuid::Uuid::parse_str(candidate)
        .ok()
        .map(|value| value.hyphenated().to_string())
}

fn rebuild_rollout_identity(
    request_id: &str,
    session_id: Option<&str>,
    resumed_identities: &HashSet<(String, String)>,
) -> Option<String> {
    let (rollout_id, _) = thread_identity_from_request_id(request_id)?;
    let session_id = normalize_thread_id(session_id?)?;
    (rollout_id == session_id || resumed_identities.contains(&(rollout_id.clone(), session_id)))
        .then_some(rollout_id)
}

fn explicit_parent_from_meta(payload: &serde_json::Value) -> ParentResolution {
    let forked_from = non_empty_string(payload.get("forked_from_id"));
    let spawned_from = payload
        .get("source")
        .and_then(|source| source.get("subagent"))
        .and_then(|subagent| subagent.get("thread_spawn"))
        .and_then(|spawn| non_empty_string(spawn.get("parent_thread_id")));

    match (forked_from, spawned_from) {
        (None, None) => ParentResolution::None,
        (Some(parent), None) | (None, Some(parent)) => ParentResolution::Parent(parent),
        (Some(forked), Some(spawned)) if forked == spawned => ParentResolution::Parent(forked),
        (Some(forked), Some(spawned)) => ParentResolution::Deferred(format!(
            "forked_from_id ({forked}) 与 thread_spawn.parent_thread_id ({spawned}) 不一致"
        )),
    }
}

fn parse_timestamp(value: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(serde_json::Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn parse_signature_counters(value: Option<&serde_json::Value>) -> Option<TokenCountersSignature> {
    let value = value?.as_object()?;
    Some(TokenCountersSignature {
        input: value
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64),
        cached_input: value
            .get("cached_input_tokens")
            .or_else(|| value.get("cache_read_input_tokens"))
            .and_then(serde_json::Value::as_u64),
        cache_write_input: value
            .get("cache_write_input_tokens")
            .or_else(|| value.get("cache_creation_input_tokens"))
            .or_else(|| value.get("cache_creation_tokens"))
            .and_then(serde_json::Value::as_u64),
        output: value
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64),
        reasoning_output: value
            .get("reasoning_output_tokens")
            .and_then(serde_json::Value::as_u64),
        total: value
            .get("total_tokens")
            .and_then(serde_json::Value::as_u64),
    })
}

fn parse_token_signature(info: &serde_json::Value) -> Option<TokenUsageSignature> {
    let total = parse_signature_counters(info.get("total_token_usage"));
    let last = parse_signature_counters(info.get("last_token_usage"));
    (total.is_some() || last.is_some()).then_some(TokenUsageSignature { total, last })
}

fn token_snapshot_source(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("rate_limits")
        .and_then(|rate_limits| rate_limits.get("limit_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// 单个同步 pass 的共享状态。
///
/// - `cursors`：pass 开始时一次性预载的 `session_log_sync` 快照，替代逐文件
///   SELECT（尤其是游标继承要按文件名查同一 rollout 的其他路径，逐文件跑等于
///   每 pass 全表扫 N 次）。快照语义：同 pass 内其他文件刚写入的游标对后续
///   游标继承不可见——影响仅是多一轮由 request_id 去重兜底的重扫，
///   不丢数据、不双算。
/// - `pricing`：模型定价 pass 级缓存。定价表在 pass 进行中被修改时本 pass
///   仍用旧价，下一个同步 pass 生效。
struct CodexSyncPass {
    cursors: HashMap<String, (i64, i64)>,
    /// rollout 逻辑文件名（去掉 `.zst`）→ 记过游标的路径，供游标继承按名查找
    rollout_cursor_paths: HashMap<String, Vec<String>>,
    byte_offsets: HashMap<String, Option<i64>>,
    pricing: HashMap<String, Option<ModelPricing>>,
}

impl CodexSyncPass {
    fn load(db: &Database) -> Result<Self, AppError> {
        let conn = lock_conn!(db.conn);
        let mut stmt = conn
            .prepare("SELECT file_path, last_modified, last_line_offset FROM session_log_sync")
            .map_err(|e| AppError::Database(format!("预载同步游标失败: {e}")))?;
        let cursors = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?),
                ))
            })
            .and_then(|rows| rows.collect::<Result<HashMap<_, _>, _>>())
            .map_err(|e| AppError::Database(format!("预载同步游标失败: {e}")))?;
        let mut stmt = conn.prepare("SELECT file_path, last_byte_offset FROM session_log_sync")?;
        let byte_offsets = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
            })?
            .collect::<Result<HashMap<_, _>, _>>()?;
        let mut rollout_cursor_paths: HashMap<String, Vec<String>> = HashMap::new();
        for path in cursors.keys() {
            let name = codex_rollout_file::logical_file_name(path);
            if is_rollout_filename(name) {
                rollout_cursor_paths
                    .entry(name.to_string())
                    .or_default()
                    .push(path.clone());
            }
        }
        Ok(Self {
            cursors,
            rollout_cursor_paths,
            byte_offsets,
            pricing: HashMap::new(),
        })
    }
}

/// 读游标；同一个 rollout 记在别的路径下的游标读得更远时继承它。
///
/// 路径会变的情况：Codex 归档 / 取消归档会移动文件，压缩成 `.jsonl.zst`
/// 或恢复对话时解压回 `.jsonl` 会换后缀。行号只看内容，各形态通用。
/// 🔴 这是压缩文件不被重复计费的唯一屏障：request_id 去重只覆盖明细保留的
/// 30 天，而被压缩的文件至少 7 天没动、往往更老。
fn get_codex_sync_state(
    db: &Database,
    file_path: &Path,
    pass: &CodexSyncPass,
) -> Result<(i64, i64), AppError> {
    let file_path_str = file_path.to_string_lossy().to_string();
    let state = pass.cursors.get(&file_path_str).copied().unwrap_or((0, 0));
    let logical_name = codex_rollout_file::logical_file_name(&file_path_str);
    let Some(paths) = pass.rollout_cursor_paths.get(logical_name) else {
        return Ok(state);
    };
    // 与原 SQL 等价：ORDER BY last_line_offset DESC, last_modified DESC LIMIT 1
    // → 在快照上按 (offset, modified) 取最大。
    let inherited = paths
        .iter()
        .filter(|path| path.as_str() != file_path_str)
        .filter_map(|path| pass.cursors.get(path))
        .map(|&(modified, offset)| (offset, modified))
        .max();

    match inherited {
        Some((offset, modified)) if offset > state.1 => {
            update_sync_state(db, &file_path_str, modified, offset)?;
            Ok((modified, offset))
        }
        _ => Ok(state),
    }
}

/// 归一化 Codex 模型名
///
/// 处理规则（按顺序）：
/// 1. 转小写：`GLM-4.6` → `glm-4.6`
/// 2. 剥离 provider 前缀：`openai/gpt-5.4` → `gpt-5.4`
/// 3. 剥离 ISO 日期后缀：`gpt-5.4-2026-03-05` → `gpt-5.4`
/// 4. 剥离紧凑日期后缀：`gpt-5.4-20260305` → `gpt-5.4`
fn normalize_codex_model(raw: &str) -> String {
    // Step 1: 小写
    let mut name = raw.to_lowercase();

    // Step 2: 剥离 "provider/" 前缀（如 openai/, azure/）
    if let Some(pos) = name.rfind('/') {
        name = name[pos + 1..].to_string();
    }

    // Step 3: 剥离 ISO 日期后缀 -YYYY-MM-DD（正好 11 字符）
    if name.len() > 11 && name.is_char_boundary(name.len() - 11) {
        let suffix = &name[name.len() - 11..];
        if suffix.is_ascii()
            && suffix.as_bytes()[0] == b'-'
            && suffix[1..5].chars().all(|c| c.is_ascii_digit())
            && suffix.as_bytes()[5] == b'-'
            && suffix[6..8].chars().all(|c| c.is_ascii_digit())
            && suffix.as_bytes()[8] == b'-'
            && suffix[9..11].chars().all(|c| c.is_ascii_digit())
        {
            name.truncate(name.len() - 11);
        }
    }

    // Step 4: 剥离紧凑日期后缀 -YYYYMMDD（正好 9 字符）
    if name.len() > 9 {
        let parts: Vec<&str> = name.rsplitn(2, '-').collect();
        if parts.len() == 2 {
            if let Some(suffix) = parts.first() {
                if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
                    name = parts[1].to_string();
                }
            }
        }
    }

    name
}

/// 计算两次累计值之间的 delta
fn compute_delta(prev: &Option<CumulativeTokens>, current: &CumulativeTokens) -> DeltaTokens {
    match prev {
        None => DeltaTokens {
            input: current.input as u32,
            cached_input: current.cached_input as u32,
            cache_write_input: current.cache_write_input as u32,
            cache_write_reported: current.cache_write_reported,
            output: current.output as u32,
        },
        Some(p) => DeltaTokens {
            input: current.input.saturating_sub(p.input) as u32,
            cached_input: current.cached_input.saturating_sub(p.cached_input) as u32,
            cache_write_input: current
                .cache_write_input
                .saturating_sub(p.cache_write_input) as u32,
            cache_write_reported: current.cache_write_reported,
            output: current.output.saturating_sub(p.output) as u32,
        },
    }
}

fn usage_matches(last: &CumulativeTokens, usage: &CumulativeTokens) -> bool {
    last.input == usage.input
        && last.cached_input == usage.cached_input
        && last.cache_write_input == usage.cache_write_input
        && last.output == usage.output
}

fn usage_is_zero(usage: &CumulativeTokens) -> bool {
    usage.input == 0 && usage.cached_input == 0 && usage.cache_write_input == 0 && usage.output == 0
}

fn delta_from_usage(usage: &CumulativeTokens) -> DeltaTokens {
    let input = usage.input.min(u32::MAX as u64) as u32;
    let cached_input = usage.cached_input.min(usage.input).min(u32::MAX as u64) as u32;
    let fresh_input = input.saturating_sub(cached_input);
    DeltaTokens {
        input,
        cached_input,
        cache_write_input: (usage.cache_write_input.min(u32::MAX as u64) as u32).min(fresh_input),
        cache_write_reported: usage.cache_write_reported,
        output: usage.output.min(u32::MAX as u64) as u32,
    }
}

fn signature_for_last_usage(usage: &CumulativeTokens) -> TokenUsageSignature {
    TokenUsageSignature {
        total: None,
        last: Some(TokenCountersSignature {
            input: Some(usage.input),
            cached_input: Some(usage.cached_input),
            cache_write_input: usage
                .cache_write_reported
                .then_some(usage.cache_write_input),
            output: Some(usage.output),
            reasoning_output: None,
            total: Some(usage.input.saturating_add(usage.output)),
        }),
    }
}

struct PendingUsageRecord {
    key: String,
    usage: CumulativeTokens,
    timestamp: Option<String>,
    model: String,
    service_tier: CodexServiceTier,
}

fn push_unmatched_usage(
    events: &mut Vec<ParsedTokenEvent>,
    pending: PendingUsageRecord,
    confirmed_line_offset: i64,
    has_billable_tokens: &mut bool,
) {
    let delta = delta_from_usage(&pending.usage);
    if delta.is_zero() {
        return;
    }
    *has_billable_tokens = true;
    events.push(ParsedTokenEvent {
        // The zero token_count confirms this usage. If it arrives in the next
        // scan, the usage_record itself is already behind the persisted cursor.
        line_offset: confirmed_line_offset,
        signature: signature_for_last_usage(&pending.usage),
        delta,
        event_index: None,
        request_suffix: Some(format!("usage:{}", pending.key)),
        model: pending.model,
        service_tier: pending.service_tier,
        timestamp: pending.timestamp,
        timing: None,
    });
}

fn update_high_water(high_water: &mut CumulativeTokens, current: &CumulativeTokens) {
    high_water.input = high_water.input.max(current.input);
    high_water.cached_input = high_water.cached_input.max(current.cached_input);
    high_water.cache_write_input = high_water.cache_write_input.max(current.cache_write_input);
    high_water.cache_write_reported |= current.cache_write_reported;
    high_water.output = high_water.output.max(current.output);
}

fn clamp_delta_cache(mut delta: DeltaTokens) -> DeltaTokens {
    delta.cached_input = delta.cached_input.min(delta.input);
    delta.cache_write_input = delta
        .cache_write_input
        .min(delta.input.saturating_sub(delta.cached_input));
    delta
}

/// 从 JSON Value 中提取累计 token 用量
fn parse_cumulative_tokens(total_usage: &serde_json::Value) -> Option<CumulativeTokens> {
    let fields = total_usage.as_object()?;
    if ![
        "input_tokens",
        "cached_input_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "cache_creation_input_tokens",
        "cache_creation_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ]
    .iter()
    .any(|field| fields.contains_key(*field))
    {
        return None;
    }
    let cache_write = total_usage
        .get("cache_write_input_tokens")
        .or_else(|| total_usage.get("cache_creation_input_tokens"))
        .or_else(|| total_usage.get("cache_creation_tokens"))
        .and_then(serde_json::Value::as_u64);
    Some(CumulativeTokens {
        input: total_usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_input: total_usage
            .get("cached_input_tokens")
            .or_else(|| total_usage.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_write_input: cache_write.unwrap_or(0),
        cache_write_reported: cache_write.is_some(),
        output: total_usage
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

type RolloutIndex = HashMap<String, Vec<PathBuf>>;

#[derive(Debug, Default)]
struct CodexFileSyncResult {
    imported: u32,
    skipped: u32,
    suspected_duplicates: u32,
    deferred: bool,
}

/// 同步 Codex 使用数据（从 JSONL 会话日志）
pub fn sync_codex_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let codex_dir = get_codex_config_dir();
    let files = collect_codex_session_files(&codex_dir);
    let rollout_index = build_rollout_index(&files);
    let mut pass = CodexSyncPass::load(db)?;

    let mut result = SessionSyncResult {
        imported: 0,
        skipped: 0,
        files_scanned: files.len() as u32,
        suspected_duplicates: 0,
        deferred_files: 0,
        errors: vec![],
    };

    for file_path in &files {
        match sync_single_codex_file(db, file_path, &rollout_index, &mut pass) {
            Ok(file_result) => {
                result.imported = result.imported.saturating_add(file_result.imported);
                result.skipped = result.skipped.saturating_add(file_result.skipped);
                result.suspected_duplicates = result
                    .suspected_duplicates
                    .saturating_add(file_result.suspected_duplicates);
                if file_result.deferred {
                    result.deferred_files = result.deferred_files.saturating_add(1);
                }
            }
            Err(e) => {
                let msg = format!("Codex 会话文件解析失败 {}: {e}", file_path.display());
                log::warn!("[CODEX-SYNC] {msg}");
                result.errors.push(msg);
            }
        }
    }

    if result.imported > 0 || result.deferred_files > 0 {
        log::info!(
            "[CODEX-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, deferred {} 个, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.deferred_files,
            result.files_scanned
        );
    }

    Ok(result)
}

fn copy_model_pricing_to_staging(source: &Database, staging: &Database) -> Result<(), AppError> {
    let pricing_rows = {
        let conn = lock_conn!(source.conn);
        let mut statement = conn
            .prepare(
                "SELECT model_id, display_name, input_cost_per_million,
                        output_cost_per_million, cache_read_cost_per_million,
                        cache_creation_cost_per_million
                 FROM model_pricing",
            )
            .map_err(|error| AppError::Database(format!("读取模型定价失败: {error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| AppError::Database(format!("查询模型定价失败: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Database(format!("解析模型定价失败: {error}")))?;
        rows
    };

    let mut conn = lock_conn!(staging.conn);
    let transaction = conn
        .transaction()
        .map_err(|error| AppError::Database(format!("开启临时定价事务失败: {error}")))?;
    transaction
        .execute("DELETE FROM model_pricing", [])
        .map_err(|error| AppError::Database(format!("清理临时模型定价失败: {error}")))?;
    {
        let mut statement = transaction
            .prepare_cached(
                "INSERT INTO model_pricing (
                    model_id, display_name, input_cost_per_million,
                    output_cost_per_million, cache_read_cost_per_million,
                    cache_creation_cost_per_million
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|error| AppError::Database(format!("准备临时模型定价写入失败: {error}")))?;
        for row in pricing_rows {
            statement
                .execute(rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.5])
                .map_err(|error| AppError::Database(format!("写入临时模型定价失败: {error}")))?;
        }
    }
    transaction
        .commit()
        .map_err(|error| AppError::Database(format!("提交临时模型定价失败: {error}")))
}

fn audit_codex_rebuild_sources(files: &[PathBuf]) -> CodexRebuildSourceAudit {
    let mut audit = CodexRebuildSourceAudit::default();
    let mut fingerprints: HashMap<String, CodexRebuildFileFingerprint> = HashMap::new();

    for path in files {
        let Some(thread_id) = thread_id_from_filename(path) else {
            continue;
        };
        let path_text = path.to_string_lossy().to_string();
        let before = fs::File::open(path)
            .ok()
            .and_then(|file| ParentFileStamp::from_file(&file));
        let parsed = parse_codex_file_with_index(path, Some(thread_id.clone()), None);
        let after = fs::File::open(path)
            .ok()
            .and_then(|file| ParentFileStamp::from_file(&file));

        let is_stable = before.is_some() && before == after;
        let has_untrusted_billable_time = parsed.as_ref().is_ok_and(|parsed| {
            parsed.token_events.iter().any(|event| {
                event.event_index.is_some()
                    && event
                        .timestamp
                        .as_deref()
                        .is_none_or(|timestamp| DateTime::parse_from_rfc3339(timestamp).is_err())
            })
        });
        let has_unterminated_tail = parsed
            .as_ref()
            .is_ok_and(|parsed| parsed.has_unterminated_tail);
        if parsed.is_err() || !is_stable || has_untrusted_billable_time || has_unterminated_tail {
            audit.blocked_threads.insert(thread_id.clone());
        }
        if let Ok(parsed) = &parsed {
            if let ParentResolution::Parent(parent_id) = &parsed.parent {
                if audit
                    .parents
                    .get(&thread_id)
                    .is_some_and(|existing| existing != parent_id)
                {
                    audit.blocked_threads.insert(thread_id.clone());
                } else {
                    audit.parents.insert(thread_id.clone(), parent_id.clone());
                }
            }
            let fingerprint = CodexRebuildFileFingerprint::from_parsed(parsed);
            match fingerprints.get_mut(&thread_id) {
                Some(existing) if !existing.is_prefix_compatible_with(&fingerprint) => {
                    audit.blocked_threads.insert(thread_id.clone());
                }
                Some(existing) if fingerprint.events.len() > existing.events.len() => {
                    *existing = fingerprint;
                }
                Some(_) => {}
                None => {
                    fingerprints.insert(thread_id.clone(), fingerprint);
                }
            }
        }
        audit.sources.insert(path_text, (thread_id, before));
    }

    audit
}

fn finalize_codex_rebuild_audit(
    mut audit: CodexRebuildSourceAudit,
    staged_cursors: &[StagedCodexCursor],
) -> HashSet<String> {
    let staged_paths = staged_cursors
        .iter()
        .map(|cursor| cursor.file_path.as_str())
        .collect::<HashSet<_>>();

    for (path, (thread_id, expected_stamp)) in &audit.sources {
        let current_stamp = fs::File::open(path)
            .ok()
            .and_then(|file| ParentFileStamp::from_file(&file));
        if !staged_paths.contains(path.as_str())
            || expected_stamp.is_none()
            || current_stamp != *expected_stamp
        {
            audit.blocked_threads.insert(thread_id.clone());
        }
    }

    // A source created after preflight may have been picked up by the staging
    // scan. Keep its staged rows out of this rebuild; the next rebuild can
    // audit it from the beginning.
    for cursor in staged_cursors {
        if !audit.sources.contains_key(&cursor.file_path) {
            if let Some(thread_id) = thread_id_from_cursor_path(&cursor.file_path) {
                audit.blocked_threads.insert(thread_id);
            }
        }
    }

    // A child's replay prefix was derived from its parent's preflight
    // snapshot. If that parent is unsafe, every descendant calculated from it
    // is unsafe as well; propagate until the dependency graph reaches a fixed
    // point so grandchildren cannot apply stale inherited-token boundaries.
    loop {
        let blocked_before = audit.blocked_threads.len();
        for (child, parent) in &audit.parents {
            if audit.blocked_threads.contains(parent) {
                audit.blocked_threads.insert(child.clone());
            }
        }
        if audit.blocked_threads.len() == blocked_before {
            break;
        }
    }

    audit.blocked_threads
}

fn reprice_staged_codex_rows(
    conn: &rusqlite::Connection,
    rows: &mut [StagedCodexRow],
) -> Result<(), AppError> {
    let mut pricing_cache: HashMap<String, Option<ModelPricing>> = HashMap::new();
    for row in rows {
        let multiplier = row.cost_multiplier.parse::<Decimal>().map_err(|error| {
            AppError::Database(format!(
                "Codex 临时明细的 cost_multiplier 无效 ({}): {error}",
                row.request_id
            ))
        })?;
        let usage = TokenUsage {
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_creation_tokens: row.cache_write_tokens,
            cache_creation_1h_tokens: 0,
            model: Some(row.model.clone()),
            message_id: None,
        };
        let pricing = pricing_cache
            .entry(row.model.clone())
            .or_insert_with(|| find_codex_pricing(conn, &row.model));
        let (input, output, cache_read, cache_write_cost, total) = match pricing {
            Some(pricing) => {
                let cost = CostCalculator::calculate_for_app("codex", &usage, pricing, multiplier);
                (
                    cost.input_cost.to_string(),
                    cost.output_cost.to_string(),
                    cost.cache_read_cost.to_string(),
                    cost.cache_creation_cost.to_string(),
                    cost.total_cost.to_string(),
                )
            }
            None => (
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
            ),
        };
        row.input_cost_usd = input;
        row.output_cost_usd = output;
        row.cache_read_cost_usd = cache_read;
        row.cache_write_cost_usd = cache_write_cost;
        row.total_cost_usd = total;
    }
    Ok(())
}

fn load_staged_codex_rows(staging: &Database) -> Result<Vec<StagedCodexRow>, AppError> {
    let conn = lock_conn!(staging.conn);
    let mut statement = conn
        .prepare(
            "SELECT request_id, model, request_model, pricing_model,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    input_token_semantics, input_cost_usd, output_cost_usd,
                    cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
                    session_id, cost_multiplier, created_at
             FROM proxy_request_logs
             WHERE data_source = 'codex_session'
             ORDER BY request_id",
        )
        .map_err(|error| AppError::Database(format!("读取临时 Codex 明细失败: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(StagedCodexRow {
                request_id: row.get(0)?,
                model: row.get(1)?,
                request_model: row.get(2)?,
                pricing_model: row.get(3)?,
                input_tokens: row.get::<_, i64>(4)? as u32,
                output_tokens: row.get::<_, i64>(5)? as u32,
                cache_read_tokens: row.get::<_, i64>(6)? as u32,
                cache_write_tokens: row.get::<_, i64>(7)? as u32,
                input_token_semantics: row.get(8)?,
                input_cost_usd: row.get(9)?,
                output_cost_usd: row.get(10)?,
                cache_read_cost_usd: row.get(11)?,
                cache_write_cost_usd: row.get(12)?,
                total_cost_usd: row.get(13)?,
                session_id: row.get(14)?,
                cost_multiplier: row.get(15)?,
                created_at: row.get(16)?,
            })
        })
        .map_err(|error| AppError::Database(format!("查询临时 Codex 明细失败: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::Database(format!("解析临时 Codex 明细失败: {error}")))?;
    Ok(rows)
}

fn load_staged_codex_cursors(staging: &Database) -> Result<Vec<StagedCodexCursor>, AppError> {
    let conn = lock_conn!(staging.conn);
    let mut statement = conn
        .prepare(
            "SELECT file_path, last_modified, last_line_offset, last_synced_at, last_byte_offset
             FROM session_log_sync",
        )
        .map_err(|error| AppError::Database(format!("读取临时 Codex cursor 失败: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok(StagedCodexCursor {
                file_path: row.get(0)?,
                last_modified: row.get(1)?,
                last_line_offset: row.get(2)?,
                last_synced_at: row.get(3)?,
                last_byte_offset: row.get(4)?,
            })
        })
        .map_err(|error| AppError::Database(format!("查询临时 Codex cursor 失败: {error}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::Database(format!("解析临时 Codex cursor 失败: {error}")))?;
    Ok(rows)
}

fn apply_staged_codex_usage(
    db: &Database,
    mut staged_rows: Vec<StagedCodexRow>,
    staged_cursors: Vec<StagedCodexCursor>,
    blocked_threads: &HashSet<String>,
) -> Result<(u32, u32), AppError> {
    // Only completed staging cursors can establish the two-UUID relationship.
    // Group by the physical rollout ID, which also owns the request ordinals
    // and cursor; never merge different files solely by their session ID.
    // Source audit excludes changed/conflicting files before any rows apply.
    let resumed_identities = staged_cursors
        .iter()
        .filter_map(|cursor| {
            let rollout_id = thread_id_from_cursor_path(&cursor.file_path)?;
            let filename = cursor.file_path.rsplit(['/', '\\']).next()?;
            let session_id = leading_thread_id_from_filename(Path::new(filename))?;
            Some((rollout_id, session_id))
        })
        .collect::<HashSet<_>>();
    let mut conn = lock_conn!(db.conn);
    // Recalculate with the live pricing table while holding its connection
    // lock. A concurrent pricing edit cannot race between this step and commit.
    reprice_staged_codex_rows(&conn, &mut staged_rows)?;

    let live_rows = {
        let mut statement = conn
            .prepare(
                "SELECT request_id, session_id, created_at, total_cost_usd
                 FROM proxy_request_logs
                 WHERE data_source = 'codex_session'",
            )
            .map_err(|error| AppError::Database(format!("读取现有 Codex 明细失败: {error}")))?;
        let rows = statement
            .query_map([], |row| {
                let request_id = row.get::<_, String>(0)?;
                let session_id = row.get::<_, Option<String>>(1)?;
                let thread_id = rebuild_rollout_identity(
                    &request_id,
                    session_id.as_deref(),
                    &resumed_identities,
                );
                let created_at = row.get::<_, i64>(2)?;
                Ok(LiveCodexRowIdentity {
                    request_id,
                    thread_id,
                    local_date: local_date_from_unix(created_at),
                    total_cost_usd: row.get(3)?,
                    created_at,
                })
            })
            .map_err(|error| AppError::Database(format!("查询现有 Codex 明细失败: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Database(format!("解析现有 Codex 明细失败: {error}")))?;
        rows
    };

    // Rollup 没有 thread/request 维度，无法安全地只替换其中一部分。任何
    // Codex rollup 日期都作为保护区，避免 staging 明细与既有汇总双算。
    let rollup_dates = {
        let mut statement = conn
            .prepare("SELECT DISTINCT date FROM usage_daily_rollups WHERE app_type = 'codex'")
            .map_err(|error| AppError::Database(format!("读取 Codex 汇总保护日期失败: {error}")))?;
        let dates = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Database(format!("查询 Codex 汇总保护日期失败: {error}")))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|error| AppError::Database(format!("解析 Codex 汇总保护日期失败: {error}")))?;
        dates
    };
    let mut protected_dates = rollup_dates.clone();
    // Rollups only persist a local calendar date, not the timezone used when
    // they were created. Moving between UTC+14 and UTC-12 can shift the same
    // epoch across two civil dates, so protect a two-day halo to fail closed.
    for date in rollup_dates {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d") {
            for day_offset in [-2i64, -1, 1, 2] {
                if let Some(adjacent) = date.checked_add_signed(chrono::Duration::days(day_offset))
                {
                    protected_dates.insert(adjacent.format("%Y-%m-%d").to_string());
                }
            }
        }
    }
    for row in &live_rows {
        if row.thread_id.is_none() {
            if let Some(local_date) = &row.local_date {
                protected_dates.insert(local_date.clone());
            }
        }
    }

    let mut live_by_thread: HashMap<String, Vec<&LiveCodexRowIdentity>> = HashMap::new();
    for row in &live_rows {
        if let Some(thread_id) = &row.thread_id {
            live_by_thread
                .entry(thread_id.clone())
                .or_default()
                .push(row);
        }
    }

    let mut skipped = 0u32;
    let mut staged_by_thread: HashMap<String, Vec<StagedCodexRow>> = HashMap::new();
    for row in staged_rows {
        if let Some(thread_id) =
            rebuild_rollout_identity(&row.request_id, Some(&row.session_id), &resumed_identities)
        {
            staged_by_thread.entry(thread_id).or_default().push(row);
        } else {
            skipped = skipped.saturating_add(1);
        }
    }

    let mut safe_threads = HashSet::new();
    for (thread_id, rows) in &staged_by_thread {
        if blocked_threads.contains(thread_id) {
            skipped = skipped.saturating_add(rows.len() as u32);
            continue;
        }
        let staged_by_request = rows
            .iter()
            .map(|row| (row.request_id.as_str(), row))
            .collect::<HashMap<_, _>>();
        let live_is_covered = live_by_thread.get(thread_id).is_none_or(|live_rows| {
            live_rows.iter().all(|live| {
                let Some(local_date) = &live.local_date else {
                    return false;
                };
                if protected_dates.contains(local_date) {
                    // Protected dates may legitimately lack staged detail, but
                    // a colliding request ID must still identify the same
                    // event. Otherwise advancing this thread's cursor could
                    // permanently hide a rewritten event.
                    return staged_by_request
                        .get(live.request_id.as_str())
                        .is_none_or(|staged| staged.created_at == live.created_at);
                }
                let Some(staged) = staged_by_request.get(live.request_id.as_str()) else {
                    return false;
                };
                // Event ordinals are only stable while the rollout history is
                // append-only. Anchor an existing request ID to its event
                // timestamp so a truncated/rewritten JSONL cannot replace an
                // unrelated historical row that reused the same ordinal.
                if staged.created_at != live.created_at {
                    return false;
                }
                let (Ok(live_cost), Ok(staged_cost)) = (
                    live.total_cost_usd.parse::<Decimal>(),
                    staged.total_cost_usd.parse::<Decimal>(),
                ) else {
                    return false;
                };
                if live_cost < Decimal::ZERO || staged_cost < Decimal::ZERO {
                    return false;
                }
                live_cost == Decimal::ZERO || staged_cost > Decimal::ZERO
            })
        });
        if live_is_covered {
            safe_threads.insert(thread_id.clone());
        } else {
            skipped = skipped.saturating_add(rows.len() as u32);
        }
    }

    let transaction = conn
        .transaction()
        .map_err(|error| AppError::Database(format!("开启 Codex 安全重建事务失败: {error}")))?;

    for live in &live_rows {
        let Some(thread_id) = &live.thread_id else {
            continue;
        };
        let Some(local_date) = &live.local_date else {
            continue;
        };
        if safe_threads.contains(thread_id) && !protected_dates.contains(local_date) {
            transaction
                .execute(
                    "DELETE FROM proxy_request_logs
                     WHERE request_id = ?1 AND data_source = 'codex_session'",
                    [&live.request_id],
                )
                .map_err(|error| {
                    AppError::Database(format!("替换 Codex 明细前清理旧行失败: {error}"))
                })?;
        }
    }

    let mut imported = 0u32;
    for (thread_id, rows) in &staged_by_thread {
        if !safe_threads.contains(thread_id) {
            continue;
        }
        for row in rows {
            let effective_created_at = row.created_at;
            let local_date = row.local_date();
            let Some(local_date) = local_date else {
                skipped = skipped.saturating_add(1);
                continue;
            };
            if protected_dates.contains(&local_date) {
                skipped = skipped.saturating_add(1);
                continue;
            }

            let dedup_key = DedupKey {
                app_type: "codex",
                model: &row.model,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                cache_read_tokens: row.cache_read_tokens,
                cache_creation_tokens: row.cache_write_tokens,
                cache_write_known: row.input_token_semantics == INPUT_TOKEN_SEMANTICS_TOTAL
                    || row.cache_write_tokens > 0,
                created_at: effective_created_at,
            };
            if should_skip_session_insert(&transaction, &row.request_id, &dedup_key)? {
                skipped = skipped.saturating_add(1);
                continue;
            }

            let inserted = transaction
                .execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model, request_model, pricing_model,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        input_token_semantics,
                        input_cost_usd, output_cost_usd, cache_read_cost_usd,
                        cache_creation_cost_usd, total_cost_usd,
                        latency_ms, first_token_ms, status_code, error_message, session_id,
                        provider_type, is_streaming, cost_multiplier, created_at, data_source
                     ) VALUES (
                        ?1, '_codex_session', 'codex', ?2, ?3, ?4,
                        ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                        0, NULL, 200, NULL, ?15, 'codex_session', 1, ?16, ?17,
                        'codex_session'
                     )",
                    rusqlite::params![
                        row.request_id,
                        row.model,
                        row.request_model,
                        row.pricing_model,
                        row.input_tokens,
                        row.output_tokens,
                        row.cache_read_tokens,
                        row.cache_write_tokens,
                        row.input_token_semantics,
                        row.input_cost_usd,
                        row.output_cost_usd,
                        row.cache_read_cost_usd,
                        row.cache_write_cost_usd,
                        row.total_cost_usd,
                        row.session_id,
                        row.cost_multiplier,
                        effective_created_at,
                    ],
                )
                .map_err(|error| {
                    AppError::Database(format!("应用 Codex 临时重建明细失败: {error}"))
                })?;
            if inserted > 0 {
                imported = imported.saturating_add(1);
            } else {
                skipped = skipped.saturating_add(1);
            }
        }
    }

    // 只 upsert staging 中实际完成的 exact path。旧 CODEX_HOME 或已删除
    // JSONL 的 cursor 即使 UUID 相同也保留，避免清除仅存的历史同步证据。
    for cursor in staged_cursors {
        let should_apply = thread_id_from_cursor_path(&cursor.file_path)
            .is_some_and(|thread_id| safe_threads.contains(&thread_id));
        if should_apply {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO session_log_sync (
                        file_path, last_modified, last_line_offset, last_synced_at, last_byte_offset
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        cursor.file_path,
                        cursor.last_modified,
                        cursor.last_line_offset,
                        cursor.last_synced_at,
                        cursor.last_byte_offset,
                    ],
                )
                .map_err(|error| AppError::Database(format!("写入 Codex cursor 失败: {error}")))?;
        }
    }

    transaction
        .commit()
        .map_err(|error| AppError::Database(format!("提交 Codex 安全重建事务失败: {error}")))?;
    clear_codex_replay_caches();
    Ok((imported, skipped))
}

/// 在独立内存库完整解析当前 JSONL，确认可重建行后再单事务替换现库。
/// 缺源、坏源、旧 request_id 和任何已有 Codex rollup 日期都原样保留。
pub(crate) fn rebuild_codex_usage_preserving_history(
    db: &Database,
) -> Result<SessionSyncResult, AppError> {
    let codex_dir = get_codex_config_dir();
    let files = collect_codex_session_files(&codex_dir);
    let audit = audit_codex_rebuild_sources(&files);
    let staging = Database::memory()?;
    copy_model_pricing_to_staging(db, &staging)?;
    clear_codex_replay_caches();
    let mut result = sync_codex_usage(&staging)?;
    let staged_rows = load_staged_codex_rows(&staging)?;
    let staged_cursors = load_staged_codex_cursors(&staging)?;
    let blocked_threads = finalize_codex_rebuild_audit(audit, &staged_cursors);
    let (imported, skipped) =
        apply_staged_codex_usage(db, staged_rows, staged_cursors, &blocked_threads)?;
    result.imported = imported;
    result.skipped = result.skipped.saturating_add(skipped);
    Ok(result)
}

/// 收集所有 Codex 会话 JSONL 文件
fn collect_codex_session_files(codex_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    // 1. 扫描 sessions/YYYY/MM/DD/*.jsonl（日期分区目录）
    let sessions_dir = codex_dir.join("sessions");
    if sessions_dir.is_dir() {
        collect_jsonl_recursive(&sessions_dir, &mut files, 0, 3);
    }

    // 2. 扫描 archived_sessions/*.jsonl（扁平归档目录）
    let archived_dir = codex_dir.join("archived_sessions");
    if archived_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&archived_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if codex_rollout_file::is_rollout_file(&path) {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    // 压缩 / 解压的瞬间两种形态并存时只读普通形态，避免同一线程出现两个候选
    codex_rollout_file::dedupe_siblings(files)
}

fn build_rollout_index(files: &[PathBuf]) -> RolloutIndex {
    let mut index = RolloutIndex::new();
    for path in files {
        if let Some(thread_id) = thread_id_from_filename(path) {
            index.entry(thread_id).or_default().push(path.clone());
        }
        // paginated / resume 的续写页（`<threadId>_<rolloutId>`）同时登记在线程
        // 本体 UUID 下：子代理的 forked_from_id 指向线程本体，解析父时间线时
        // 必须能看到首页之后的分段。
        if let Some(thread_id) = leading_thread_id_from_filename(path) {
            index.entry(thread_id).or_default().push(path.clone());
        }
    }
    for paths in index.values_mut() {
        paths.sort();
        paths.dedup();
    }
    index
}

/// 递归扫描目录下的 .jsonl 文件（限制最大深度）
fn collect_jsonl_recursive(dir: &Path, files: &mut Vec<PathBuf>, depth: u32, max_depth: u32) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < max_depth {
            collect_jsonl_recursive(&path, files, depth + 1, max_depth);
        } else if codex_rollout_file::is_rollout_file(&path) {
            files.push(path);
        }
    }
}

/// 为翻页续写页解析其上一分段的截止累计高水位基线。
///
/// 当续写页中的 `token_count` 缺少有效的 `last_token_usage` 时，代码依赖
/// `compute_delta(&total_high_water, total)` 计算增量。若续写页不恢复前序分段高水位，
/// 首条记录会误将整段历史累计值作为增量计费。通过当前逻辑线程的候选文件列表（按时间排序）
/// 或 `history_base_id`，定位到紧邻前序分段并提取其最终高水位；若前序分段缺失或无法读取，
/// 必须返回错误以触发防御性暂缓导入（deferred），防止重复记账。
fn resolve_continuation_baseline(
    file_path: &Path,
    meta_thread_id: &str,
    history_base_id: Option<&str>,
    rollout_index: Option<&RolloutIndex>,
) -> Result<Option<CumulativeTokens>, String> {
    let Some(rollout_index) = rollout_index else {
        return Err("缺少 rollout_index 无法解析续写页基线".to_string());
    };

    let current_file_name = file_path.file_name();

    // 1. 优先在当前逻辑线程关联的候选文件中，按文件名时间自然序倒序寻找早于当前分段的前序页
    if let Some(candidates) = rollout_index.get(meta_thread_id) {
        for prev_path in candidates.iter().rev() {
            if prev_path.file_name() < current_file_name {
                let timeline = load_parent_timeline(prev_path).map_err(|e| {
                    format!(
                        "无法读取续写页前序分段 {}: {}",
                        prev_path.display(),
                        e.reason
                    )
                })?;
                if let Some(hw) = timeline.high_water.clone() {
                    return Ok(Some(hw));
                }
            }
        }
    }

    // 2. 若当前线程候选列表未找到且 history_base_id 指向独立 UUID，
    // 尝试从该独立候选列表中查找早于当前分段的文件
    if let Some(hb_id) = history_base_id {
        if hb_id != meta_thread_id {
            if let Some(candidates) = rollout_index.get(hb_id) {
                for prev_path in candidates.iter().rev() {
                    if prev_path.file_name() < current_file_name {
                        let timeline = load_parent_timeline(prev_path).map_err(|e| {
                            format!(
                                "无法读取续写页前序分段 {}: {}",
                                prev_path.display(),
                                e.reason
                            )
                        })?;
                        if let Some(hw) = timeline.high_water.clone() {
                            return Ok(Some(hw));
                        }
                    }
                }
            }
        }
    }

    if history_base_id.is_some() {
        return Err(format!(
            "找不到续写页 history_base 对应的前序分段文件 (meta: {meta_thread_id})"
        ));
    }

    Ok(None)
}

#[cfg(test)]
fn parse_codex_file(
    file_path: &Path,
    root_thread_id: Option<String>,
) -> Result<ParsedCodexFile, AppError> {
    parse_codex_file_with_index(file_path, root_thread_id, None)
}

fn parse_codex_file_with_index(
    file_path: &Path,
    root_thread_id: Option<String>,
    rollout_index: Option<&RolloutIndex>,
) -> Result<ParsedCodexFile, AppError> {
    #[cfg(test)]
    tests::record_codex_parse(file_path);
    let file =
        fs::File::open(file_path).map_err(|e| AppError::Config(format!("无法打开文件: {e}")))?;
    // 压缩文件（`.jsonl.zst`）的 observed_bytes 记磁盘上的压缩大小：
    // 它只用来和下轮的文件大小比对，记解压后长度会让每轮都重新解压。
    let compressed_size = if codex_rollout_file::is_compressed(file_path) {
        Some(
            file.metadata()
                .map_err(|e| AppError::Config(format!("无法读取文件元数据: {e}")))?
                .len(),
        )
    } else {
        None
    };
    let mut reader = codex_rollout_file::reader_from_file(file_path, file)
        .map_err(|e| AppError::Config(format!("无法打开文件: {e}")))?;
    let mut root_meta_seen = false;
    let mut root_timestamp = None;
    let mut meta_thread_id = None;
    let mut parent = ParentResolution::None;
    let mut current_model = "unknown".to_string();
    let mut current_service_tier = CodexServiceTier::Unknown;
    // `total_token_usage` is session-cumulative, including across model and
    // rate-limit bucket changes. Divergent snapshots are handled by preferring
    // exact `last_token_usage`, not by splitting the cumulative baseline.
    let mut total_high_water = None;
    // Rate-limit refreshes can re-emit unchanged token info under another
    // `limit_id`. Same-source repeats are identified by that source's latest
    // full snapshot; cross-source repeats must match the immediately preceding
    // token event. Do not compare against other sources' older snapshots:
    // those stale signatures can legitimately recur after a counter reset.
    let mut last_signature_by_source: HashMap<Option<String>, TokenUsageSignature> = HashMap::new();
    let mut previous_token_signature = None;
    let mut is_continuation = false;
    let mut history_base_thread_id = None;
    let mut event_index = 0u32;
    let mut token_events = Vec::new();
    let mut usage_record_events = Vec::new();
    let mut pending_usage: Option<PendingUsageRecord> = None;
    let mut line_offset = 0i64;
    let mut observed_bytes = 0i64;
    let mut has_unterminated_tail = false;
    let mut has_billable_tokens = false;
    let mut timer = RequestTimer::default();

    let mut line = String::new();
    loop {
        line.clear();
        let next_line_offset = line_offset.saturating_add(1);
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            AppError::Config(format!(
                "读取 {} 第 {next_line_offset} 行失败: {error}",
                file_path.display()
            ))
        })?;
        // Include the incomplete suffix for unchanged-file detection, but do
        // not consume its line cursor or trust it during a destructive rebuild.
        observed_bytes += bytes_read as i64;
        if bytes_read == 0 {
            break;
        }
        // JSONL writers may expose the final line while it is still being
        // appended. A complete JSON value is valid even without a final
        // newline, but an invalid unterminated value must not advance the line
        // cursor: the next pass needs to revisit that same physical line.
        let unterminated_value = if !line.ends_with('\n') {
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(value) => Some(value),
                Err(_) => {
                    has_unterminated_tail = true;
                    break;
                }
            }
        } else {
            None
        };
        line_offset = next_line_offset;
        if line.trim().is_empty() {
            continue;
        }

        timer.observe_line(&line);

        let is_event_msg = line.contains("\"event_msg\"");
        let is_turn_context = line.contains("\"turn_context\"");
        let is_session_meta = line.contains("\"session_meta\"");
        let is_usage_record = line.contains("\"token_usage_record\"");
        if !is_event_msg && !is_turn_context && !is_session_meta && !is_usage_record {
            continue;
        }
        if is_event_msg
            && !line.contains("\"token_count\"")
            && !line.contains("\"thread_settings_applied\"")
        {
            continue;
        }

        let value = if let Some(value) = unterminated_value {
            value
        } else {
            serde_json::from_str(&line).map_err(|error| {
                AppError::Config(format!(
                    "解析 {} 第 {line_offset} 行失败: {error}",
                    file_path.display()
                ))
            })?
        };
        let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };

        match event_type {
            "session_meta" if !root_meta_seen => {
                root_meta_seen = true;
                root_timestamp = parse_timestamp(value.get("timestamp"));
                let payload = value.get("payload").unwrap_or(&serde_json::Value::Null);
                parent = explicit_parent_from_meta(payload);

                meta_thread_id = non_empty_string(
                    payload
                        .get("id")
                        .or_else(|| payload.get("thread_id"))
                        .or_else(|| payload.get("threadId")),
                )
                .map(|id| {
                    uuid::Uuid::parse_str(&id)
                        .map(|value| value.hyphenated().to_string())
                        .unwrap_or(id)
                });
                let leading_id = leading_thread_id_from_filename(file_path);
                if let (Some(filename_id), Some(meta_id)) =
                    (&root_thread_id, meta_thread_id.as_ref())
                {
                    let matches =
                        filename_id == meta_id || leading_id.as_deref() == Some(meta_id.as_str());
                    if !matches {
                        parent = ParentResolution::Deferred(format!(
                            "文件名线程 ID ({filename_id}) 与 root meta ID ({meta_id}) 不一致"
                        ));
                    }
                }

                // 分支线程翻页产生的续写页（`<branchId>_<pageId>`）以及带有指向本线程
                // history_base 的延续分段，其 session_meta 仍保留所属分支会话的
                // forked_from_id / parent_thread_id；但续写页是在本线程之后实时追加的，
                // 记录的是翻页后的新事件，新格式续写页不复制父会话用量。如果将其作为
                // 新 fork 处理，其 session_meta 时间戳是翻页时刻（晚于原始父线程末尾），
                // 会因父线程无法达到 child cutoff 而被永久 deferred。
                // 识别为本线程延续后，将其标为 Continuation，不剔除父重放（无需对齐父时间线）。
                history_base_thread_id = non_empty_string(
                    payload
                        .get("history_base")
                        .and_then(|hb| hb.get("thread_id")),
                )
                .map(|id| {
                    uuid::Uuid::parse_str(&id)
                        .map(|value| value.hyphenated().to_string())
                        .unwrap_or(id)
                });
                let is_paginated = payload
                    .get("history_mode")
                    .and_then(serde_json::Value::as_str)
                    == Some("paginated");
                let has_history_base = history_base_thread_id.is_some();
                let has_parent = matches!(parent, ParentResolution::Parent(_));
                is_continuation = match (leading_id.as_deref(), meta_thread_id.as_deref()) {
                    (Some(leading), Some(meta)) if leading == meta => {
                        is_paginated || has_history_base || has_parent
                    }
                    _ => {
                        has_history_base
                            && history_base_thread_id.as_deref() == meta_thread_id.as_deref()
                    }
                };

                if is_continuation {
                    let ancestor_parent_id = match &parent {
                        ParentResolution::Parent(p) => Some(p.clone()),
                        _ => None,
                    };
                    parent = ParentResolution::Continuation {
                        ancestor_parent_id,
                        history_base_id: history_base_thread_id.clone(),
                    };
                }

                if let ParentResolution::Parent(parent_id) = &mut parent {
                    match uuid::Uuid::parse_str(parent_id) {
                        Ok(value) => *parent_id = value.hyphenated().to_string(),
                        Err(_) => {
                            parent = ParentResolution::Deferred(format!(
                                "显式 parent_thread_id 不是有效 UUID: {parent_id}"
                            ));
                        }
                    }
                }
                if matches!((&root_thread_id, &parent), (Some(root), ParentResolution::Parent(parent_id)) if root == parent_id)
                {
                    parent = ParentResolution::Deferred(
                        "parent_thread_id 与 root_thread_id 相同".to_string(),
                    );
                }
            }
            "turn_context" => {
                if let Some(payload) = value.get("payload") {
                    if let Some(model) = payload
                        .get("model")
                        .or_else(|| payload.get("info").and_then(|info| info.get("model")))
                        .and_then(serde_json::Value::as_str)
                    {
                        current_model = normalize_codex_model(model);
                    }
                }
            }
            "event_msg" => {
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                match payload.get("type").and_then(serde_json::Value::as_str) {
                    Some("thread_settings_applied") => {
                        let settings = payload.get("thread_settings");
                        if let Some(model) = settings
                            .and_then(|settings| settings.get("model"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::trim)
                            .filter(|model| !model.is_empty())
                        {
                            current_model = normalize_codex_model(model);
                        }
                        current_service_tier = CodexServiceTier::from_raw(
                            settings
                                .and_then(|settings| settings.get("service_tier"))
                                .and_then(serde_json::Value::as_str),
                        );
                        continue;
                    }
                    Some("token_count") => {}
                    _ => continue,
                }
                let Some(info) = payload.get("info").filter(|info| !info.is_null()) else {
                    continue;
                };
                let Some(signature) = parse_token_signature(info) else {
                    continue;
                };

                if let Some(model) = info
                    .get("model")
                    .or_else(|| info.get("model_name"))
                    .or_else(|| payload.get("model"))
                    .and_then(serde_json::Value::as_str)
                {
                    current_model = normalize_codex_model(model);
                }

                let snapshot_source = token_snapshot_source(payload);
                let total = info
                    .get("total_token_usage")
                    .and_then(parse_cumulative_tokens);
                let last = info
                    .get("last_token_usage")
                    .and_then(parse_cumulative_tokens);
                if let Some(pending) = pending_usage.take() {
                    // 对得上的 last 已经由这条 token_count 入账。last 为 0 是远程压缩
                    // 把本次用量只写在 token_usage_record 里之后的空快照。其它对不上的
                    // 记录只用于耗时，不能再记一行。
                    let covered = last
                        .as_ref()
                        .is_some_and(|last| usage_matches(last, &pending.usage));
                    if !covered && last.as_ref().is_some_and(usage_is_zero) {
                        push_unmatched_usage(
                            &mut usage_record_events,
                            pending,
                            line_offset,
                            &mut has_billable_tokens,
                        );
                    }
                }
                if total.is_none() && last.is_none() {
                    continue;
                }
                let has_total_snapshot = total.is_some();
                let duplicate_snapshot = has_total_snapshot
                    && (last_signature_by_source.get(&snapshot_source) == Some(&signature)
                        || previous_token_signature.as_ref() == Some(&signature));
                if has_total_snapshot {
                    last_signature_by_source.insert(snapshot_source, signature.clone());
                }
                previous_token_signature = Some(signature.clone());

                let request_usage = last.clone();
                let delta = if duplicate_snapshot {
                    DeltaTokens {
                        input: 0,
                        cached_input: 0,
                        cache_write_input: 0,
                        cache_write_reported: false,
                        output: 0,
                    }
                } else if let Some(last) = last {
                    // Codex provides the exact per-request usage. Prefer it to
                    // subtracting cumulative snapshots, which may come from
                    // multiple independently advancing rate-limit lanes.
                    let mut delta = DeltaTokens {
                        input: last.input as u32,
                        cached_input: last.cached_input as u32,
                        cache_write_input: last.cache_write_input as u32,
                        cache_write_reported: last.cache_write_reported,
                        output: last.output as u32,
                    };
                    // Some rollout versions may add cache-write accounting to
                    // the cumulative snapshot before adding it to the per-call
                    // snapshot. Preserve the exact `last` values for all other
                    // buckets, but recover the missing write delta from the
                    // cumulative high-water mark when that field is available.
                    if !delta.cache_write_reported {
                        if let Some(total) =
                            total.as_ref().filter(|total| total.cache_write_reported)
                        {
                            let cumulative_delta = compute_delta(&total_high_water, total);
                            delta.cache_write_input = cumulative_delta.cache_write_input;
                            delta.cache_write_reported = true;
                        }
                    }
                    delta
                } else if let Some(total) = total.as_ref() {
                    if is_continuation && total_high_water.is_none() {
                        let meta_id = meta_thread_id.as_deref().unwrap_or_default();
                        match resolve_continuation_baseline(
                            file_path,
                            meta_id,
                            history_base_thread_id.as_deref(),
                            rollout_index,
                        ) {
                            Ok(Some(baseline)) => {
                                if total.input < baseline.input {
                                    // 续写页计数器重置（从 0 或较小值起算），以当前快照为首轮基线
                                    total_high_water = None;
                                } else {
                                    // 继承上一分段截止高水位基线
                                    total_high_water = Some(baseline);
                                }
                            }
                            Ok(None) => {
                                total_high_water = None;
                            }
                            Err(err) => {
                                // 防御机制：前序分段无法读取或基线无法可靠恢复，必须暂缓导入
                                parent = ParentResolution::Deferred(format!(
                                    "续写页缺少有效的前序累计用量基线: {err}"
                                ));
                                has_billable_tokens = true;
                                break;
                            }
                        }
                    }
                    compute_delta(&total_high_water, total)
                } else {
                    continue;
                };
                if let Some(total) = total {
                    if let Some(high_water) = total_high_water.as_mut() {
                        update_high_water(high_water, &total);
                    } else {
                        total_high_water = Some(total);
                    }
                }
                let delta = clamp_delta_cache(delta);
                let nonzero_index = if delta.is_zero() {
                    None
                } else {
                    has_billable_tokens = true;
                    event_index = event_index.saturating_add(1);
                    Some(event_index)
                };

                let timestamp = value
                    .get("timestamp")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                // 重复快照（限额刷新时重发的）不是一次请求，不参与计时
                let timing = nonzero_index.and_then(|_| {
                    timer.finish_request(
                        timestamp.as_deref().and_then(parse_timestamp_millis),
                        request_usage.as_ref(),
                    )
                });

                token_events.push(ParsedTokenEvent {
                    line_offset,
                    signature,
                    delta,
                    event_index: nonzero_index,
                    request_suffix: None,
                    model: current_model.clone(),
                    service_tier: current_service_tier,
                    timestamp,
                    timing,
                });
            }
            "token_usage_record" => {
                pending_usage = None;
                let Some(payload) = value.get("payload") else {
                    continue;
                };
                let Some(usage) = payload.get("usage").and_then(parse_cumulative_tokens) else {
                    continue;
                };
                if usage_is_zero(&usage) {
                    continue;
                }
                let response_id = payload
                    .get("response_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let key = if response_id.is_empty() {
                    format!("line{line_offset}")
                } else {
                    response_id.to_string()
                };
                pending_usage = Some(PendingUsageRecord {
                    key,
                    usage,
                    timestamp: value
                        .get("timestamp")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    model: current_model.clone(),
                    service_tier: current_service_tier,
                });
            }
            _ => {}
        }
    }

    Ok(ParsedCodexFile {
        root_thread_id,
        meta_thread_id,
        root_meta_seen,
        root_timestamp,
        parent,
        token_events,
        usage_record_events,
        line_offset,
        has_unterminated_tail,
        observed_bytes: compressed_size
            .and_then(|size| i64::try_from(size).ok())
            .unwrap_or(observed_bytes),
        has_billable_tokens,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ParentSignatureError {
    reason: String,
    cacheable: bool,
}

impl ParentSignatureError {
    fn content(reason: String) -> Self {
        Self {
            reason,
            cacheable: true,
        }
    }

    fn transient(reason: String) -> Self {
        Self {
            reason,
            cacheable: false,
        }
    }
}

#[cfg(test)]
fn parent_signatures_before(
    parent_path: &Path,
    cutoff: DateTime<Utc>,
) -> Result<Vec<TokenUsageSignature>, ParentSignatureError> {
    load_parent_timeline(parent_path)?
        .signatures_before(parent_path, cutoff)
        .map_err(ParentSignatureError::content)
}

/// 读取（或命中缓存）父 rollout 单个文件的完整 token 时间线。
fn load_parent_timeline(
    parent_path: &Path,
) -> Result<Arc<ParentTokenTimeline>, ParentSignatureError> {
    let file = fs::File::open(parent_path).map_err(|error| {
        ParentSignatureError::transient(format!(
            "无法打开父 rollout {}: {error}",
            parent_path.display()
        ))
    })?;
    let stamp = ParentFileStamp::from_file(&file);
    let cached_timeline = stamp.and_then(|stamp| {
        replay_caches().lock().ok().and_then(|caches| {
            caches
                .parent_timelines
                .get(parent_path)
                .filter(|entry| entry.stamp == stamp)
                .map(|entry| Arc::clone(&entry.timeline))
        })
    });
    if let Some(timeline) = cached_timeline {
        return Ok(timeline);
    }

    let mut events = Vec::new();
    let mut max_timestamp: Option<DateTime<Utc>> = None;
    let mut has_token_without_timestamp = false;
    let mut high_water: Option<CumulativeTokens> = None;

    // 必须扫描完整父文件，不能在首个未来时间戳处 break：rollout 写入顺序
    // 不承诺时间戳严格单调。缓存完整时间线后，不同 child cutoff 只需内存过滤。
    let mut reader = codex_rollout_file::reader_from_file(parent_path, file).map_err(|error| {
        ParentSignatureError::transient(format!(
            "无法打开父 rollout {}: {error}",
            parent_path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        // I/O/decompression failures are not evidence of stable content. Do
        // not cache a partial timeline or suppress its next resolution attempt.
        let read = reader.read_until(b'\n', &mut bytes).map_err(|error| {
            ParentSignatureError::transient(format!(
                "无法读取父 rollout {}: {error}",
                parent_path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        // Parent history determines the child's replay prefix. Fail closed on
        // invalid content; silently skipping it could charge parent usage twice.
        let line = std::str::from_utf8(&bytes).map_err(|error| {
            ParentSignatureError::transient(format!(
                "父 rollout {} 的 UTF-8 无效: {error}",
                parent_path.display()
            ))
        })?;
        if line.trim().is_empty() && line.ends_with('\n') {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(line).map_err(|error| {
            ParentSignatureError::transient(format!(
                "解析父 rollout {} 失败或尾行尚未写完: {error}",
                parent_path.display()
            ))
        })?;
        let timestamp = parse_timestamp(value.get("timestamp"));
        if let Some(timestamp) = timestamp {
            max_timestamp = Some(max_timestamp.map_or(timestamp, |current| current.max(timestamp)));
        }
        if value.get("type").and_then(serde_json::Value::as_str) != Some("event_msg")
            || value
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(serde_json::Value::as_str)
                != Some("token_count")
        {
            continue;
        }
        let Some(info) = value
            .get("payload")
            .and_then(|payload| payload.get("info"))
            .filter(|info| !info.is_null())
        else {
            continue;
        };
        let Some(signature) = parse_token_signature(info) else {
            continue;
        };
        let Some(timestamp) = timestamp else {
            has_token_without_timestamp = true;
            continue;
        };
        events.push(TimestampedTokenSignature {
            timestamp,
            signature,
        });

        if let Some(total) = info
            .get("total_token_usage")
            .and_then(parse_cumulative_tokens)
        {
            if let Some(hw) = high_water.as_mut() {
                update_high_water(hw, &total);
            } else {
                high_water = Some(total);
            }
        }
    }

    let timeline = Arc::new(ParentTokenTimeline {
        events,
        max_timestamp,
        has_token_without_timestamp,
        high_water,
    });
    if let (Some(stamp), Ok(mut caches)) = (stamp, replay_caches().lock()) {
        caches.parent_timelines.insert(
            parent_path.to_path_buf(),
            CachedParentTimeline {
                stamp,
                timeline: Arc::clone(&timeline),
            },
        );
    }
    Ok(timeline)
}

/// 父线程的一个分段（首页或某个续写页），可能有多份内容一致的副本。
struct ParentSegment {
    rollout_id: Option<String>,
    timeline: Arc<ParentTokenTimeline>,
    /// 各副本中最早的末尾时间。
    max_timestamp: Option<DateTime<Utc>>,
}

fn resolve_parent_signatures(
    parent_id: &str,
    cutoff: DateTime<Utc>,
    rollout_index: &RolloutIndex,
) -> Result<Vec<TokenUsageSignature>, ParentSignatureError> {
    let Some(candidates) = rollout_index.get(parent_id) else {
        return Err(ParentSignatureError::transient(format!(
            "找不到父 rollout: {parent_id}"
        )));
    };

    // 同一线程 UUID 可能对应多个文件：同一 rollout 的副本（如 sessions/ 与
    // archived_sessions/ 各一份）必须内容一致；paginated / resume 的续写页
    // （`<parent_id>_<rolloutId>`）则是该线程的后续分段——翻页后首页不再增长，
    // 必须按文件名时间前缀顺序拼成一条时间线，再判断是否已写到 child fork 时刻。
    let mut ordered = candidates.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    let mut merged = ParentTokenTimeline {
        events: Vec::new(),
        max_timestamp: None,
        has_token_without_timestamp: false,
        high_water: None,
    };
    let mut segments: Vec<ParentSegment> = Vec::new();
    let mut latest_path: Option<&Path> = None;
    for candidate in ordered {
        let timeline = load_parent_timeline(candidate)?;
        // 无论是否为重复副本，只要任一候选存在无时间戳错误标志，必须合入以保留防御校验
        merged.has_token_without_timestamp |= timeline.has_token_without_timestamp;
        let rollout_id = thread_id_from_filename(candidate);
        if let Some(segment) = segments
            .iter_mut()
            .find(|segment| segment.rollout_id == rollout_id)
        {
            if segment.timeline.signatures_at(cutoff) != timeline.signatures_at(cutoff) {
                return Err(ParentSignatureError::content(format!(
                    "父 rollout UUID {parent_id} 对应多个内容不一致的文件"
                )));
            }
            // 同一 rollout 的每份副本都必须写到 fork 时刻：该段取各副本中最早的
            // 末尾时间，结果不随副本顺序变化。
            segment.max_timestamp = match (segment.max_timestamp, timeline.max_timestamp) {
                (Some(current), Some(other)) => Some(current.min(other)),
                _ => None,
            };
            continue;
        }
        merged.events.extend(timeline.events.iter().cloned());
        segments.push(ParentSegment {
            rollout_id,
            max_timestamp: timeline.max_timestamp,
            timeline,
        });
        latest_path = Some(candidate.as_path());
    }
    merged.max_timestamp = segments
        .iter()
        .filter_map(|segment| segment.max_timestamp)
        .max();
    let Some(latest_path) = latest_path else {
        return Err(ParentSignatureError::transient(format!(
            "找不到父 rollout: {parent_id}"
        )));
    };
    merged
        .signatures_before(latest_path, cutoff)
        .map_err(ParentSignatureError::content)
}

/// Metadata-only dependency snapshot; opening a file checks accessibility and
/// gives the same identity stamp used by the parent timeline cache. Include all
/// candidates so archive moves, replacements and duplicate candidates retry.
/// This must be called without holding the replay-cache mutex.
fn parent_dependency_stamp(
    parent_id: &str,
    rollout_index: &RolloutIndex,
) -> Option<Vec<(PathBuf, ParentFileStamp)>> {
    let candidates = rollout_index.get(parent_id)?;
    let mut files = Vec::with_capacity(candidates.len());
    for path in candidates {
        let file = fs::File::open(path).ok()?;
        files.push((path.clone(), ParentFileStamp::from_file(&file)?));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Some(files)
}

fn matching_replay_prefix(child: &[ParsedTokenEvent], parent: &[TokenUsageSignature]) -> usize {
    let mut parent_offset = 0usize;
    let mut matched = 0usize;
    for event in child {
        let Some(relative_match) = parent[parent_offset..]
            .iter()
            .position(|signature| signature == &event.signature)
        else {
            break;
        };
        parent_offset += relative_match + 1;
        matched += 1;
    }
    matched
}

fn mark_deferred(
    file_path: &Path,
    modified: i64,
    size: u64,
    reason: PendingReason,
) -> CodexFileSyncResult {
    let entry = PendingEntry {
        modified,
        size,
        reason,
    };
    let should_warn = replay_caches()
        .lock()
        .ok()
        .and_then(|mut caches| {
            caches
                .pending
                .insert(file_path.to_path_buf(), entry.clone())
        })
        .as_ref()
        != Some(&entry);
    if should_warn {
        let reason = match &entry.reason {
            PendingReason::MissingParent(parent) => format!("找不到父 rollout {parent}"),
            PendingReason::Stable(reason) | PendingReason::Retryable { reason, .. } => {
                reason.clone()
            }
        };
        log::warn!("[CODEX-SYNC] deferred {}: {reason}", file_path.display());
    }
    CodexFileSyncResult {
        deferred: true,
        ..CodexFileSyncResult::default()
    }
}

/// 单文件批量插入的事务粒度。批内 UI 查询会被连接互斥锁挡住约几毫秒，
/// 批间释放锁让读侧插队——兼顾吞吐（避免逐行 autocommit 的每行 fsync）
/// 与大文件重导期间面板的响应性。
const CODEX_INSERT_BATCH_SIZE: usize = 1000;

fn update_codex_sync_state_on_conn(
    conn: &rusqlite::Connection,
    file_path: &str,
    modified: i64,
    parsed: &ParsedCodexFile,
) -> Result<(), AppError> {
    update_sync_state_on_conn(conn, file_path, modified, parsed.line_offset)?;
    conn.execute(
        "UPDATE session_log_sync SET last_byte_offset = ?1 WHERE file_path = ?2",
        rusqlite::params![parsed.observed_bytes, file_path],
    )?;
    Ok(())
}

fn update_codex_sync_state(
    db: &Database,
    file_path: &str,
    modified: i64,
    parsed: &ParsedCodexFile,
) -> Result<(), AppError> {
    let conn = lock_conn!(db.conn);
    let tx = conn.unchecked_transaction()?;
    update_codex_sync_state_on_conn(&tx, file_path, modified, parsed)?;
    tx.commit()?;
    Ok(())
}

/// 同步单个 Codex JSONL 文件。
fn sync_single_codex_file(
    db: &Database,
    file_path: &Path,
    rollout_index: &RolloutIndex,
    pass: &mut CodexSyncPass,
) -> Result<CodexFileSyncResult, AppError> {
    let file_path_str = file_path.to_string_lossy().to_string();

    // 获取文件元数据
    let metadata = fs::metadata(file_path)
        .map_err(|e| AppError::Config(format!("无法读取文件元数据: {e}")))?;
    let file_modified = metadata_modified_nanos(&metadata);
    let file_size = metadata.len();

    // 检查同步状态
    let (last_modified, last_offset) = get_codex_sync_state(db, file_path, pass)?;

    // Windows may keep mtime unchanged while Codex holds its write handle open.
    // Legacy cursors have no byte offset: rescan once to catch up and persist it.
    let last_byte_offset = pass.byte_offsets.get(&file_path_str).copied().flatten();
    if file_modified == last_modified && last_byte_offset == i64::try_from(file_size).ok() {
        return Ok(CodexFileSyncResult::default());
    }

    // Capture child identity before parsing as well as before checking a
    // pending entry: replacing a deferred child can preserve mtime and size.
    // Unknown/inaccessible identity must not suppress the parser's retry/error.
    let child_file = fs::File::open(file_path)
        .ok()
        .and_then(|file| ParentFileStamp::from_file(&file));
    let pending = replay_caches()
        .lock()
        .ok()
        .and_then(|caches| caches.pending.get(file_path).cloned());
    if let Some(pending) = pending {
        if pending.modified == file_modified && pending.size == file_size {
            let unchanged = match &pending.reason {
                PendingReason::MissingParent(parent) => !rollout_index.contains_key(parent),
                PendingReason::Stable(_) => true,
                PendingReason::Retryable {
                    parent_id,
                    child_file: Some(previous_child),
                    parent_files: Some(parent_files),
                    ..
                } => {
                    child_file.as_ref() == Some(previous_child)
                        && parent_dependency_stamp(parent_id, rollout_index).as_ref()
                            == Some(parent_files)
                }
                PendingReason::Retryable { .. } => false,
            };
            if unchanged {
                return Ok(CodexFileSyncResult {
                    deferred: true,
                    ..CodexFileSyncResult::default()
                });
            }
        }
    }

    let parsed = parse_codex_file_with_index(
        file_path,
        thread_id_from_filename(file_path),
        Some(rollout_index),
    )?;
    if !parsed.has_billable_tokens {
        update_codex_sync_state(db, &file_path_str, file_modified, &parsed)?;
        return Ok(CodexFileSyncResult {
            deferred: parsed.has_unterminated_tail,
            ..CodexFileSyncResult::default()
        });
    }
    let Some(root_thread_id) = parsed.root_thread_id.as_deref() else {
        return Ok(mark_deferred(
            file_path,
            file_modified,
            file_size,
            PendingReason::Stable("文件名缺少有效的尾部 UUID".to_string()),
        ));
    };
    if !parsed.root_meta_seen {
        return Ok(mark_deferred(
            file_path,
            file_modified,
            file_size,
            PendingReason::Stable("含计费 token 但尚无 session_meta".to_string()),
        ));
    }

    let replay_prefix = match &parsed.parent {
        ParentResolution::None | ParentResolution::Continuation { .. } => 0,
        ParentResolution::Deferred(reason) => {
            let pending_reason = if let Some(meta_id) = parsed.meta_thread_id.as_deref() {
                if rollout_index.contains_key(meta_id) {
                    PendingReason::Retryable {
                        reason: reason.clone(),
                        parent_id: meta_id.to_string(),
                        child_file,
                        parent_files: parent_dependency_stamp(meta_id, rollout_index),
                    }
                } else {
                    PendingReason::Stable(reason.clone())
                }
            } else {
                PendingReason::Stable(reason.clone())
            };
            return Ok(mark_deferred(
                file_path,
                file_modified,
                file_size,
                pending_reason,
            ));
        }
        ParentResolution::Parent(parent_id) => {
            let Some(cutoff) = parsed.root_timestamp else {
                return Ok(mark_deferred(
                    file_path,
                    file_modified,
                    file_size,
                    PendingReason::Stable(
                        "parented rollout 的 root meta 缺少有效 timestamp".to_string(),
                    ),
                ));
            };
            // 先把查询结果拷出来再释放锁：Rust 2021 下 `if let .. else` 的临时值活到
            // else 分支结束，锁中毒时 Err 里仍攥着 guard，else 里再加锁会自锁。
            // None = 锁已中毒（不走缓存），Some(None) = 未命中。
            let cached_prefix = replay_caches().lock().ok().map(|caches| {
                caches
                    .replay_prefixes
                    .get(file_path)
                    .filter(|cached| cached.modified == file_modified && cached.size == file_size)
                    .map(|cached| cached.prefix)
            });
            match cached_prefix {
                Some(Some(prefix)) => prefix,
                Some(None) => {
                    let parent_files = parent_dependency_stamp(parent_id, rollout_index);
                    let parent_signatures =
                        match resolve_parent_signatures(parent_id, cutoff, rollout_index) {
                            Ok(signatures) => signatures,
                            Err(error) => {
                                let pending_reason = if rollout_index.contains_key(parent_id) {
                                    PendingReason::Retryable {
                                        reason: error.reason,
                                        parent_id: parent_id.clone(),
                                        child_file,
                                        parent_files: if error.cacheable {
                                            parent_files
                                        } else {
                                            None
                                        },
                                    }
                                } else {
                                    PendingReason::MissingParent(parent_id.clone())
                                };
                                return Ok(mark_deferred(
                                    file_path,
                                    file_modified,
                                    file_size,
                                    pending_reason,
                                ));
                            }
                        };
                    let prefix = matching_replay_prefix(&parsed.token_events, &parent_signatures);
                    if let Ok(mut caches) = replay_caches().lock() {
                        caches.replay_prefixes.insert(
                            file_path.to_path_buf(),
                            CachedReplayPrefix {
                                modified: file_modified,
                                size: file_size,
                                prefix,
                            },
                        );
                    }
                    prefix
                }
                None => {
                    let parent_signatures =
                        resolve_parent_signatures(parent_id, cutoff, rollout_index)
                            .map_err(|error| AppError::Config(error.reason))?;
                    matching_replay_prefix(&parsed.token_events, &parent_signatures)
                }
            }
        }
    };

    if let Ok(mut caches) = replay_caches().lock() {
        caches.pending.remove(file_path);
    }

    let mut result = CodexFileSyncResult {
        deferred: parsed.has_unterminated_tail,
        ..CodexFileSyncResult::default()
    };
    let mut to_insert: Vec<(&ParsedTokenEvent, String)> = Vec::new();
    for (token_offset, event) in parsed.token_events.iter().enumerate() {
        let Some(event_index) = event.event_index else {
            continue;
        };
        if token_offset < replay_prefix {
            if event.line_offset > last_offset {
                result.skipped = result.skipped.saturating_add(1);
            }
            continue;
        }
        if event.line_offset <= last_offset {
            continue;
        }
        to_insert.push((event, event_index.to_string()));
    }
    for event in &parsed.usage_record_events {
        let Some(suffix) = event.request_suffix.as_deref() else {
            continue;
        };
        if event.line_offset <= last_offset {
            continue;
        }
        to_insert.push((event, suffix.to_string()));
    }

    // 分批事务写库：逐行 autocommit（journal_mode=delete 下每行一整套
    // journal 建立/fsync/删除）是全量重导的最大耗时项。批内任一插入失败
    // 都回滚该批且不推进游标；下一 pass 重扫时由 request_id 主键 + 指纹
    // 去重兜底，不会丢数据或双算。
    //
    // session_id 记 root meta 的线程 ID：双段文件名（thread/revert 的替换
    // rollout）下是前置 UUID，与会话管理器侧的会话身份同口径；尾部 rollout
    // ID 只承担 request_id 去重键（event_index 按物理文件计数，不能改用
    // 前置 ID）。
    let session_thread_id = parsed.meta_thread_id.as_deref().unwrap_or(root_thread_id);
    let batch_count = to_insert.len().div_ceil(CODEX_INSERT_BATCH_SIZE);
    for (batch_index, batch) in to_insert.chunks(CODEX_INSERT_BATCH_SIZE).enumerate() {
        let is_last_batch = batch_index + 1 == batch_count;
        let conn = lock_conn!(db.conn);
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| AppError::Database(format!("开启 Codex 会话写入事务失败: {e}")))?;

        let mut batch_imported = 0u32;
        let mut batch_skipped = 0u32;
        let mut batch_suspected = 0u32;
        for (event, suffix) in batch {
            let request_id = format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{root_thread_id}:{suffix}");
            match insert_codex_session_entry_on_conn(
                &tx,
                &request_id,
                &event.delta,
                &event.model,
                event.service_tier,
                Some(session_thread_id),
                event.timestamp.as_deref(),
                event.timing,
                &mut batch_suspected,
                &mut pass.pricing,
            ) {
                Ok(true) => batch_imported += 1,
                Ok(false) => batch_skipped += 1,
                Err(error) => {
                    return Err(AppError::Database(format!(
                        "插入 Codex 会话明细失败 ({request_id}): {error}"
                    )));
                }
            }
        }
        if is_last_batch {
            // 游标推进与最后一批数据同事务提交：中途崩溃时两者一起回滚，
            // 不会出现"游标已推进但数据缺失"的丢数据窗口。
            update_codex_sync_state_on_conn(&tx, &file_path_str, file_modified, &parsed)?;
        }
        tx.commit()
            .map_err(|e| AppError::Database(format!("提交 Codex 会话写入事务失败: {e}")))?;

        result.imported = result.imported.saturating_add(batch_imported);
        result.skipped = result.skipped.saturating_add(batch_skipped);
        result.suspected_duplicates = result.suspected_duplicates.saturating_add(batch_suspected);
    }

    if to_insert.is_empty() {
        update_codex_sync_state(db, &file_path_str, file_modified, &parsed)?;
    }
    Ok(result)
}

/// 插入单条 Codex 会话记录到 proxy_request_logs（自取锁的便捷包装，测试专用；
/// 生产路径走 [`insert_codex_session_entry_on_conn`] 以复用批量事务与定价缓存）
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn insert_codex_session_entry(
    db: &Database,
    request_id: &str,
    delta: &DeltaTokens,
    model: &str,
    service_tier: CodexServiceTier,
    session_id: Option<&str>,
    timestamp: Option<&str>,
    suspected_duplicates: &mut u32,
) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    insert_codex_session_entry_on_conn(
        &conn,
        request_id,
        delta,
        model,
        service_tier,
        session_id,
        timestamp,
        None,
        suspected_duplicates,
        &mut HashMap::new(),
    )
}

/// 插入单条 Codex 会话记录到 proxy_request_logs。
///
/// 调用方负责持锁/事务；`pricing_cache` 按原始 model 字符串键控（
/// `find_codex_pricing` 是纯函数式查找，同串必同结果），全量重导时把
/// 每事件一次的定价 SELECT 降为每模型一次。`timing` 是按事件时间戳估出来的
/// 请求耗时和首字时间（见 [`RequestTimer`]），估不出来传 None。
#[allow(clippy::too_many_arguments)]
fn insert_codex_session_entry_on_conn(
    conn: &rusqlite::Connection,
    request_id: &str,
    delta: &DeltaTokens,
    model: &str,
    service_tier: CodexServiceTier,
    session_id: Option<&str>,
    timestamp: Option<&str>,
    timing: Option<EstimatedTiming>,
    suspected_duplicates: &mut u32,
    pricing_cache: &mut HashMap<String, Option<ModelPricing>>,
) -> Result<bool, AppError> {
    let created_at = timestamp
        .and_then(|ts| {
            chrono::DateTime::parse_from_rfc3339(ts)
                .ok()
                .map(|dt| dt.timestamp())
        })
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

    let input_token_semantics = if delta.cache_write_reported {
        INPUT_TOKEN_SEMANTICS_TOTAL
    } else {
        INPUT_TOKEN_SEMANTICS_LEGACY
    };

    let dedup_key = DedupKey {
        app_type: "codex",
        model,
        input_tokens: delta.input,
        output_tokens: delta.output,
        cache_read_tokens: delta.cached_input,
        cache_creation_tokens: delta.cache_write_input,
        cache_write_known: delta.cache_write_reported,
        created_at,
    };
    if should_skip_session_insert(conn, request_id, &dedup_key)? {
        return Ok(false);
    }
    if has_suspected_codex_session_duplicate(conn, request_id, &dedup_key)? {
        *suspected_duplicates = suspected_duplicates.saturating_add(1);
        log::warn!(
            "[CODEX-SYNC] 疑似重复会话用量: request_id={request_id}, model={model}, tier={service_tier:?}, input={}, output={}, cache_read={}, cache_write={}",
            delta.input,
            delta.output,
            delta.cached_input,
            delta.cache_write_input
        );
    }

    // 计算费用
    let usage = TokenUsage {
        input_tokens: delta.input,
        output_tokens: delta.output,
        cache_read_tokens: delta.cached_input,
        cache_creation_tokens: delta.cache_write_input,
        cache_creation_1h_tokens: 0,
        model: Some(model.to_string()),
        message_id: None,
    };

    let pricing = pricing_cache
        .entry(model.to_string())
        .or_insert_with(|| find_codex_pricing(conn, model));
    let multiplier = service_tier.quota_cost_multiplier(model);
    let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) = match pricing
    {
        Some(p) => {
            let cost = CostCalculator::calculate_for_app("codex", &usage, p, multiplier);
            (
                cost.input_cost.to_string(),
                cost.output_cost.to_string(),
                cost.cache_read_cost.to_string(),
                cost.cache_creation_cost.to_string(),
                cost.total_cost.to_string(),
            )
        }
        None => (
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
        ),
    };

    let inserted_rows = conn
        .prepare_cached(
                    "INSERT INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        )
        .and_then(|mut stmt| stmt.execute(rusqlite::params![
                request_id,
                "_codex_session",    // provider_id
                "codex",             // app_type
                model,
                model,               // request_model = model
                model,               // pricing_model = normalized session model
                delta.input,
                delta.output,
                delta.cached_input,
                delta.cache_write_input,
                input_token_semantics,
                input_cost,
                output_cost,
                cache_read_cost,
                cache_creation_cost,
                total_cost,
                timing.map_or(0, |t| t.latency_ms), // latency_ms: 按时间戳估算，0 = 没有计时
                timing.and_then(|t| t.first_token_ms), // first_token_ms: 按时间戳估算，推不出来为 NULL
                200i64,              // status_code
                Option::<String>::None, // error_message
                session_id.map(|s| s.to_string()),
                Some("codex_session"), // provider_type
                1i64,                // is_streaming
                multiplier.to_string(),
                created_at,
                "codex_session",     // data_source
            ]))
        .map_err(|e| AppError::Database(format!("插入 Codex 会话日志失败: {e}")))?;

    Ok(inserted_rows > 0)
}

/// 查找 Codex 模型定价（带归一化）
fn find_codex_pricing(conn: &rusqlite::Connection, model_id: &str) -> Option<ModelPricing> {
    let model_id = normalize_codex_model(model_id);
    let mut pricing = find_model_pricing(conn, &model_id)?;
    // This importer estimates subscription quota, not an API invoice. Codex
    // Credits have no separate cache-write charge. This calculator splits writes
    // out of total input, so bill them at the ordinary input rate, not zero or
    // the API write premium. Keep shared prices and recorded tokens intact.
    // https://learn.chatgpt.com/docs/pricing#token-rates
    if CodexServiceTier::Fast.quota_cost_multiplier(&model_id) > Decimal::ONE {
        pricing.cache_creation_cost_per_million = pricing.input_cost_per_million;
    }
    Some(pricing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_usage::get_sync_state;
    use std::io::Write;
    use tempfile::tempdir;

    const PARENT_ID: &str = "00000000-0000-4000-8000-000000000001";
    const CHILD_A_ID: &str = "00000000-0000-4000-8000-000000000002";
    const CHILD_B_ID: &str = "00000000-0000-4000-8000-000000000003";
    const PARENT_PAGE_ID: &str = "00000000-0000-4000-8000-000000000004";
    const CHILD_A_PAGE_ID: &str = "00000000-0000-4000-8000-000000000005";
    const CHILD_A_PAGE_2_ID: &str = "00000000-0000-4000-8000-000000000006";

    thread_local! {
        static CODEX_PARSE_COUNTS: std::cell::RefCell<HashMap<PathBuf, usize>> =
            std::cell::RefCell::new(HashMap::new());
    }

    pub(super) fn record_codex_parse(path: &Path) {
        CODEX_PARSE_COUNTS.with(|counts| {
            *counts.borrow_mut().entry(path.to_path_buf()).or_default() += 1;
        });
    }

    fn codex_parse_count(path: &Path) -> usize {
        CODEX_PARSE_COUNTS.with(|counts| counts.borrow().get(path).copied().unwrap_or(0))
    }

    fn retryable_parent_and_child(dir: &Path) -> (PathBuf, PathBuf) {
        let parent = rollout_path(dir, PARENT_ID);
        let child = rollout_path(dir, CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:02Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, None, Some(PARENT_ID), "2026-07-10T03:00:05Z"),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:06Z"),
                token_count_at(1_300, 1_050, 150, "2026-07-10T03:00:07Z"),
            ],
        );
        (parent, child)
    }

    fn append_jsonl(path: &Path, value: serde_json::Value) {
        use std::io::Write;
        let mut writer = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(writer, "{value}").unwrap();
    }

    fn write_jsonl(path: &Path, values: &[serde_json::Value]) {
        let contents = values
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(path, contents).unwrap();
    }

    fn rollout_path(dir: &Path, thread_id: &str) -> PathBuf {
        dir.join(format!("rollout-2026-07-10T03-00-00-{thread_id}.jsonl"))
    }

    fn paginated_session_meta_at(
        thread_id: &str,
        forked_from_id: Option<&str>,
        spawned_from_id: Option<&str>,
        history_base_thread_id: Option<&str>,
        timestamp: &str,
    ) -> serde_json::Value {
        let source = spawned_from_id.map_or_else(
            || serde_json::Value::String("cli".to_string()),
            |parent| {
                serde_json::json!({
                    "subagent": {
                        "thread_spawn": { "parent_thread_id": parent }
                    }
                })
            },
        );
        let mut payload = serde_json::json!({
            "id": thread_id,
            "forked_from_id": forked_from_id,
            "source": source,
            "history_mode": "paginated",
        });
        if let Some(hb_id) = history_base_thread_id {
            payload["history_base"] = serde_json::json!({
                "thread_id": hb_id,
                "end_ordinal_exclusive": 100,
                "end_byte_offset": 5000,
            });
        }
        serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": payload
        })
    }

    fn session_meta_at(
        thread_id: &str,
        forked_from_id: Option<&str>,
        spawned_from_id: Option<&str>,
        timestamp: &str,
    ) -> serde_json::Value {
        let source = spawned_from_id.map_or_else(
            || serde_json::Value::String("cli".to_string()),
            |parent| {
                serde_json::json!({
                    "subagent": {
                        "thread_spawn": { "parent_thread_id": parent }
                    }
                })
            },
        );
        serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "forked_from_id": forked_from_id,
                "source": source
            }
        })
    }

    fn session_meta(thread_id: &str) -> serde_json::Value {
        session_meta_at(thread_id, None, None, "2026-07-10T03:00:00Z")
    }

    fn turn_context_for_model_at(model: &str, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "turn_context",
            "payload": { "model": model }
        })
    }

    fn turn_context_at(timestamp: &str) -> serde_json::Value {
        turn_context_for_model_at("gpt-5.6-sol", timestamp)
    }

    fn turn_context() -> serde_json::Value {
        turn_context_at("2026-07-10T03:00:01Z")
    }

    fn thread_settings_at(model: serde_json::Value, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": { "model": model }
            }
        })
    }

    fn thread_settings_with_tier_at(
        service_tier: Option<&str>,
        model: &str,
        timestamp: &str,
    ) -> serde_json::Value {
        let mut event = thread_settings_at(serde_json::json!(model), timestamp);
        if let Some(tier) = service_tier {
            event["payload"]["thread_settings"]["service_tier"] = serde_json::json!(tier);
        }
        event
    }

    fn token_count_at(input: u64, cached: u64, output: u64, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "output_tokens": output,
                    "reasoning_output_tokens": 0,
                    "total_tokens": input + output
                }}
            }
        })
    }

    fn token_count(input: u64, cached: u64, output: u64) -> serde_json::Value {
        token_count_at(input, cached, output, "2026-07-10T03:00:02Z")
    }

    fn token_count_with_cache_write_at(
        input: u64,
        cached: u64,
        cache_write: u64,
        output: u64,
        timestamp: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "cache_write_input_tokens": cache_write,
                    "output_tokens": output,
                    "reasoning_output_tokens": 0,
                    "total_tokens": input + output
                }}
            }
        })
    }

    fn token_count_without_timestamp(input: u64, cached: u64, output: u64) -> serde_json::Value {
        let mut value = token_count(input, cached, output);
        value
            .as_object_mut()
            .expect("token_count must be an object")
            .remove("timestamp");
        value
    }

    #[allow(clippy::too_many_arguments)]
    fn token_count_with_last_at(
        total_input: u64,
        total_cached: u64,
        total_output: u64,
        last_input: u64,
        last_cached: u64,
        last_output: u64,
        limit_id: &str,
        timestamp: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": total_input,
                        "cached_input_tokens": total_cached,
                        "output_tokens": total_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": total_input + total_output
                    },
                    "last_token_usage": {
                        "input_tokens": last_input,
                        "cached_input_tokens": last_cached,
                        "output_tokens": last_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": last_input + last_output
                    }
                },
                "rate_limits": { "limit_id": limit_id }
            }
        })
    }

    fn token_usage_record_at(
        response_id: &str,
        input: u64,
        cached: u64,
        output: u64,
        timestamp: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "token_usage_record",
            "payload": {
                "response_id": response_id,
                "usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "cache_write_input_tokens": 0,
                    "output_tokens": output,
                    "reasoning_output_tokens": 0,
                    "total_tokens": input + output
                }
            }
        })
    }

    fn codex_usage_rows(db: &Database) -> Result<Vec<(String, i64, i64)>, AppError> {
        let conn = lock_conn!(db.conn);
        let mut stmt = conn
            .prepare(
                "SELECT request_id, input_tokens, output_tokens
                 FROM proxy_request_logs WHERE data_source = 'codex_session'
                 ORDER BY request_id",
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| AppError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(rows)
    }

    #[test]
    fn remote_compaction_usage_record_is_imported_when_the_following_token_count_is_zero(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let path = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &path,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_usage_record_at("resp_compact", 26_705, 0, 491, "2026-07-10T03:00:02Z"),
                token_count_with_last_at(0, 0, 0, 0, 0, 0, "default", "2026-07-10T03:00:02Z"),
            ],
        );

        let result = sync_test_file(&db, &path, &[&path])?;

        assert_eq!(result.imported, 1, "compaction usage must be imported");
        let rows = codex_usage_rows(&db)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, 26_705);
        assert_eq!(rows[0].2, 491);
        assert!(rows[0].0.ends_with(":usage:resp_compact"));
        Ok(())
    }

    #[test]
    fn usage_record_covered_by_token_count_is_not_imported_twice() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let path = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &path,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_usage_record_at("resp_turn", 26_705, 1_000, 491, "2026-07-10T03:00:02Z"),
                token_count_with_last_at(
                    26_705,
                    1_000,
                    491,
                    26_705,
                    1_000,
                    491,
                    "default",
                    "2026-07-10T03:00:03Z",
                ),
            ],
        );

        let result = sync_test_file(&db, &path, &[&path])?;

        assert_eq!(result.imported, 1);
        let rows = codex_usage_rows(&db)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, 26_705);
        assert_eq!(rows[0].2, 491);
        assert_eq!(
            rows[0].0,
            format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{PARENT_ID}:1")
        );
        Ok(())
    }

    #[test]
    fn compaction_retains_fast_cache_write_and_rebuild_fingerprint() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let path = rollout_path(temp.path(), PARENT_ID);
        let mut usage = token_usage_record_at(
            "resp_compact",
            1_000_000,
            100_000,
            10_000,
            "2026-07-10T03:00:02Z",
        );
        usage["payload"]["usage"]["cache_write_input_tokens"] = serde_json::json!(100_000);
        let lines = [
            session_meta(PARENT_ID),
            thread_settings_with_tier_at(Some("priority"), "gpt-5.6-sol", "2026-07-10T03:00:01Z"),
            usage,
            token_count_with_last_at(0, 0, 0, 0, 0, 0, "default", "2026-07-10T03:00:02Z"),
        ];
        write_jsonl(&path, &lines);
        let parsed = parse_codex_file(&path, Some(PARENT_ID.into()))?;
        let before = CodexRebuildFileFingerprint::from_parsed(&parsed);
        assert_eq!(parsed.usage_record_events.len(), 1);
        assert!(parsed.usage_record_events[0].delta.cache_write_reported);
        assert_eq!(sync_test_file(&db, &path, &[&path])?.imported, 1);
        assert_eq!(sync_test_file(&db, &path, &[&path])?.imported, 0);
        {
            let conn = lock_conn!(db.conn);
            let (writes, semantics, multiplier, write_cost): (i64, i64, String, String) = conn.query_row(
                "SELECT cache_creation_tokens, input_token_semantics, cost_multiplier, cache_creation_cost_usd
                 FROM proxy_request_logs WHERE data_source='codex_session'",
                [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            assert_eq!(writes, 100_000);
            assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_TOTAL);
            assert_eq!(multiplier.parse::<Decimal>().unwrap(), Decimal::new(25, 1));
            assert_eq!(write_cost.parse::<Decimal>().unwrap(), Decimal::new(4, 1));
        }
        let mut changed = lines.clone();
        changed[2]["payload"]["response_id"] = serde_json::json!("different-response");
        write_jsonl(&path, &changed);
        let after = CodexRebuildFileFingerprint::from_parsed(&parse_codex_file(
            &path,
            Some(PARENT_ID.into()),
        )?);
        assert!(!before.is_prefix_compatible_with(&after));
        changed[2]["payload"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_write_input_tokens");
        write_jsonl(&path, &changed);
        let missing = parse_codex_file(&path, Some(PARENT_ID.into()))?;
        assert!(!missing.usage_record_events[0].delta.cache_write_reported);
        Ok(())
    }

    #[test]
    fn already_scanned_compaction_stays_behind_the_cursor() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let path = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &path,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_usage_record_at("resp_compact", 26_705, 0, 491, "2026-07-10T03:00:02Z"),
                token_count_with_last_at(0, 0, 0, 0, 0, 0, "default", "2026-07-10T03:00:02Z"),
            ],
        );
        let metadata = fs::metadata(&path).unwrap();
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO session_log_sync
                    (file_path, last_modified, last_line_offset, last_byte_offset, last_synced_at)
                 VALUES (?1, ?2, 100, ?3, 0)",
                rusqlite::params![
                    path.to_string_lossy().to_string(),
                    metadata_modified_nanos(&metadata),
                    i64::try_from(metadata.len()).unwrap()
                ],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        }

        let result = sync_test_file(&db, &path, &[&path])?;

        assert_eq!(result.imported, 0);
        assert!(codex_usage_rows(&db)?.is_empty());
        Ok(())
    }

    #[test]
    fn compaction_confirmed_on_next_scan_is_imported_once() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let path = rollout_path(temp.path(), PARENT_ID);
        let mut lines = vec![
            session_meta(PARENT_ID),
            turn_context(),
            token_usage_record_at("resp_later", 1000, 100, 10, "2026-07-10T03:00:02Z"),
        ];
        write_jsonl(&path, &lines);
        assert_eq!(sync_test_file(&db, &path, &[&path])?.imported, 0);
        lines.push(token_count_with_last_at(
            0,
            0,
            0,
            0,
            0,
            0,
            "default",
            "2026-07-10T03:00:02Z",
        ));
        write_jsonl(&path, &lines);
        assert_eq!(sync_test_file(&db, &path, &[&path])?.imported, 1);
        assert_eq!(sync_test_file(&db, &path, &[&path])?.imported, 0);
        assert_eq!(codex_usage_rows(&db)?.len(), 1);
        Ok(())
    }

    fn sync_test_file(
        db: &Database,
        file: &Path,
        all_files: &[&Path],
    ) -> Result<CodexFileSyncResult, AppError> {
        let files = all_files
            .iter()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();
        let mut pass = CodexSyncPass::load(db)?;
        sync_single_codex_file(db, file, &build_rollout_index(&files), &mut pass)
    }

    fn thread_request_id(thread_id: &str, event_index: u32) -> String {
        format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{thread_id}:{event_index}")
    }

    fn staged_codex_row(
        thread_id: &str,
        event_index: u32,
        input_tokens: u32,
        total_cost_usd: &str,
        created_at: i64,
    ) -> StagedCodexRow {
        StagedCodexRow {
            request_id: thread_request_id(thread_id, event_index),
            model: "gpt-5.6-sol".to_string(),
            request_model: Some("gpt-5.6-sol".to_string()),
            pricing_model: Some("gpt-5.6-sol".to_string()),
            input_tokens,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
            input_token_semantics: INPUT_TOKEN_SEMANTICS_TOTAL,
            input_cost_usd: total_cost_usd.to_string(),
            output_cost_usd: "0".to_string(),
            cache_read_cost_usd: "0".to_string(),
            cache_write_cost_usd: "0".to_string(),
            total_cost_usd: total_cost_usd.to_string(),
            session_id: thread_id.to_string(),
            cost_multiplier: "2.5".to_string(),
            created_at,
        }
    }

    fn insert_live_codex_row(
        db: &Database,
        thread_id: &str,
        event_index: u32,
        input_tokens: u32,
        total_cost_usd: &str,
        created_at: i64,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(db.conn);
        conn.execute(
            "INSERT INTO proxy_request_logs (
                request_id, provider_id, app_type, model, request_model, pricing_model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_token_semantics, input_cost_usd, output_cost_usd,
                cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
                latency_ms, status_code, session_id, provider_type, is_streaming,
                cost_multiplier, created_at, data_source
             ) VALUES (
                ?1, '_codex_session', 'codex', 'gpt-5.6-sol', 'gpt-5.6-sol',
                'gpt-5.6-sol', ?2, 2, 3, 4, ?3, ?4, '0', '0', '0', ?4,
                0, 200, ?5, 'codex_session', 1, '1', ?6, 'codex_session'
             )",
            rusqlite::params![
                thread_request_id(thread_id, event_index),
                input_tokens,
                INPUT_TOKEN_SEMANTICS_TOTAL,
                total_cost_usd,
                thread_id,
                created_at,
            ],
        )?;
        Ok(())
    }

    fn assert_unchanged_codex_file_is_skipped(db: &Database, file: &Path) -> Result<(), AppError> {
        let changes_before: i64 = {
            let conn = lock_conn!(db.conn);
            conn.query_row("SELECT total_changes()", [], |row| row.get(0))?
        };
        // Reload the persisted cursor each time: returning zero imports alone
        // does not prove that the file was skipped rather than fully reparsed.
        for _ in 0..3 {
            assert_eq!(sync_test_file(db, file, &[file])?.imported, 0);
        }
        let conn = lock_conn!(db.conn);
        let changes_after: i64 = conn.query_row("SELECT total_changes()", [], |row| row.get(0))?;
        assert_eq!(
            changes_after, changes_before,
            "unchanged file rewrote its cursor"
        );
        Ok(())
    }

    #[test]
    fn test_unchanged_incomplete_tail_is_skipped_after_cursor_reload() -> Result<(), AppError> {
        use std::io::Write;
        for has_usage in [false, true] {
            for tail in ["{\"type\":\"event_msg\"", "  "] {
                let db = Database::memory()?;
                let dir = tempdir().unwrap();
                let file = rollout_path(dir.path(), PARENT_ID);
                let mut records = vec![session_meta(PARENT_ID), turn_context()];
                if has_usage {
                    records.push(token_count(100, 50, 10));
                }
                write_jsonl(&file, &records);
                {
                    let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
                    writer.write_all(tail.as_bytes()).unwrap();
                }
                assert_eq!(
                    sync_test_file(&db, &file, &[&file])?.imported,
                    u32::from(has_usage)
                );
                assert_eq!(
                    get_sync_state(&db, &file.to_string_lossy())?.1,
                    records.len() as i64
                );
                assert_unchanged_codex_file_is_skipped(&db, &file)?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_append_with_unchanged_mtime_survives_reload_without_duplicates() -> Result<(), AppError>
    {
        use std::io::Write;
        let db = Database::memory()?;
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(100, 50, 10),
            ],
        );
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writeln!(writer, "{}", token_count(250, 100, 30)).unwrap();
        writer
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        // Each call reloads its cursor from the DB, as after an application restart.
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        let conn = lock_conn!(db.conn);
        let totals: (i64, i64, i64) = conn.query_row(
            "SELECT count(*), sum(input_tokens), sum(output_tokens) FROM proxy_request_logs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(totals, (2, 250, 30));
        Ok(())
    }

    #[test]
    fn test_legacy_cursor_catches_up_with_unchanged_mtime() -> Result<(), AppError> {
        let db = Database::memory()?;
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(100, 50, 10),
                token_count(250, 100, 30),
            ],
        );
        let modified = metadata_modified_nanos(&fs::metadata(&file).unwrap());
        update_sync_state(&db, &file.to_string_lossy(), modified, 3)?;
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        let conn = lock_conn!(db.conn);
        let bytes: i64 =
            conn.query_row("SELECT last_byte_offset FROM session_log_sync", [], |r| {
                r.get(0)
            })?;
        assert_eq!(bytes as u64, fs::metadata(&file).unwrap().len());
        Ok(())
    }

    #[test]
    fn test_complete_final_record_without_newline_is_imported_once() -> Result<(), AppError> {
        use std::io::Write;
        let db = Database::memory()?;
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(&file, &[session_meta(PARENT_ID), turn_context()]);
        let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
        write!(writer, "{}", token_count(100, 50, 10)).unwrap();
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        writeln!(writer).unwrap();
        writeln!(writer, "{}", token_count(250, 100, 30)).unwrap();
        writer
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        let conn = lock_conn!(db.conn);
        let totals: (i64, i64, i64) = conn.query_row(
            "SELECT count(*), sum(input_tokens), sum(output_tokens) FROM proxy_request_logs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(totals, (2, 250, 30));
        Ok(())
    }

    #[test]
    fn test_partial_live_record_is_retried_after_append() -> Result<(), AppError> {
        use std::io::Write;
        let db = Database::memory()?;
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(100, 50, 10),
            ],
        );
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        let next = format!("{}\n", token_count(250, 100, 30));
        let split = next.len() / 2;
        let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writer.write_all(&next.as_bytes()[..split]).unwrap();
        writer
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?.1, 3);
        assert_unchanged_codex_file_is_skipped(&db, &file)?;
        writer.write_all(&next.as_bytes()[split..]).unwrap();
        writer
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?.1, 4);
        Ok(())
    }

    /// revert 产生的替换 rollout 是 `<threadId>_<rolloutId>` 双段文件名，
    /// root meta 的 id 是原线程 ID（第一个 UUID）、forked_from_id 为空。
    /// 旧校验只认末尾 UUID，会把这类文件永久 deferred，其后 resume 追加
    /// 的用量全部丢失。
    #[test]
    fn test_resumed_rollout_meta_id_matching_leading_uuid_is_not_deferred() -> Result<(), AppError>
    {
        let dir = tempdir().unwrap();
        let file = dir.path().join(format!(
            "rollout-2026-08-26T17-18-13-{PARENT_ID}_{CHILD_A_ID}.jsonl"
        ));
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(100, 50, 20, "2026-08-26T09:18:20Z"),
            ],
        );

        let parsed = parse_codex_file(&file, thread_id_from_filename(&file))?;

        // 恢复会话没有显式 parent，不应因 ID 不一致被拒
        assert!(
            !matches!(parsed.parent, ParentResolution::Deferred(_)),
            "恢复会话不应被 deferred，实际: {:?}",
            parsed.parent
        );
        assert_eq!(parsed.root_thread_id.as_deref(), Some(CHILD_A_ID));
        assert!(parsed.has_billable_tokens);
        Ok(())
    }

    /// 分支线程翻页产生的双段文件名带有残留的 forked_from_id，
    /// 解析时应被识别为本线程延续（Continuation），并恢复上一页基线计算增量。
    #[test]
    fn test_parse_codex_file_recognizes_branch_thread_paginated_continuation(
    ) -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let root = rollout_path(dir.path(), CHILD_A_ID);
        write_jsonl(
            &root,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-08-26T09:18:00Z"),
                token_count_at(50, 25, 10, "2026-08-26T09:18:05Z"),
            ],
        );
        let file = dir.path().join(format!(
            "rollout-2026-08-26T17-18-13-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &file,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_ID),
                    "2026-08-26T09:18:20Z",
                ),
                turn_context(),
                token_count_at(100, 50, 20, "2026-08-26T09:18:21Z"),
            ],
        );

        let index = build_rollout_index(&[root, file.clone()]);
        let parsed =
            parse_codex_file_with_index(&file, thread_id_from_filename(&file), Some(&index))?;

        assert!(
            matches!(parsed.parent, ParentResolution::Continuation { .. }),
            "分支线程续写页应被识别为 Continuation，实际: {:?}",
            parsed.parent
        );
        assert_eq!(parsed.root_thread_id.as_deref(), Some(CHILD_A_PAGE_ID));
        assert_eq!(parsed.meta_thread_id.as_deref(), Some(CHILD_A_ID));
        assert!(parsed.has_billable_tokens);
        // 验证前序基线恢复：100 - 50 = 50 增量
        assert_eq!(parsed.token_events[0].delta.input, 50);
        assert_eq!(parsed.token_events[0].delta.output, 10);
        Ok(())
    }

    /// 完整同步链路下双段 rollout 的两个 UUID 分工：request_id 用尾部
    /// rollout ID（event_index 按物理文件计数，去重键不能换），库里
    /// session_id 记前置线程 ID，与会话管理器侧的会话身份同口径。
    #[test]
    fn test_resumed_rollout_session_id_uses_leading_thread_id() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = temp.path().join(format!(
            "rollout-2026-08-26T17-18-13-{PARENT_ID}_{CHILD_A_ID}.jsonl"
        ));
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(100, 50, 10),
            ],
        );

        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);

        let conn = lock_conn!(db.conn);
        let (request_id, session_id) = conn
            .prepare(
                "SELECT request_id, session_id FROM proxy_request_logs
                 WHERE data_source = 'codex_session'",
            )?
            .query_row([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
        assert_eq!(
            request_id,
            format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_ID}:1")
        );
        assert_eq!(session_id, PARENT_ID);
        Ok(())
    }

    /// 单段文件名的不一致仍要拒收。
    #[test]
    fn test_single_uuid_filename_meta_mismatch_still_deferred() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        // meta 里写的是另一个线程的 ID —— 这不是 revert 双段文件名能解释的形态
        write_jsonl(
            &file,
            &[
                session_meta(CHILD_B_ID),
                turn_context(),
                token_count_at(1, 1, 1, "2026-07-10T03:00:02Z"),
            ],
        );

        let parsed = parse_codex_file(&file, thread_id_from_filename(&file))?;
        assert!(matches!(parsed.parent, ParentResolution::Deferred(_)));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_resumed_rollout_preserves_fast_cache_write_and_identity() -> Result<(), AppError>
    {
        let db = Database::memory()?;
        let staging = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = temp.path().join(format!(
            "rollout-2026-08-26T17-18-13-{PARENT_ID}_{CHILD_A_ID}.jsonl"
        ));
        let timestamp = "2026-08-26T09:18:20Z";
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_with_tier_at(Some("priority"), "gpt-5.6-sol", timestamp),
                token_count_with_cache_write_at(100, 20, 10, 5, timestamp),
            ],
        );
        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        assert_eq!(sync_test_file(&staging, &file, &[&file])?.imported, 1);
        // Include an existing row with separate rollout and session IDs so
        // the rebuild exercises both live and staged identity validation.
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        let staged_rows = load_staged_codex_rows(&staging)?;
        let cursors = load_staged_codex_cursors(&staging)?;
        let blocked = finalize_codex_rebuild_audit(audit, &cursors);
        assert!(blocked.is_empty());
        assert_eq!(
            apply_staged_codex_usage(&db, staged_rows.clone(), cursors.clone(), &blocked)?,
            (1, 0)
        );
        assert_eq!(
            apply_staged_codex_usage(&db, staged_rows, cursors, &blocked)?,
            (1, 0)
        );
        let conn = lock_conn!(db.conn);
        let rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'codex_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(rows, 1);
        let (session_id, cache_write, multiplier): (String, i64, String) = conn.query_row(
            "SELECT session_id, cache_creation_tokens, cost_multiplier
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(CHILD_A_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(session_id, PARENT_ID);
        assert_eq!(cache_write, 10);
        assert_eq!(multiplier.parse::<Decimal>().unwrap(), Decimal::new(25, 1));
        let offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [file.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(offset, 3);
        let byte_offset: i64 = conn.query_row(
            "SELECT last_byte_offset FROM session_log_sync WHERE file_path = ?1",
            [file.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(byte_offset as u64, fs::metadata(&file).unwrap().len());
        drop(conn);
        assert_unchanged_codex_file_is_skipped(&db, &file)?;
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_rejects_unverified_resumed_identity() -> Result<(), AppError> {
        for leading_id in [None, Some(CHILD_B_ID)] {
            let db = Database::memory()?;
            let mut row = staged_codex_row(CHILD_A_ID, 1, 100, "1", 1_700_000_000);
            row.session_id = PARENT_ID.to_string();
            let filename = leading_id.map_or_else(
                || format!("rollout-2026-08-26-{CHILD_A_ID}.jsonl"),
                |id| format!("rollout-2026-08-26-{id}_{CHILD_A_ID}.jsonl"),
            );
            let cursor = StagedCodexCursor {
                file_path: filename,
                last_modified: 1,
                last_line_offset: 3,
                last_byte_offset: None,
                last_synced_at: 1,
            };
            assert_eq!(
                apply_staged_codex_usage(&db, vec![row], vec![cursor], &HashSet::new())?,
                (0, 1)
            );
            let conn = lock_conn!(db.conn);
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM session_log_sync", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 0);
        }
        Ok(())
    }

    #[test]
    fn rebuild_audit_blocks_conflicting_resumed_session_ids() {
        let temp = tempdir().unwrap();
        let files = [PARENT_ID, CHILD_B_ID].map(|id| {
            let file = temp
                .path()
                .join(format!("rollout-2026-08-26-{id}_{CHILD_A_ID}.jsonl"));
            write_jsonl(&file, &[session_meta(id), token_count(100, 20, 10)]);
            file
        });
        let audit = audit_codex_rebuild_sources(&files);
        assert!(audit.blocked_threads.contains(CHILD_A_ID));
    }

    #[test]
    fn test_delta_first_event() {
        let prev = None;
        let current = CumulativeTokens {
            input: 17934,
            cached_input: 9600,
            cache_write_input: 0,
            output: 454,
            ..CumulativeTokens::default()
        };
        let delta = compute_delta(&prev, &current);
        assert_eq!(delta.input, 17934);
        assert_eq!(delta.cached_input, 9600);
        assert_eq!(delta.output, 454);
        assert!(!delta.is_zero());
    }

    #[test]
    fn clamp_delta_cache_never_exceeds_total_input() {
        let clamped = clamp_delta_cache(DeltaTokens {
            input: 100,
            cached_input: 80,
            cache_write_input: 50,
            cache_write_reported: true,
            output: 10,
        });
        assert_eq!(clamped.cached_input, 80);
        assert_eq!(clamped.cache_write_input, 20);
        assert_eq!(
            clamped.cached_input + clamped.cache_write_input,
            clamped.input
        );
    }

    #[test]
    fn test_delta_subsequent_event() {
        let prev = Some(CumulativeTokens {
            input: 17934,
            cached_input: 9600,
            cache_write_input: 0,
            output: 454,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 36722,
            cached_input: 27904,
            cache_write_input: 0,
            output: 804,
            ..CumulativeTokens::default()
        };
        let delta = compute_delta(&prev, &current);
        assert_eq!(delta.input, 36722 - 17934);
        assert_eq!(delta.cached_input, 27904 - 9600);
        assert_eq!(delta.output, 804 - 454);
    }

    #[test]
    fn test_delta_zero_at_task_boundary() {
        let prev = Some(CumulativeTokens {
            input: 58346,
            cached_input: 46976,
            cache_write_input: 0,
            output: 1045,
            ..CumulativeTokens::default()
        });
        // task 边界：相同的累计值
        let current = CumulativeTokens {
            input: 58346,
            cached_input: 46976,
            cache_write_input: 0,
            output: 1045,
            ..CumulativeTokens::default()
        };
        let delta = compute_delta(&prev, &current);
        assert!(delta.is_zero());
    }

    #[test]
    fn test_delta_saturating_sub() {
        // 异常情况：当前值小于前值（不应发生，但需防护）
        let prev = Some(CumulativeTokens {
            input: 100,
            cached_input: 50,
            cache_write_input: 0,
            output: 30,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 80,
            cached_input: 40,
            cache_write_input: 0,
            output: 20,
            ..CumulativeTokens::default()
        };
        let delta = compute_delta(&prev, &current);
        assert_eq!(delta.input, 0);
        assert_eq!(delta.cached_input, 0);
        assert_eq!(delta.output, 0);
        assert!(delta.is_zero());
    }

    #[test]
    fn test_cache_write_delta_preserves_reported_zero() {
        let prev = Some(CumulativeTokens {
            input: 100,
            cached_input: 20,
            cache_write_input: 10,
            cache_write_reported: true,
            output: 5,
        });
        let current = CumulativeTokens {
            input: 200,
            cached_input: 50,
            cache_write_input: 10,
            cache_write_reported: true,
            output: 15,
        };

        let delta = compute_delta(&prev, &current);
        assert_eq!(delta.input, 100);
        assert_eq!(delta.cached_input, 30);
        assert_eq!(delta.cache_write_input, 0);
        assert!(delta.cache_write_reported);
        assert_eq!(delta.output, 10);
    }

    #[test]
    fn test_token_signature_distinguishes_missing_and_explicit_zero_cache_write() {
        let without_cache_write = serde_json::json!({
            "total_token_usage": {
                "input_tokens": 100,
                "cached_input_tokens": 20,
                "output_tokens": 5
            }
        });
        let with_zero_cache_write = serde_json::json!({
            "total_token_usage": {
                "input_tokens": 100,
                "cached_input_tokens": 20,
                "cache_write_input_tokens": 0,
                "output_tokens": 5
            }
        });

        assert_ne!(
            parse_token_signature(&without_cache_write),
            parse_token_signature(&with_zero_cache_write)
        );
    }

    #[test]
    fn cumulative_cache_write_fills_missing_last_usage_field() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let mixed_usage = |total_input: u64,
                           total_cached: u64,
                           total_write: u64,
                           total_output: u64,
                           last_input: u64,
                           last_cached: u64,
                           last_output: u64,
                           timestamp: &str| {
            serde_json::json!({
                "timestamp": timestamp,
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": total_input,
                            "cached_input_tokens": total_cached,
                            "cache_write_input_tokens": total_write,
                            "output_tokens": total_output,
                            "total_tokens": total_input + total_output
                        },
                        "last_token_usage": {
                            "input_tokens": last_input,
                            "cached_input_tokens": last_cached,
                            "output_tokens": last_output,
                            "total_tokens": last_input + last_output
                        }
                    }
                }
            })
        };
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                mixed_usage(100, 20, 10, 5, 60, 10, 3, "2026-07-10T03:00:02Z"),
                mixed_usage(200, 50, 25, 10, 70, 15, 4, "2026-07-10T03:00:03Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert_eq!(parsed.token_events.len(), 2);
        assert_eq!(parsed.token_events[0].delta.input, 60);
        assert_eq!(parsed.token_events[0].delta.cache_write_input, 10);
        assert!(parsed.token_events[0].delta.cache_write_reported);
        assert_eq!(parsed.token_events[1].delta.input, 70);
        assert_eq!(parsed.token_events[1].delta.cache_write_input, 15);
        assert!(parsed.token_events[1].delta.cache_write_reported);
        Ok(())
    }

    #[test]
    fn test_service_tier_and_cache_write_follow_jsonl_line_order() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let timestamp = "2026-07-10T03:00:02Z";
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_with_tier_at(Some("priority"), "gpt-5.6-sol", timestamp),
                token_count_with_cache_write_at(100, 20, 10, 5, timestamp),
                thread_settings_with_tier_at(Some("default"), "gpt-5.6-terra", timestamp),
                token_count_with_cache_write_at(200, 50, 20, 15, timestamp),
                thread_settings_with_tier_at(None, "gpt-5.6-luna", timestamp),
                token_count_with_cache_write_at(300, 80, 20, 25, timestamp),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert_eq!(parsed.token_events.len(), 3);
        assert_eq!(parsed.token_events[0].service_tier, CodexServiceTier::Fast);
        assert_eq!(parsed.token_events[0].model, "gpt-5.6-sol");
        assert_eq!(parsed.token_events[0].delta.cache_write_input, 10);
        assert!(parsed.token_events[0].delta.cache_write_reported);
        assert_eq!(
            parsed.token_events[1].service_tier,
            CodexServiceTier::Standard
        );
        assert_eq!(parsed.token_events[1].model, "gpt-5.6-terra");
        assert_eq!(parsed.token_events[1].delta.cache_write_input, 10);
        assert_eq!(
            parsed.token_events[2].service_tier,
            CodexServiceTier::Unknown
        );
        assert_eq!(parsed.token_events[2].model, "gpt-5.6-luna");
        assert_eq!(parsed.token_events[2].delta.cache_write_input, 0);
        assert!(parsed.token_events[2].delta.cache_write_reported);
        Ok(())
    }

    #[test]
    fn test_quota_multiplier_matches_personal_script_rates() {
        for model in [
            "gpt-6-astra",
            "gpt-6-astra-high",
            "gpt-6-astra-xhigh",
            "gpt-6-sol",
            "gpt-6-sol-high",
            "gpt-6-luna",
            "gpt-6-luna-xhigh",
        ] {
            assert_eq!(
                CodexServiceTier::Fast.quota_cost_multiplier(model),
                Decimal::new(25, 1)
            );
            assert_eq!(
                CodexServiceTier::Standard.quota_cost_multiplier(model),
                Decimal::from(1)
            );
            assert_eq!(
                CodexServiceTier::Unknown.quota_cost_multiplier(model),
                Decimal::from(1)
            );
        }
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.6-sol"),
            Decimal::new(25, 1)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.5"),
            Decimal::new(25, 1)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.5-high"),
            Decimal::new(25, 1)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.5-minimal"),
            Decimal::new(25, 1)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.4"),
            Decimal::from(2)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.4-high"),
            Decimal::from(2)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.4-mini-xhigh"),
            Decimal::from(2)
        );
        assert_eq!(
            CodexServiceTier::Fast.quota_cost_multiplier("gpt-5.6-sol-wm"),
            Decimal::from(1)
        );
        assert_eq!(
            CodexServiceTier::Unknown.quota_cost_multiplier("gpt-5.6-sol"),
            Decimal::from(1)
        );
    }

    #[test]
    fn test_interleaved_counter_lanes_use_exact_last_usage() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let bengal_event = token_count_with_last_at(
            87_709_262,
            83_563_008,
            240_919,
            151_258,
            147_200,
            87,
            "codex_bengalfox",
            "2026-07-10T03:00:03Z",
        );
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_with_last_at(
                    76_780_408,
                    73_010_432,
                    243_036,
                    175_074,
                    169_728,
                    6_827,
                    "codex",
                    "2026-07-10T03:00:02Z",
                ),
                bengal_event.clone(),
                token_count_with_last_at(
                    76_962_538,
                    73_180_160,
                    243_258,
                    182_130,
                    169_728,
                    222,
                    "codex",
                    "2026-07-10T03:00:04Z",
                ),
                // Repeated snapshots are notifications, not additional API usage.
                bengal_event,
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| {
                (
                    event.delta.input,
                    event.delta.cached_input,
                    event.delta.output,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            deltas,
            vec![
                (175_074, 169_728, 6_827),
                (151_258, 147_200, 87),
                (182_130, 169_728, 222),
            ]
        );
        assert!(parsed.token_events[3].delta.is_zero());
        Ok(())
    }

    #[test]
    fn test_cross_limit_snapshot_replay_is_not_double_counted() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                token_count_with_last_at(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:03Z",
                ),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100]);
        Ok(())
    }

    #[test]
    fn test_adjacent_replay_burst_across_multiple_sources_is_deduped() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                token_count_with_last_at(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:03Z",
                ),
                token_count_with_last_at(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_spark",
                    "2026-07-10T03:00:04Z",
                ),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100]);
        Ok(())
    }

    #[test]
    fn test_cross_source_replay_remains_adjacent_across_non_token_events() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                turn_context_for_model_at("gpt-5.6-sol", "2026-07-10T03:00:03Z"),
                token_count_with_last_at(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:04Z",
                ),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100]);
        Ok(())
    }

    #[test]
    fn test_same_source_repeat_is_deduped_after_another_source_advances() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                token_count_with_last_at(
                    2_000,
                    0,
                    20,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:03Z",
                ),
                // `codex` has not advanced since its X snapshot, so this is a
                // same-source replay even though another source was interleaved.
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:04Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100, 100]);
        Ok(())
    }

    #[test]
    fn test_stale_cross_source_signature_does_not_swallow_reset() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                // `codex` emits snapshot X.
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:02Z"),
                // X is replayed under another rate-limit source.
                token_count_with_last_at(
                    1_000,
                    0,
                    10,
                    100,
                    0,
                    10,
                    "codex_bengalfox",
                    "2026-07-10T03:00:03Z",
                ),
                // The original source advances to Y.
                token_count_with_last_at(2_000, 0, 20, 100, 0, 10, "codex", "2026-07-10T03:00:04Z"),
                // A genuine reset later reproduces X. The stale copy retained
                // by `codex_bengalfox` must not classify this as a replay.
                token_count_with_last_at(1_000, 0, 10, 100, 0, 10, "codex", "2026-07-10T03:00:05Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100, 100, 100]);
        Ok(())
    }

    #[test]
    fn test_full_snapshot_dedupe_allows_counter_reset() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let first =
            token_count_with_last_at(100, 50, 10, 100, 50, 10, "codex", "2026-07-10T03:00:02Z");
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                first.clone(),
                first,
                token_count_with_last_at(
                    200,
                    100,
                    20,
                    100,
                    50,
                    10,
                    "codex",
                    "2026-07-10T03:00:04Z",
                ),
                // A restarted counter may legitimately return to an older
                // total after another full snapshot has advanced the source.
                token_count_with_last_at(100, 50, 10, 50, 25, 5, "codex", "2026-07-10T03:00:05Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100, 100, 50]);
        Ok(())
    }

    #[test]
    fn test_cache_write_tokens_are_billed_at_subscription_input_rate() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        // Codex reports cache writes as a subset of input_tokens, disjoint from cached reads.
        let usage = serde_json::json!({
            "input_tokens": 1_000,
            "cached_input_tokens": 300,
            "cache_write_input_tokens": 600,
            "output_tokens": 50,
            "reasoning_output_tokens": 0,
            "total_tokens": 1_050
        });
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                serde_json::json!({
                    "timestamp": "2026-07-10T03:00:02Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": { "total_token_usage": usage, "last_token_usage": usage }
                    }
                }),
            ],
        );

        let db = Database::memory()?;
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);

        let conn = lock_conn!(db.conn);
        let fresh_input = crate::services::sql_helpers::fresh_input_sql("");
        let (fresh, cache_read, cache_creation, cache_creation_cost, total_cost): (
            i64,
            i64,
            i64,
            String,
            String,
        ) = conn.query_row(
            &format!(
                "SELECT {fresh_input}, cache_read_tokens, cache_creation_tokens,
                        cache_creation_cost_usd, total_cost_usd
                 FROM proxy_request_logs WHERE data_source = 'codex_session'"
            ),
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        assert_eq!((fresh, cache_read, cache_creation), (100, 300, 600));
        // Subscription estimate: $4 input and cache write, $20 output, $0.40 cache read.
        // The shared API price table still retains its separate cache-write premium.
        assert_eq!(
            cache_creation_cost.parse::<Decimal>().unwrap(),
            "0.0024".parse().unwrap()
        );
        assert_eq!(
            total_cost.parse::<Decimal>().unwrap(),
            "0.00392".parse().unwrap()
        );
        Ok(())
    }

    #[test]
    fn test_empty_last_usage_falls_back_to_total() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                serde_json::json!({
                    "timestamp": "2026-07-10T03:00:02Z",
                    "type": "event_msg",
                    "payload": {
                        "type": "token_count",
                        "info": {
                            "total_token_usage": {
                                "input_tokens": 100,
                                "cached_input_tokens": 0,
                                "output_tokens": 10,
                                "reasoning_output_tokens": 0,
                                "total_tokens": 110
                            },
                            "last_token_usage": {}
                        }
                    }
                }),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100]);
        Ok(())
    }

    #[test]
    fn test_empty_total_does_not_enable_snapshot_deduplication() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let event = |limit_id: &str, timestamp: &str| {
            serde_json::json!({
                "timestamp": timestamp,
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {},
                        "last_token_usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 0,
                            "output_tokens": 10,
                            "reasoning_output_tokens": 0,
                            "total_tokens": 110
                        }
                    },
                    "rate_limits": { "limit_id": limit_id }
                }
            })
        };
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                event("codex", "2026-07-10T03:00:02Z"),
                // Without a usable cumulative total, identical per-request
                // usage is not enough evidence that this is a replay.
                event("codex_bengalfox", "2026-07-10T03:00:03Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100, 100]);
        Ok(())
    }

    #[test]
    fn test_total_fallback_uses_session_baseline_across_model_switch() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context_for_model_at("model-a", "2026-07-10T03:00:01Z"),
                token_count_at(100, 50, 10, "2026-07-10T03:00:02Z"),
                turn_context_for_model_at("model-b", "2026-07-10T03:00:03Z"),
                token_count_at(150, 75, 15, "2026-07-10T03:00:04Z"),
            ],
        );

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        let deltas = parsed
            .token_events
            .iter()
            .filter(|event| !event.delta.is_zero())
            .map(|event| event.delta.input)
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec![100, 50]);
        Ok(())
    }

    #[test]
    fn test_thread_settings_model_switch_imports_correct_usage_and_cost() -> Result<(), AppError> {
        use std::io::Write;

        let db = Database::memory()?;
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let first_usage = token_count_at(100, 50, 10, "2026-07-10T03:00:02Z");
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context_for_model_at("gpt-6-astra", "2026-07-10T03:00:01Z"),
                first_usage.clone(),
            ],
        );
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);

        {
            let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
            for event in [
                thread_settings_at(
                    serde_json::json!("OpenAI/GPT-6.1-SOL-2026-10-01"),
                    "2026-07-10T03:00:03Z",
                ),
                // A model change must not turn a repeated snapshot into usage.
                first_usage,
                token_count_at(150, 75, 15, "2026-07-10T03:00:04Z"),
            ] {
                writeln!(writer, "{event}").unwrap();
            }
        }
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_unchanged_codex_file_is_skipped(&db, &file)?;

        let conn = lock_conn!(db.conn);
        let mut statement = conn.prepare(
            "SELECT model, request_model, input_tokens, cache_read_tokens, output_tokens,
                    total_cost_usd, request_id
             FROM proxy_request_logs ORDER BY created_at",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 2);
        for (row, (model, input, cached, output, cost, index)) in rows.iter().zip([
            ("gpt-6-astra", 100, 50, 10, "0.00105", 1),
            ("gpt-6.1-sol", 50, 25, 5, "0.0001025", 2),
        ]) {
            assert_eq!(
                (&*row.0, &*row.1, row.2, row.3, row.4),
                (model, model, input, cached, output)
            );
            assert_eq!(
                row.5.parse::<Decimal>().unwrap(),
                cost.parse::<Decimal>().unwrap()
            );
            assert_eq!(
                row.6,
                format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{PARENT_ID}:{index}")
            );
        }
        Ok(())
    }

    #[test]
    fn test_thread_settings_model_without_turn_context() -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_at(serde_json::json!("gpt-6.1-sol"), "2026-07-10T03:00:01Z"),
                token_count(100, 50, 10),
            ],
        );
        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert_eq!(parsed.token_events.len(), 1);
        assert_eq!(parsed.token_events[0].model, "gpt-6.1-sol");
        Ok(())
    }

    #[test]
    fn test_thread_settings_model_ignores_invalid_metadata_and_allows_overrides(
    ) -> Result<(), AppError> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let mut events = vec![
            session_meta(PARENT_ID),
            thread_settings_at(serde_json::json!("gpt-6.1-sol"), "2026-07-10T03:00:01Z"),
        ];
        for model in [
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!(""),
            serde_json::json!(" \t "),
        ] {
            events.push(thread_settings_at(model, "2026-07-10T03:00:02Z"));
        }
        events.push(serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "thread_settings_applied", "thread_settings": { "service_tier": "priority" } }
        }));
        // The fast text filter is not sufficient evidence of an applied setting.
        let mut unrelated =
            thread_settings_at(serde_json::json!("wrong-model"), "2026-07-10T03:00:02Z");
        unrelated["type"] = serde_json::json!("response_item");
        events.push(unrelated);
        events.push(token_count(100, 50, 10));
        events.push(turn_context_for_model_at(
            "gpt-6-astra",
            "2026-07-10T03:00:03Z",
        ));
        events.push(token_count_at(150, 75, 15, "2026-07-10T03:00:04Z"));
        let mut explicit_model = token_count_at(200, 100, 20, "2026-07-10T03:00:05Z");
        explicit_model["payload"]["info"]["model"] = serde_json::json!("custom-model");
        events.push(explicit_model);
        write_jsonl(&file, &events);

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert_eq!(
            parsed
                .token_events
                .iter()
                .map(|event| event.model.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-6.1-sol", "gpt-6-astra", "custom-model"]
        );
        Ok(())
    }

    #[test]
    fn test_parse_cumulative_tokens_valid() {
        let json: serde_json::Value = serde_json::json!({
            "input_tokens": 17934,
            "cached_input_tokens": 9600,
            "output_tokens": 454,
            "reasoning_output_tokens": 233,
            "total_tokens": 18388
        });
        let tokens = parse_cumulative_tokens(&json).unwrap();
        assert_eq!(tokens.input, 17934);
        assert_eq!(tokens.cached_input, 9600);
        assert_eq!(tokens.output, 454);
    }

    #[test]
    fn test_parse_cumulative_tokens_null() {
        let json = serde_json::Value::Null;
        assert!(parse_cumulative_tokens(&json).is_none());
    }

    #[test]
    fn test_parse_cumulative_tokens_rejects_empty_object_but_accepts_explicit_zero() {
        assert!(parse_cumulative_tokens(&serde_json::json!({})).is_none());

        let tokens = parse_cumulative_tokens(&serde_json::json!({ "input_tokens": 0 }))
            .expect("an explicit zero is valid usage");
        assert_eq!(tokens.input, 0);
        assert_eq!(tokens.cached_input, 0);
        assert_eq!(tokens.output, 0);
    }

    #[test]
    fn test_parse_cumulative_tokens_alt_field_names() {
        // 某些版本可能使用 cache_read_input_tokens 而非 cached_input_tokens
        let json: serde_json::Value = serde_json::json!({
            "input_tokens": 1000,
            "cache_read_input_tokens": 500,
            "output_tokens": 200
        });
        let tokens = parse_cumulative_tokens(&json).unwrap();
        assert_eq!(tokens.cached_input, 500);
    }

    #[test]
    fn test_collect_codex_session_files_nonexistent() {
        let files = collect_codex_session_files(Path::new("/nonexistent/path"));
        assert!(files.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_thread_spawn_parent_strips_replay_and_keeps_live_usage() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, None, Some(PARENT_ID), "2026-07-10T03:00:05Z"),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:06Z"),
                token_count_at(1_300, 1_050, 150, "2026-07-10T03:00:07Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (result.imported, result.skipped, result.deferred),
            (1, 1, false)
        );

        let conn = lock_conn!(db.conn);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_ID}:2")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (300, 150, 50));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_poisoned_replay_cache_does_not_deadlock_parented_sync() -> Result<(), AppError> {
        clear_codex_replay_caches();
        // 模拟持锁断言失败把缓存锁弄中毒。
        let _ = std::thread::spawn(|| {
            let _guard = replay_caches().lock().unwrap();
            panic!("poison the replay cache");
        })
        .join();
        assert!(replay_caches().is_poisoned());

        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, None, Some(PARENT_ID), "2026-07-10T03:00:05Z"),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:06Z"),
                token_count_at(1_300, 1_050, 150, "2026-07-10T03:00:07Z"),
            ],
        );

        // 放到子线程里跑：一旦回归成自锁，测试按超时失败而不是挂住整个测试进程。
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = sync_test_file(&db, &child, &[&parent, &child])
                .map(|result| (result.imported, result.skipped, result.deferred));
            let _ = tx.send(result);
            drop(temp);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("parented sync deadlocked on a poisoned replay cache")?;
        assert_eq!(result, (1, 1, false));

        clear_codex_replay_caches();
        assert!(!replay_caches().is_poisoned());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_filtered_parent_events_use_subsequence_prefix_alignment() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:02Z"),
                token_count_at(300, 150, 30, "2026-07-10T03:00:03Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:05Z"),
                token_count_at(100, 50, 10, "2026-07-10T03:00:06Z"),
                token_count_at(300, 150, 30, "2026-07-10T03:00:07Z"),
                token_count_at(450, 220, 45, "2026-07-10T03:00:08Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!((result.imported, result.skipped), (1, 2));
        Ok(())
    }

    /// 文件系统给不出文件身份时（如 Windows 访问 \\wsl.localhost）父时间线按设计不进缓存，
    /// 这类环境只校验结果、不校验缓存复用。
    fn parent_cache_supported(path: &Path) -> bool {
        let supported = ParentFileStamp::from_file(&fs::File::open(path).unwrap()).is_some();
        if !supported {
            eprintln!(
                "no file identity for {}; skipping parent cache assertions",
                path.display()
            );
        }
        supported
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_malformed_lines_defer_child_until_repaired() -> Result<(), AppError> {
        let cases: &[(&[u8], &[u8], bool)] = &[
            // A damaged parent must not lead to double-counting its replayed usage.
            // Once repaired, a complete final record without a newline is valid.
            (b"\xff\n", b"", true),
            (b"", b"\xff\n", false),
            (b"", b"\xff", false),
            // Malformed JSON must also fail closed; blank lines alone are harmless.
            (b"{malformed\n \t\r\n", b"{malformed", false),
        ];
        for compressed in [false, true] {
            for &(prefix, suffix, remove_final_newline) in cases {
                clear_codex_replay_caches();
                let db = Database::memory()?;
                let temp = tempdir().unwrap();
                let (parent, child) = retryable_parent_and_child(temp.path());
                append_jsonl(&parent, turn_context_at("2026-07-10T03:00:10Z"));
                let mut contents = fs::read(&parent).unwrap();
                if remove_final_newline {
                    assert_eq!(contents.pop(), Some(b'\n'));
                }
                fs::write(&parent, [prefix, contents.as_slice(), suffix].concat()).unwrap();
                let parent = if compressed {
                    compress_rollout(&parent)
                } else {
                    parent
                };

                let deferred = sync_test_file(&db, &child, &[&parent, &child])?;
                assert_eq!(
                    (deferred.imported, deferred.skipped, deferred.deferred),
                    (0, 0, true)
                );
                assert!(!load_parent_timeline(&parent).unwrap_err().cacheable);
                let repaired = if compressed {
                    zstd::stream::encode_all(contents.as_slice(), 0).unwrap()
                } else {
                    contents
                };
                fs::write(&parent, repaired).unwrap();
                let result = sync_test_file(&db, &child, &[&parent, &child])?;
                assert_eq!(
                    (result.imported, result.skipped, result.deferred),
                    (1, 1, false)
                );
                let timeline = load_parent_timeline(&parent).unwrap();
                assert_eq!(timeline.events.len(), 1);
                assert_eq!(
                    timeline.max_timestamp,
                    Some("2026-07-10T03:00:10Z".parse::<DateTime<Utc>>().unwrap())
                );
                for _ in 0..3 {
                    let warm = sync_test_file(&db, &child, &[&parent, &child])?;
                    assert_eq!((warm.imported, warm.deferred), (0, false));
                }
                assert_eq!(codex_parse_count(&child), 2);
                if parent_cache_supported(&parent) {
                    assert!(Arc::ptr_eq(
                        &timeline,
                        &load_parent_timeline(&parent).unwrap()
                    ));
                }
                let conn = lock_conn!(db.conn);
                let usage: (i64, i64, i64) = conn.query_row(
                    "SELECT input_tokens, cache_read_tokens, output_tokens FROM proxy_request_logs",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                assert_eq!(usage, (300, 150, 50));
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_rollout_is_cached_once_across_fork_cutoffs() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:10Z"),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );

        let early = "2026-07-10T03:00:05Z".parse::<DateTime<Utc>>().unwrap();
        let late = "2026-07-10T03:00:15Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(parent_signatures_before(&parent, early).unwrap().len(), 1);
        let first_timeline = parent_cache_supported(&parent).then(|| {
            Arc::clone(&replay_caches().lock().unwrap().parent_timelines[&parent].timeline)
        });
        assert_eq!(parent_signatures_before(&parent, late).unwrap().len(), 2);

        let caches = replay_caches().lock().unwrap();
        match first_timeline {
            Some(first_timeline) => {
                assert_eq!(caches.parent_timelines.len(), 1);
                assert!(Arc::ptr_eq(
                    &first_timeline,
                    &caches.parent_timelines[&parent].timeline
                ));
            }
            None => assert!(caches.parent_timelines.is_empty()),
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_rollout_cache_invalidates_after_append() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let cutoff = "2026-07-10T03:00:15Z".parse::<DateTime<Utc>>().unwrap();
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );
        assert_eq!(parent_signatures_before(&parent, cutoff).unwrap().len(), 1);

        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:10Z"),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );
        assert_eq!(parent_signatures_before(&parent, cutoff).unwrap().len(), 2);

        let expected_entries = usize::from(parent_cache_supported(&parent));
        assert_eq!(
            replay_caches().lock().unwrap().parent_timelines.len(),
            expected_entries
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_rollout_content_error_cache_preserves_open_errors() {
        clear_codex_replay_caches();
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let cutoff = "2026-07-10T03:00:05Z".parse::<DateTime<Utc>>().unwrap();
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_without_timestamp(100, 50, 10),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );

        let first_error = parent_signatures_before(&parent, cutoff).unwrap_err();
        assert!(first_error
            .reason
            .contains("token_count 缺少有效 timestamp"));
        assert!(first_error.cacheable);
        let cached_timeline =
            || Arc::clone(&replay_caches().lock().unwrap().parent_timelines[&parent].timeline);
        let first_timeline = parent_cache_supported(&parent).then(cached_timeline);

        let second_error = parent_signatures_before(&parent, cutoff).unwrap_err();
        assert_eq!(second_error, first_error);
        if let Some(first_timeline) = first_timeline {
            assert!(Arc::ptr_eq(&first_timeline, &cached_timeline()));
        }

        fs::remove_file(&parent).unwrap();
        let open_error = parent_signatures_before(&parent, cutoff).unwrap_err();
        assert!(open_error.reason.contains("无法打开父 rollout"));
        assert!(!open_error.cacheable);
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_rollout_nanosecond_cutoffs_are_exact() {
        clear_codex_replay_caches();
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:00.000000500Z"),
                turn_context_at("2026-07-10T03:00:00.000000900Z"),
            ],
        );

        let before = "2026-07-10T03:00:00.000000300Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let after = "2026-07-10T03:00:00.000000700Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        assert!(parent_signatures_before(&parent, before)
            .unwrap()
            .is_empty());
        assert_eq!(parent_signatures_before(&parent, after).unwrap().len(), 1);
        let expected_entries = usize::from(parent_cache_supported(&parent));
        assert_eq!(
            replay_caches().lock().unwrap().parent_timelines.len(),
            expected_entries
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn test_parent_file_stamp_distinguishes_same_size_same_mtime_files() {
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let replacement = temp.path().join("replacement.jsonl");
        let values = [session_meta(PARENT_ID), token_count(100, 50, 10)];
        write_jsonl(&parent, &values);
        write_jsonl(&replacement, &values);
        if !parent_cache_supported(&parent) {
            return;
        }
        let original_file = fs::File::open(&parent).unwrap();
        let original_metadata = original_file.metadata().unwrap();
        let replacement_file = fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap();
        replacement_file
            .set_times(fs::FileTimes::new().set_modified(original_metadata.modified().unwrap()))
            .unwrap();
        let original_stamp = ParentFileStamp::from_file(&original_file).unwrap();
        let replacement_stamp = ParentFileStamp::from_file(&replacement_file).unwrap();
        assert_eq!(
            (original_stamp.size, original_stamp.modified_nanos),
            (replacement_stamp.size, replacement_stamp.modified_nanos)
        );
        assert_ne!(original_stamp, replacement_stamp);
    }

    #[test]
    #[serial_test::serial]
    fn test_empty_fork_imports_no_parent_usage() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:02Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:05Z"),
                token_count_at(100, 50, 10, "2026-07-10T03:00:06Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:07Z"),
                serde_json::json!({
                    "timestamp": "2026-07-10T03:00:08Z",
                    "type": "event_msg",
                    "payload": { "type": "thread_settings_applied" }
                }),
            ],
        );

        let result = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (result.imported, result.skipped, result.deferred),
            (0, 2, false)
        );
        let conn = lock_conn!(db.conn);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'codex_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_conflicting_explicit_parents_are_deferred() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &child,
            &[
                session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    Some(CHILD_B_ID),
                    "2026-07-10T03:00:05Z",
                ),
                token_count_at(100, 50, 10, "2026-07-10T03:00:06Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&child])?;
        assert!(result.deferred);
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_future_signature_cannot_extend_replay_prefix() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:06Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:05Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:07Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (result.imported, result.skipped, result.deferred),
            (1, 0, false)
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_duplicate_parent_replica_with_missing_timestamp_rejects_both_orders(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let sessions_parent = temp
            .path()
            .join("sessions")
            .join(format!("rollout-2026-07-10T03-00-01-{PARENT_ID}.jsonl"));
        let archived_parent = temp
            .path()
            .join("archived_sessions")
            .join(format!("rollout-2026-07-10T03-00-01-{PARENT_ID}.jsonl"));
        fs::create_dir_all(sessions_parent.parent().unwrap()).unwrap();
        fs::create_dir_all(archived_parent.parent().unwrap()).unwrap();

        write_jsonl(
            &archived_parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        write_jsonl(
            &sessions_parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                token_count(500, 400, 50),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );

        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, None, Some(PARENT_ID), "2026-07-10T03:00:05Z"),
                token_count_at(1_500, 1_000, 200, "2026-07-10T03:00:06Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&archived_parent, &sessions_parent, &child])?;
        assert!(
            result.deferred,
            "任一副本含有无效时间戳时，必须保留无效时间戳保护并 deferred"
        );

        let result_swapped =
            sync_test_file(&db, &child, &[&sessions_parent, &archived_parent, &child])?;
        assert!(
            result_swapped.deferred,
            "候选顺序不应改变无效时间戳的防御结果"
        );

        Ok(())
    }

    /// paginated 线程翻页后，续写页是 `<threadId>_<rolloutId>` 双段文件名，
    /// 首页从此不再增长。之后 spawn 的子代理 fork 时刻晚于首页最后事件，
    /// 父时间线必须把首页与续写页拼起来看，否则子代理会被永久 deferred。
    #[test]
    #[serial_test::serial]
    fn test_paginated_parent_pages_are_merged_before_fork_cutoff() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent_root = rollout_path(temp.path(), PARENT_ID);
        let parent_page = temp.path().join(format!(
            "rollout-2026-07-10T03-00-20-{PARENT_ID}_{PARENT_PAGE_ID}.jsonl"
        ));
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &parent_root,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
            ],
        );
        write_jsonl(
            &parent_page,
            &[
                session_meta_at(PARENT_ID, None, None, "2026-07-10T03:00:20Z"),
                token_count_at(2_000, 1_800, 200, "2026-07-10T03:00:21Z"),
                turn_context_at("2026-07-10T03:00:40Z"),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    Some(PARENT_ID),
                    "2026-07-10T03:00:30Z",
                ),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:31Z"),
                token_count_at(2_000, 1_800, 200, "2026-07-10T03:00:32Z"),
                token_count_at(2_300, 1_950, 250, "2026-07-10T03:00:33Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&parent_root, &parent_page, &child])?;
        assert_eq!(
            (result.imported, result.skipped, result.deferred),
            (1, 2, false)
        );

        let conn = lock_conn!(db.conn);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_ID}:3")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (300, 150, 50));
        Ok(())
    }

    /// #8161: 分支线程翻页产生的续写页（`<branchId>_<pageId>` 双段文件名），
    /// 其 session_meta 仍保留所属分支会话的 forked_from_id，但翻页时间晚于原始父线程末尾。
    /// 续写页应被识别为分支线程自身的延续，不剔除父重放（新格式续写页不复制父用量），
    /// 避免因父线程无法达到翻页时刻而被永久 deferred。
    #[test]
    #[serial_test::serial]
    fn test_paginated_continuation_pages_for_forked_branch_thread_are_imported(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        // 1. 父线程在 03:00:20 结束
        let parent_root = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &parent_root,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );

        // 2. 分支线程 CHILD_A 首页在 03:00:15 fork 自 PARENT_ID
        // 首页重放了父线程的 1000 tokens，并新产生了 500 tokens
        let branch_root = temp
            .path()
            .join(format!("rollout-2026-07-10T03-00-15-{CHILD_A_ID}.jsonl"));
        write_jsonl(
            &branch_root,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:15Z"),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:16Z"),
                token_count_at(1_500, 1_300, 150, "2026-07-10T03:00:20Z"),
            ],
        );

        // 3. 分支线程 CHILD_A 翻页，产生双段续写页，时间戳为 03:01:00（远晚于父线程末尾 03:00:10）
        // session_meta 仍带 forked_from_id: PARENT_ID，新产生 200 tokens
        let branch_page = temp.path().join(format!(
            "rollout-2026-07-10T03-01-00-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_ID),
                    "2026-07-10T03:01:00Z",
                ),
                turn_context_at("2026-07-10T03:01:01Z"),
                token_count_at(200, 150, 50, "2026-07-10T03:01:05Z"),
            ],
        );

        let files = [
            parent_root.as_path(),
            branch_root.as_path(),
            branch_page.as_path(),
        ];

        // 同步分支线程首页：成功导入 1 条（500 tokens），跳过 1 条重放（1000 tokens）
        let root_result = sync_test_file(&db, &branch_root, &files)?;
        assert_eq!(
            (
                root_result.imported,
                root_result.skipped,
                root_result.deferred
            ),
            (1, 1, false)
        );

        // 同步分支线程续写页：识别为自身延续，直接导入 1 条（200 tokens），绝不 deferred！
        let page_result = sync_test_file(&db, &branch_page, &files)?;
        assert_eq!(
            (
                page_result.imported,
                page_result.skipped,
                page_result.deferred
            ),
            (1, 0, false)
        );

        // 验证数据库：session_id 统一为 CHILD_A_ID，且两条记录用量准确
        let conn = lock_conn!(db.conn);
        let mut stmt = conn.prepare(
            "SELECT request_id, session_id, input_tokens, output_tokens
             FROM proxy_request_logs
             WHERE session_id = ?1
             ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([CHILD_A_ID], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(rows.len(), 2);
        // 首页记录
        assert_eq!(
            rows[0].0,
            format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_ID}:2")
        );
        assert_eq!(rows[0].1, CHILD_A_ID);
        assert_eq!(rows[0].2, 500); // 1500 - 1000
        assert_eq!(rows[0].3, 50); // 150 - 100
                                   // 续写页记录
        assert_eq!(
            rows[1].0,
            format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_PAGE_ID}:1")
        );
        assert_eq!(rows[1].1, CHILD_A_ID);
        assert_eq!(rows[1].2, 200); // 1700 - 1500
        assert_eq!(rows[1].3, 50); // 200 - 150

        Ok(())
    }

    /// 测试分支线程多次翻页（第 3 页续写页，其 history_base 指向上一页 rollout ID），
    /// 以及在分支线程多页续写之后再派生子代理的完整链条。
    #[test]
    #[serial_test::serial]
    fn test_paginated_continuation_multiple_pages_and_subagent_on_branch() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        let parent_root = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &parent_root,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:20Z"),
            ],
        );

        // 分支线程 CHILD_A 首页
        let branch_root = temp
            .path()
            .join(format!("rollout-2026-07-10T03-00-15-{CHILD_A_ID}.jsonl"));
        write_jsonl(
            &branch_root,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:15Z"),
                turn_context(),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:16Z"),
                token_count_at(1_500, 1_300, 150, "2026-07-10T03:00:20Z"),
            ],
        );

        // 分支线程第 2 页（续写页 1）
        let branch_page_1 = temp.path().join(format!(
            "rollout-2026-07-10T03-01-00-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page_1,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_ID),
                    "2026-07-10T03:01:00Z",
                ),
                turn_context_at("2026-07-10T03:01:01Z"),
                token_count_at(1_700, 1_450, 200, "2026-07-10T03:01:05Z"),
            ],
        );

        // 分支线程第 3 页（续写页 2），history_base 指向第 2 页的 rollout ID
        let branch_page_2 = temp.path().join(format!(
            "rollout-2026-07-10T03-02-00-{CHILD_A_ID}_{CHILD_A_PAGE_2_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page_2,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_PAGE_ID),
                    "2026-07-10T03:02:00Z",
                ),
                turn_context_at("2026-07-10T03:02:01Z"),
                token_count_at(2_000, 1_650, 250, "2026-07-10T03:02:10Z"),
                turn_context_at("2026-07-10T03:02:30Z"),
            ],
        );

        // 在分支线程翻页写完后，从分支线程派生子代理 CHILD_B（fork 时刻 03:02:30）
        let subagent = temp
            .path()
            .join(format!("rollout-2026-07-10T03-02-30-{CHILD_B_ID}.jsonl"));
        write_jsonl(
            &subagent,
            &[
                session_meta_at(
                    CHILD_B_ID,
                    Some(CHILD_A_ID),
                    Some(CHILD_A_ID),
                    "2026-07-10T03:02:30Z",
                ),
                turn_context(),
                // 重放 CHILD_A 从头到尾（包括多页）的 token
                token_count_at(1_000, 900, 100, "2026-07-10T03:02:31Z"),
                token_count_at(1_500, 1_300, 150, "2026-07-10T03:02:32Z"),
                token_count_at(1_700, 1_450, 200, "2026-07-10T03:02:33Z"),
                token_count_at(2_000, 1_650, 250, "2026-07-10T03:02:34Z"),
                // 子代理自己消耗 150 tokens
                token_count_at(2_150, 1_750, 280, "2026-07-10T03:02:40Z"),
            ],
        );

        let files = [
            parent_root.as_path(),
            branch_root.as_path(),
            branch_page_1.as_path(),
            branch_page_2.as_path(),
            subagent.as_path(),
        ];

        // 先同步分支线程首页：成功导入 500 tokens（跳过父 1000 重放）
        let r_res = sync_test_file(&db, &branch_root, &files)?;
        assert_eq!(
            (r_res.imported, r_res.skipped, r_res.deferred),
            (1, 1, false)
        );

        // 验证续写页 1 成功导入：基线继承首页 1500，增量仅为 200（1700 - 1500）
        let p1_res = sync_test_file(&db, &branch_page_1, &files)?;
        assert_eq!((p1_res.imported, p1_res.deferred), (1, false));

        // 验证续写页 2 成功导入：基线继承第 2 页 1700，增量仅为 300（2000 - 1700）
        let p2_res = sync_test_file(&db, &branch_page_2, &files)?;
        assert_eq!((p2_res.imported, p2_res.deferred), (1, false));

        // 验证子代理解析父线程 CHILD_A 的时间线时，能合并 CHILD_A 的首页 + 两页续写页，
        // 成功达到 03:02:30 并剔除全部 4 条重放，导入子代理自身的 1 条用量（150 tokens）
        let sub_res = sync_test_file(&db, &subagent, &files)?;
        assert_eq!(
            (sub_res.imported, sub_res.skipped, sub_res.deferred),
            (1, 4, false)
        );

        let conn = lock_conn!(db.conn);
        // 精确断言续写页 1 的入库用量（防止将 1700 历史累计值算为增量）
        let p1_usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!(
                "{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_PAGE_ID}:1"
            )],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(p1_usage, (200, 150, 50));

        // 精确断言续写页 2 的入库用量（防止将 2000 历史累计值算为增量）
        let p2_usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!(
                "{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_PAGE_2_ID}:1"
            )],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(p2_usage, (300, 200, 50));

        // 断言子代理用量
        let sub_usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_B_ID}:5")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(sub_usage, (150, 100, 30));

        // 精确断言分支线程 CHILD_A 的全生命周期总 input 用量：
        // 首页 500 + 第 2 页 200 + 第 3 页 300 = 1000 tokens
        let branch_total_input: i64 = conn.query_row(
            "SELECT sum(input_tokens) FROM proxy_request_logs WHERE session_id = ?1",
            [CHILD_A_ID],
            |row| row.get(0),
        )?;
        assert_eq!(branch_total_input, 1_000);

        Ok(())
    }

    /// 确保防御性：单段文件名的新 fork 首页即使其 history_base 指向父会话某页，
    /// 也不会被误判为延续页，父线程未达到 cutoff 时依然必须被 deferred。
    #[test]
    #[serial_test::serial]
    fn test_single_segment_fork_is_not_treated_as_continuation() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        let parent_root = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &parent_root,
            &[
                session_meta(PARENT_ID),
                token_count_at(1_000, 900, 100, "2026-07-10T03:00:01Z"),
                // 父线程在 03:00:05 结束
                turn_context_at("2026-07-10T03:00:05Z"),
            ],
        );

        // 子会话首页 fork 时刻是 03:00:20（晚于父线程结束时间 03:00:05）
        // 单段文件名，history_base 指向 PARENT_ID（父会话某页）
        let child = temp
            .path()
            .join(format!("rollout-2026-07-10T03-00-20-{CHILD_A_ID}.jsonl"));
        write_jsonl(
            &child,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(PARENT_ID),
                    "2026-07-10T03:00:20Z",
                ),
                turn_context_at("2026-07-10T03:00:21Z"),
                token_count_at(1_200, 1_000, 120, "2026-07-10T03:00:25Z"),
            ],
        );

        let files = [parent_root.as_path(), child.as_path()];
        let result = sync_test_file(&db, &child, &files)?;
        assert!(
            result.deferred,
            "单段文件名的新 fork 首页在父线程未达到 cutoff 时必须被 deferred"
        );

        Ok(())
    }

    /// 对照测试 1：带有有效 last_token_usage 的续写页直接使用 last 计费，无需回溯基线。
    #[test]
    #[serial_test::serial]
    fn test_paginated_continuation_with_valid_last_token_usage() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        let branch_page = temp.path().join(format!(
            "rollout-2026-07-10T03-01-00-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_ID),
                    "2026-07-10T03:01:00Z",
                ),
                turn_context_at("2026-07-10T03:01:01Z"),
                // 带有显式的 last_token_usage
                token_count_with_last_at(
                    1_700,
                    1_450,
                    200,
                    120,
                    80,
                    30,
                    "codex",
                    "2026-07-10T03:01:05Z",
                ),
            ],
        );

        let files = [branch_page.as_path()];
        let result = sync_test_file(&db, &branch_page, &files)?;
        assert_eq!((result.imported, result.deferred), (1, false));

        let conn = lock_conn!(db.conn);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!(
                "{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_PAGE_ID}:1"
            )],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (120, 80, 30));
        Ok(())
    }

    /// 对照测试 2：防御机制——当续写页缺少 last_token_usage 且上一分段缺失无法恢复基线时，
    /// 必须安全 deferred，绝对不能将累计总值直接当作增量入库。
    #[test]
    #[serial_test::serial]
    fn test_paginated_continuation_deferred_when_prev_segment_missing() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        // 仅存在续写页，其前序首页文件缺失
        let branch_page = temp.path().join(format!(
            "rollout-2026-07-10T03-01-00-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    Some(PARENT_ID),
                    None,
                    Some(CHILD_A_ID),
                    "2026-07-10T03:01:00Z",
                ),
                turn_context_at("2026-07-10T03:01:01Z"),
                token_count_at(1_700, 1_450, 200, "2026-07-10T03:01:05Z"),
            ],
        );

        let files = [branch_page.as_path()];
        let result = sync_test_file(&db, &branch_page, &files)?;
        assert!(
            result.deferred,
            "前序分段缺失且无 last_token_usage 时必须 deferred"
        );

        // 验证数据库中绝无错误写入
        let count: i64 = {
            let conn = lock_conn!(db.conn);
            conn.query_row(
                "SELECT count(*) FROM proxy_request_logs WHERE session_id = ?1",
                [CHILD_A_ID],
                |row| row.get(0),
            )?
        };
        assert_eq!(count, 0, "缺失前序基线时绝不应有数据入库");

        // 验证游标未推进
        let sync_state = get_sync_state(&db, &branch_page.to_string_lossy())?;
        assert_eq!(sync_state, (0, 0), "deferred 时游标不应推进");
        Ok(())
    }

    /// 对照测试 3：续写页计数器重置场景——新页 total_token_usage 从较小数值起算，
    /// 自动识别为计数器重置，以当前值作为增量计费，未被饱和减法吞为 0。
    #[test]
    #[serial_test::serial]
    fn test_paginated_continuation_with_counter_reset() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();

        // 首页结束于累计 1500
        let branch_root = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &branch_root,
            &[
                session_meta(CHILD_A_ID),
                token_count_at(1_500, 1_300, 150, "2026-07-10T03:00:20Z"),
            ],
        );

        // 续写页计数器重置为 180 (180 < 1500)
        let branch_page = temp.path().join(format!(
            "rollout-2026-07-10T03-01-00-{CHILD_A_ID}_{CHILD_A_PAGE_ID}.jsonl"
        ));
        write_jsonl(
            &branch_page,
            &[
                paginated_session_meta_at(
                    CHILD_A_ID,
                    None,
                    None,
                    Some(CHILD_A_ID),
                    "2026-07-10T03:01:00Z",
                ),
                turn_context_at("2026-07-10T03:01:01Z"),
                token_count_at(180, 100, 25, "2026-07-10T03:01:05Z"),
            ],
        );

        let files = [branch_root.as_path(), branch_page.as_path()];
        let root_res = sync_test_file(&db, &branch_root, &files)?;
        assert_eq!((root_res.imported, root_res.deferred), (1, false));

        let page_res = sync_test_file(&db, &branch_page, &files)?;
        assert_eq!((page_res.imported, page_res.deferred), (1, false));

        let conn = lock_conn!(db.conn);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs WHERE request_id = ?1",
            [format!(
                "{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_PAGE_ID}:1"
            )],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (180, 100, 25), "计数器重置时应以新页数值作为增量");
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_missing_parent_is_deferred_and_recovered_without_child_change() -> Result<(), AppError>
    {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, None, Some(PARENT_ID), "2026-07-10T03:00:05Z"),
                token_count_at(900, 400, 90, "2026-07-10T03:00:06Z"),
            ],
        );

        let deferred = sync_test_file(&db, &child, &[&child])?;
        assert!(deferred.deferred);
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));

        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
            ],
        );
        let recovered = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!((recovered.imported, recovered.deferred), (1, false));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_skips_child_body_until_parent_append() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        let cached = parent_cache_supported(&parent);

        for _ in 0..4 {
            assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        }
        if cached {
            assert_eq!(codex_parse_count(&child), 1);
        }
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));

        append_jsonl(&parent, turn_context_at("2026-07-10T03:00:10Z"));
        let recovered = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (1, 1, false)
        );
        if cached {
            assert_eq!(codex_parse_count(&child), 2);
        }
        assert_eq!(sync_test_file(&db, &child, &[&parent, &child])?.imported, 0);
        if cached {
            assert_eq!(codex_parse_count(&child), 2);
        }
        let conn = lock_conn!(db.conn);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens FROM proxy_request_logs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (300, 150, 50));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_invalidates_when_child_changes() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        let cached = parent_cache_supported(&parent);
        assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        append_jsonl(
            &child,
            token_count_at(1_600, 1_200, 200, "2026-07-10T03:00:08Z"),
        );
        for _ in 0..3 {
            assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        }
        if cached {
            assert_eq!(codex_parse_count(&child), 2);
        }
        append_jsonl(&parent, turn_context_at("2026-07-10T03:00:10Z"));
        let recovered = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (2, 1, false)
        );
        if cached {
            assert_eq!(codex_parse_count(&child), 3);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_tracks_added_and_removed_parent_candidates() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        let cached = parent_cache_supported(&parent);
        assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        let archived_dir = temp.path().join("archived_sessions");
        fs::create_dir(&archived_dir).unwrap();
        let archived_parent = rollout_path(&archived_dir, PARENT_ID);
        fs::copy(&parent, &archived_parent).unwrap();
        append_jsonl(&archived_parent, turn_context_at("2026-07-10T03:00:10Z"));
        assert!(sync_test_file(&db, &child, &[&parent, &archived_parent, &child])?.deferred);
        if cached {
            assert_eq!(codex_parse_count(&child), 2, "new candidate must retry");
        }
        assert!(sync_test_file(&db, &child, &[&parent, &archived_parent, &child])?.deferred);
        if cached {
            assert_eq!(codex_parse_count(&child), 2);
        }
        fs::remove_file(&parent).unwrap();
        let recovered = sync_test_file(&db, &child, &[&archived_parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (1, 1, false)
        );
        if cached {
            assert_eq!(codex_parse_count(&child), 3, "removed candidate must retry");
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_does_not_cache_unknown_parent_metadata() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        let parent_contents = fs::read(&parent).unwrap();
        fs::remove_file(&parent).unwrap();
        // The index can outlive a file removed during a pass. An inaccessible
        // candidate must retry, rather than cache an unknown stamp forever.
        for _ in 0..2 {
            assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        }
        assert_eq!(codex_parse_count(&child), 2);
        fs::write(&parent, parent_contents).unwrap();
        append_jsonl(&parent, turn_context_at("2026-07-10T03:00:10Z"));
        assert!(!sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        assert_eq!(codex_parse_count(&child), 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_detects_same_size_same_mtime_parent_replacement(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        if !parent_cache_supported(&parent) {
            return Ok(());
        }
        assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        let metadata = fs::metadata(&parent).unwrap();
        let replacement = temp.path().join("replacement.jsonl");
        let contents = fs::read_to_string(&parent)
            .unwrap()
            .replace("03:00:02Z", "03:00:10Z");
        fs::write(&replacement, contents).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        fs::rename(&replacement, &parent).unwrap();
        let replacement_metadata = fs::metadata(&parent).unwrap();
        assert_eq!(metadata.len(), replacement_metadata.len());
        assert_eq!(
            metadata.modified().unwrap(),
            replacement_metadata.modified().unwrap()
        );
        let recovered = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (1, 1, false)
        );
        assert_eq!(codex_parse_count(&child), 2);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_retries_parent_decompression_errors() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        let compressed_parent = PathBuf::from(format!("{}.zst", parent.display()));
        fs::remove_file(&parent).unwrap();
        fs::write(&compressed_parent, b"invalid zstd bytes").unwrap();
        // Opening/statting this file succeeds; decoding its body fails. That
        // transient failure must not be suppressed by an unchanged fingerprint.
        for _ in 0..2 {
            assert!(sync_test_file(&db, &child, &[&compressed_parent, &child])?.deferred);
        }
        assert_eq!(codex_parse_count(&child), 2);
        assert!(!replay_caches()
            .lock()
            .unwrap()
            .parent_timelines
            .contains_key(&compressed_parent));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_parent_decompression_error_after_valid_prefix_does_not_cache_partial_timeline(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        append_jsonl(&parent, turn_context_at("2026-07-10T03:00:10Z"));
        let mut contents = fs::read(&parent).unwrap();
        // Fill the rollout reader's buffer before it reaches a corrupt second
        // frame, so valid token events are read before decompression fails.
        contents.extend(vec![b'\n'; 2 << 20]);
        fs::write(&parent, contents).unwrap();
        let compressed_parent = compress_rollout(&parent);
        let valid_frame = fs::read(&compressed_parent).unwrap();
        let mut corrupt_frames = valid_frame.clone();
        corrupt_frames.extend_from_slice(b"invalid zstd bytes");
        fs::write(&compressed_parent, corrupt_frames).unwrap();
        let mut reader = codex_rollout_file::open_reader(&compressed_parent).unwrap();
        let mut first_line = Vec::new();
        assert!(reader.read_until(b'\n', &mut first_line).unwrap() > 0);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first_line).unwrap(),
            session_meta(PARENT_ID)
        );
        drop(reader);

        for _ in 0..2 {
            let error = load_parent_timeline(&compressed_parent).unwrap_err();
            assert!(!error.cacheable);
            assert!(error.reason.contains("无法读取父 rollout"));
            assert!(sync_test_file(&db, &child, &[&compressed_parent, &child])?.deferred);
            assert!(!replay_caches()
                .lock()
                .unwrap()
                .parent_timelines
                .contains_key(&compressed_parent));
        }
        assert_eq!(codex_parse_count(&child), 2);
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));

        fs::write(&compressed_parent, valid_frame).unwrap();
        let recovered = sync_test_file(&db, &child, &[&compressed_parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (1, 1, false)
        );
        assert_eq!(codex_parse_count(&child), 3);
        assert_eq!(
            sync_test_file(&db, &child, &[&compressed_parent, &child])?.imported,
            0
        );
        assert_eq!(codex_parse_count(&child), 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_retryable_pending_detects_same_size_same_mtime_child_replacement(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let (parent, child) = retryable_parent_and_child(temp.path());
        if !parent_cache_supported(&child) {
            return Ok(());
        }
        assert!(sync_test_file(&db, &child, &[&parent, &child])?.deferred);
        let alternate_parent = rollout_path(temp.path(), CHILD_B_ID);
        fs::write(
            &alternate_parent,
            fs::read_to_string(&parent)
                .unwrap()
                .replace(PARENT_ID, CHILD_B_ID),
        )
        .unwrap();
        append_jsonl(&alternate_parent, turn_context_at("2026-07-10T03:00:10Z"));

        let metadata = fs::metadata(&child).unwrap();
        let replacement = temp.path().join("replacement.jsonl");
        fs::write(
            &replacement,
            fs::read_to_string(&child)
                .unwrap()
                .replace(PARENT_ID, CHILD_B_ID),
        )
        .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        fs::rename(&replacement, &child).unwrap();
        let replacement_metadata = fs::metadata(&child).unwrap();
        assert_eq!(metadata.len(), replacement_metadata.len());
        assert_eq!(
            metadata.modified().unwrap(),
            replacement_metadata.modified().unwrap()
        );
        let recovered = sync_test_file(&db, &child, &[&parent, &alternate_parent, &child])?;
        assert_eq!(
            (recovered.imported, recovered.skipped, recovered.deferred),
            (1, 1, false)
        );
        assert_eq!(codex_parse_count(&child), 2);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_billable_file_without_meta_is_deferred_without_cursor() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(&child, &[turn_context(), token_count(100, 50, 10)]);

        let result = sync_test_file(&db, &child, &[&child])?;
        assert!(result.deferred);
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));

        std::thread::sleep(std::time::Duration::from_millis(2));
        write_jsonl(
            &child,
            &[
                turn_context(),
                token_count(100, 50, 10),
                session_meta_at(CHILD_A_ID, None, None, "2026-07-10T03:00:03Z"),
            ],
        );
        let recovered = sync_test_file(&db, &child, &[&child])?;
        assert_eq!((recovered.imported, recovered.deferred), (1, false));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_non_billable_file_without_meta_advances_cursor() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let child = rollout_path(temp.path(), CHILD_A_ID);
        write_jsonl(
            &child,
            &[
                turn_context(),
                token_count_at(0, 0, 0, "2026-07-10T03:00:02Z"),
            ],
        );

        let result = sync_test_file(&db, &child, &[&child])?;
        assert!(!result.deferred);
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?.1, 2);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_subagents_use_filename_thread_ids() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let child_a = rollout_path(temp.path(), CHILD_A_ID);
        let child_b = rollout_path(temp.path(), CHILD_B_ID);
        write_jsonl(
            &child_a,
            &[
                session_meta(CHILD_A_ID),
                turn_context(),
                token_count(100, 50, 10),
            ],
        );
        write_jsonl(
            &child_b,
            &[
                session_meta(CHILD_B_ID),
                turn_context(),
                token_count(200, 100, 20),
            ],
        );

        assert_eq!(
            sync_test_file(&db, &child_a, &[&child_a, &child_b])?.imported,
            1
        );
        assert_eq!(
            sync_test_file(&db, &child_b, &[&child_a, &child_b])?.imported,
            1
        );

        let conn = lock_conn!(db.conn);
        let request_ids = conn
            .prepare(
                "SELECT request_id FROM proxy_request_logs
                 WHERE data_source = 'codex_session' ORDER BY request_id",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(
            request_ids,
            vec![
                format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_A_ID}:1"),
                format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{CHILD_B_ID}:1")
            ]
        );
        Ok(())
    }

    #[test]
    fn test_archived_log_inherits_cursor_and_only_imports_appended_usage() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&archived).unwrap();
        let source = rollout_path(&sessions, PARENT_ID);
        let archived_file = rollout_path(&archived, PARENT_ID);
        write_jsonl(
            &archived_file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(100, 50, 10),
                token_count(200, 100, 20),
            ],
        );

        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, request_model,
                    input_tokens, output_tokens, cache_read_tokens,
                    total_cost_usd, latency_ms, status_code, session_id,
                    created_at, data_source
                ) VALUES ('codex_session:parent:2', '_codex_session', 'codex',
                          'gpt-5.6-sol', 'gpt-5.6-sol', 999, 99, 0, '0', 0,
                          200, 'parent', 1, 'codex_session')",
                [],
            )?;
        }
        let source_path = source.to_string_lossy().to_string();
        update_sync_state(&db, &source_path, 1, 3)?;

        assert_eq!(
            sync_test_file(&db, &archived_file, &[&archived_file])?.imported,
            1
        );
        assert_eq!(
            sync_test_file(&db, &archived_file, &[&archived_file])?.imported,
            0
        );

        let conn = lock_conn!(db.conn);
        let old_row_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs
             WHERE request_id = 'codex_session:parent:2'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(old_row_count, 1);
        let usage: (i64, i64, i64) = conn.query_row(
            "SELECT input_tokens, cache_read_tokens, output_tokens
             FROM proxy_request_logs
             WHERE request_id = ?1",
            [format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{PARENT_ID}:2")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(usage, (100, 50, 10));
        drop(conn);
        assert_eq!(get_sync_state(&db, &archived_file.to_string_lossy())?.1, 4);

        Ok(())
    }

    fn compress_rollout(plain: &Path) -> PathBuf {
        let compressed = PathBuf::from(format!("{}.zst", plain.display()));
        let bytes = fs::read(plain).unwrap();
        fs::write(
            &compressed,
            zstd::stream::encode_all(bytes.as_slice(), 3).unwrap(),
        )
        .unwrap();
        fs::remove_file(plain).unwrap();
        compressed
    }

    fn decompress_rollout(compressed: &Path) -> PathBuf {
        let plain = codex_rollout_file::logical_path(compressed);
        let text = codex_rollout_file::read_to_string(compressed).unwrap();
        fs::write(&plain, text).unwrap();
        fs::remove_file(compressed).unwrap();
        plain
    }

    fn prune_detail_rows(db: &Database) -> Result<(), AppError> {
        let conn = lock_conn!(db.conn);
        conn.execute("DELETE FROM proxy_request_logs", [])?;
        Ok(())
    }

    /// Codex 压缩冷 rollout、恢复对话时再解压回来：两次换路径都靠游标继承防重复，
    /// 中间把明细删掉（模拟 30 天裁剪），request_id 去重兜不住。
    #[test]
    fn test_compressed_rollout_round_trip_never_double_counts() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let plain = rollout_path(&sessions, PARENT_ID);
        let mut lines = vec![
            session_meta(PARENT_ID),
            turn_context(),
            token_count(100, 50, 10),
        ];
        write_jsonl(&plain, &lines);
        assert_eq!(sync_test_file(&db, &plain, &[&plain])?.imported, 1);

        // CC Switch 没赶上第二条用量，文件就被压缩了
        lines.push(token_count(200, 100, 20));
        write_jsonl(&plain, &lines);
        let compressed = compress_rollout(&plain);
        prune_detail_rows(&db)?;
        assert_eq!(
            sync_test_file(&db, &compressed, &[&compressed])?.imported,
            1
        );
        assert_unchanged_codex_file_is_skipped(&db, &compressed)?;

        // 恢复对话：解压回普通形态并追加；普通形态的旧游标落后于压缩形态
        prune_detail_rows(&db)?;
        let plain = decompress_rollout(&compressed);
        lines.push(token_count(300, 150, 30));
        write_jsonl(&plain, &lines);
        assert_eq!(sync_test_file(&db, &plain, &[&plain])?.imported, 1);

        let conn = lock_conn!(db.conn);
        let request_ids: Vec<String> = conn
            .prepare("SELECT request_id FROM proxy_request_logs")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(
            request_ids,
            vec![format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{PARENT_ID}:3")]
        );
        drop(conn);
        assert_eq!(get_sync_state(&db, &plain.to_string_lossy())?.1, 5);
        Ok(())
    }

    #[test]
    fn test_compressed_rollout_filename_parsing() {
        let plain = PathBuf::from(format!("/s/rollout-2026-07-10T03-00-00-{PARENT_ID}.jsonl"));
        let compressed = PathBuf::from(format!("{}.zst", plain.display()));
        assert_eq!(
            thread_id_from_filename(&compressed),
            thread_id_from_filename(&plain)
        );
        assert!(thread_id_from_filename(&compressed).is_some());
        assert!(is_rollout_filename(
            compressed.file_name().unwrap().to_str().unwrap()
        ));

        let two_segment = PathBuf::from(format!(
            "/s/rollout-2026-08-26T17-18-13-{PARENT_ID}_{CHILD_A_ID}.jsonl.zst"
        ));
        assert_eq!(
            leading_thread_id_from_filename(&two_segment).as_deref(),
            Some(PARENT_ID)
        );
        assert_eq!(
            thread_id_from_filename(&two_segment).as_deref(),
            Some(CHILD_A_ID)
        );
    }

    #[test]
    fn test_insert_codex_session_skips_matching_proxy_log() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, request_model,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                    total_cost_usd, latency_ms, status_code, created_at, data_source
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    "codex-proxy",
                    "openai",
                    "codex",
                    "gpt-5.4",
                    "gpt-5.4",
                    10,
                    2,
                    1,
                    7,
                    "0.01",
                    100,
                    200,
                    1000,
                    "proxy"
                ],
            )?;
        }

        // Older rollouts carry no cache-write count; newer ones report the same
        // count the proxy logged. Either way the session row is the proxied request.
        for (request_id, cache_write_input) in
            [("codex-session-dup", 0), ("codex-session-dup-write", 7)]
        {
            let delta = DeltaTokens {
                input: 10,
                cached_input: 1,
                cache_write_input,
                cache_write_reported: cache_write_input > 0,
                output: 2,
            };
            let mut suspected_duplicates = 0;
            let inserted = insert_codex_session_entry(
                &db,
                request_id,
                &delta,
                "gpt-5.4",
                CodexServiceTier::Unknown,
                Some("session-1"),
                Some("1970-01-01T00:16:45Z"),
                &mut suspected_duplicates,
            )?;
            assert!(!inserted, "cache_write_input={cache_write_input}");
        }

        let conn = lock_conn!(db.conn);
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| {
            row.get(0)
        })?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_fast_bills_cache_write_tokens_as_ordinary_subscription_input() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_with_tier_at(
                    Some("priority"),
                    "gpt-5.6-sol",
                    "2026-07-10T03:00:01Z",
                ),
                token_count_with_cache_write_at(
                    1_000_000,
                    100_000,
                    100_000,
                    10_000,
                    "2026-07-10T03:00:02Z",
                ),
            ],
        );

        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        let conn = lock_conn!(db.conn);
        let row: (i64, i64, String, String, String, String, String, String) = conn.query_row(
            "SELECT cache_creation_tokens, input_token_semantics, pricing_model,
                    cost_multiplier, input_cost_usd, output_cost_usd,
                    cache_read_cost_usd, cache_creation_cost_usd
             FROM proxy_request_logs
             WHERE data_source = 'codex_session'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        assert_eq!(row.0, 100_000);
        assert_eq!(row.1, INPUT_TOKEN_SEMANTICS_TOTAL);
        assert_eq!(row.2, "gpt-5.6-sol");
        assert_eq!(row.3.parse::<Decimal>().unwrap(), Decimal::new(25, 1));
        assert_eq!(row.4.parse::<Decimal>().unwrap(), Decimal::new(32, 1));
        assert_eq!(row.5.parse::<Decimal>().unwrap(), Decimal::new(2, 1));
        assert_eq!(row.6.parse::<Decimal>().unwrap(), Decimal::new(4, 2));
        assert_eq!(row.7.parse::<Decimal>().unwrap(), Decimal::new(4, 1));

        let base_cost = row.4.parse::<Decimal>().unwrap()
            + row.5.parse::<Decimal>().unwrap()
            + row.6.parse::<Decimal>().unwrap()
            + row.7.parse::<Decimal>().unwrap();
        let total_cost: Decimal = conn
            .query_row(
                "SELECT total_cost_usd FROM proxy_request_logs
             WHERE data_source = 'codex_session'",
                [],
                |row| row.get::<_, String>(0),
            )?
            .parse()
            .unwrap();
        assert_eq!(base_cost, Decimal::new(384, 2));
        assert_eq!(total_cost, Decimal::new(96, 1));
        assert_eq!(total_cost, base_cost * Decimal::new(25, 1));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn gpt_6_sol_luna_import_prices_fast_and_standard_without_changing_api_rates(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        for (model, tier, expected) in [
            ("gpt-6-sol", "priority", "4.8"),
            ("gpt-6-sol-high", "fast", "4.8"),
            ("gpt-6-luna", "fast", "0.24"),
            ("gpt-6-luna-xhigh", "default", "0.096"),
        ] {
            let db = Database::memory()?;
            let temp = tempdir().unwrap();
            let file = rollout_path(temp.path(), PARENT_ID);
            write_jsonl(
                &file,
                &[
                    session_meta(PARENT_ID),
                    thread_settings_with_tier_at(Some(tier), model, "2026-09-24T03:00:01Z"),
                    token_count_with_cache_write_at(
                        1_000_000,
                        100_000,
                        100_000,
                        10_000,
                        "2026-09-24T03:00:02Z",
                    ),
                ],
            );
            assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
            assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
            let conn = lock_conn!(db.conn);
            let (total, write_tokens, write_cost): (String, i64, String) = conn.query_row(
                "SELECT total_cost_usd, cache_creation_tokens, cache_creation_cost_usd FROM proxy_request_logs WHERE data_source='codex_session'",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            assert_eq!(
                total.parse::<Decimal>().unwrap(),
                expected.parse::<Decimal>().unwrap(),
                "{model}"
            );
            assert_eq!(write_tokens, 100_000);
            let api_pricing = find_model_pricing(&conn, model).unwrap();
            assert_eq!(
                write_cost.parse::<Decimal>().unwrap(),
                Decimal::new(1, 1) * api_pricing.input_cost_per_million
            );
            // The shared price table still retains the API write premium.
            assert!(
                api_pricing.cache_creation_cost_per_million > api_pricing.input_cost_per_million
            );
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn astra_fast_import_keeps_cache_write_and_persists_quota_multiplier() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_with_tier_at(Some("fast"), "gpt-6-astra", "2026-09-10T03:00:01Z"),
                token_count_with_cache_write_at(
                    1_000_000,
                    100_000,
                    100_000,
                    10_000,
                    "2026-09-10T03:00:02Z",
                ),
            ],
        );
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 1);
        assert_eq!(sync_test_file(&db, &file, &[&file])?.imported, 0);
        let conn = lock_conn!(db.conn);
        let (multiplier, cache_write, total): (String,i64,String) = conn.query_row(
            "SELECT cost_multiplier,cache_creation_tokens,total_cost_usd FROM proxy_request_logs WHERE data_source='codex_session'",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        assert_eq!(multiplier.parse::<Decimal>().unwrap(), Decimal::new(25, 1));
        assert_eq!(cache_write, 100_000);
        // Total input includes cached reads and writes: 800k uncached, 100k read,
        // 100k write (ordinary input rate) and 10k output, at 2.5x.
        assert_eq!(total.parse::<Decimal>().unwrap(), Decimal::new(24, 0));
        Ok(())
    }

    #[test]
    fn test_codex_session_cache_writes_dedup_against_legacy_proxy_log() -> Result<(), AppError> {
        // v3.17.0 之前的代理不记录 Codex 缓存写入（legacy 语义行写入量恒为 0）。
        // 重建时带写入量的 session 行仍是同一请求；新语义代理行保持精确匹配。
        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            for (request_id, created_at, semantics) in [
                ("legacy-proxy", 1000, INPUT_TOKEN_SEMANTICS_LEGACY),
                ("total-proxy", 5000, INPUT_TOKEN_SEMANTICS_TOTAL),
            ] {
                conn.execute(
                    "INSERT INTO proxy_request_logs (
                        request_id, provider_id, app_type, model, request_model,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        total_cost_usd, latency_ms, status_code, created_at, data_source,
                        input_token_semantics
                    ) VALUES (?1, 'openai', 'codex', 'gpt-5.6', 'gpt-5.6', 1000, 50, 300, 0,
                              '0.01', 100, 200, ?2, 'proxy', ?3)",
                    rusqlite::params![request_id, created_at, semantics],
                )?;
            }
        }

        let delta = DeltaTokens {
            input: 1000,
            cached_input: 300,
            cache_write_input: 600,
            cache_write_reported: true,
            output: 50,
        };
        let mut suspected_duplicates = 0;
        for (request_id, timestamp, expect_inserted) in [
            ("session-legacy", "1970-01-01T00:16:45Z", false),
            ("session-total", "1970-01-01T01:23:25Z", true),
        ] {
            let inserted = insert_codex_session_entry(
                &db,
                request_id,
                &delta,
                "gpt-5.6",
                CodexServiceTier::Unknown,
                Some("session-1"),
                Some(timestamp),
                &mut suspected_duplicates,
            )?;
            assert_eq!(inserted, expect_inserted, "{request_id}");
        }
        Ok(())
    }

    #[test]
    fn test_codex_session_duplicate_is_observed_but_still_inserted() -> Result<(), AppError> {
        let db = Database::memory()?;
        let delta = DeltaTokens {
            input: 10,
            cached_input: 1,
            cache_write_input: 0,
            cache_write_reported: false,
            output: 2,
        };
        let mut suspected_duplicates = 0;
        assert!(insert_codex_session_entry(
            &db,
            "codex-session-a",
            &delta,
            "gpt-5.4",
            CodexServiceTier::Unknown,
            Some("session-a"),
            Some("1970-01-01T00:16:40Z"),
            &mut suspected_duplicates,
        )?);
        assert!(insert_codex_session_entry(
            &db,
            "codex-session-b",
            &delta,
            "gpt-5.4",
            CodexServiceTier::Unknown,
            Some("session-b"),
            Some("1970-01-01T00:16:45Z"),
            &mut suspected_duplicates,
        )?);
        assert_eq!(suspected_duplicates, 1);

        let conn = lock_conn!(db.conn);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'codex_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_replaces_covered_thread_but_preserves_missing_source_history(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let codex_dir = temp.path();
        let covered_cursor = rollout_path(&codex_dir.join("sessions"), PARENT_ID);
        let stale_same_thread_cursor = rollout_path(&codex_dir.join("old/sessions"), PARENT_ID);
        let missing_source_cursor = rollout_path(&codex_dir.join("sessions"), CHILD_B_ID);
        let created_at = 1_700_000_000;

        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        insert_live_codex_row(&db, CHILD_B_ID, 1, 20, "3", created_at + 1)?;

        {
            let conn = lock_conn!(db.conn);
            for (path, offset) in [
                (&covered_cursor, 3),
                (&stale_same_thread_cursor, 7),
                (&missing_source_cursor, 5),
            ] {
                conn.execute(
                    "INSERT INTO session_log_sync
                     (file_path, last_modified, last_line_offset, last_synced_at)
                     VALUES (?1, 1, ?2, 1)",
                    rusqlite::params![path.to_string_lossy(), offset],
                )?;
            }
        }

        let staged_cursor = StagedCodexCursor {
            file_path: covered_cursor.to_string_lossy().to_string(),
            last_modified: 9,
            last_line_offset: 9,
            last_byte_offset: None,
            last_synced_at: 9,
        };
        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "2", created_at)],
            vec![staged_cursor],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (1, 0));
        let conn = lock_conn!(db.conn);
        let covered: (i64, String, String, i64) = conn.query_row(
            "SELECT input_tokens, total_cost_usd, cost_multiplier, created_at
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(covered.0, 99);
        assert!(covered.1.parse::<Decimal>().unwrap() > Decimal::ZERO);
        assert_eq!(covered.2, "2.5");
        assert_eq!(covered.3, created_at);
        let missing_source: (i64, String) = conn.query_row(
            "SELECT input_tokens, total_cost_usd
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(CHILD_B_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(missing_source, (20, "3".to_string()));
        let covered_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [covered_cursor.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        let missing_source_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [missing_source_cursor.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        let stale_same_thread_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [stale_same_thread_cursor.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(
            (
                covered_offset,
                stale_same_thread_offset,
                missing_source_offset
            ),
            (9, 7, 5)
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_blocks_reused_request_id_with_different_timestamp() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let cursor_path = rollout_path(&temp.path().join("sessions"), PARENT_ID);
        let created_at = 1_700_000_000;
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO session_log_sync
                 (file_path, last_modified, last_line_offset, last_synced_at)
                 VALUES (?1, 3, 3, 3)",
                [cursor_path.to_string_lossy().to_string()],
            )?;
        }

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at + 500)],
            vec![StagedCodexCursor {
                file_path: cursor_path.to_string_lossy().to_string(),
                last_modified: 9,
                last_line_offset: 9,
                last_byte_offset: None,
                last_synced_at: 9,
            }],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (0, 1));
        let conn = lock_conn!(db.conn);
        let live: (i64, String, i64) = conn.query_row(
            "SELECT input_tokens, total_cost_usd, created_at
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let cursor_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [cursor_path.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(live, (10, "1".to_string(), created_at));
        assert_eq!(cursor_offset, 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_blocks_timestamp_collision_even_on_rollup_protected_date(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let cursor_path = rollout_path(&temp.path().join("sessions"), PARENT_ID);
        let created_at = 1_700_000_000;
        let protected_date = local_date_from_unix(created_at).unwrap();
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO usage_daily_rollups (date, app_type, provider_id, model)
                 VALUES (?1, 'codex', '_codex_session', 'gpt-5.6-sol')",
                [&protected_date],
            )?;
            conn.execute(
                "INSERT INTO session_log_sync
                 (file_path, last_modified, last_line_offset, last_synced_at)
                 VALUES (?1, 3, 3, 3)",
                [cursor_path.to_string_lossy().to_string()],
            )?;
        }

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at + 500)],
            vec![StagedCodexCursor {
                file_path: cursor_path.to_string_lossy().to_string(),
                last_modified: 9,
                last_line_offset: 9,
                last_byte_offset: None,
                last_synced_at: 9,
            }],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (0, 1));
        let conn = lock_conn!(db.conn);
        let live_input: i64 = conn.query_row(
            "SELECT input_tokens FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| row.get(0),
        )?;
        let cursor_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [cursor_path.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(live_input, 10);
        assert_eq!(cursor_offset, 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_preserves_entire_thread_when_stage_does_not_cover_live_rows(
    ) -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let codex_dir = temp.path();
        let cursor_path = rollout_path(&codex_dir.join("sessions"), PARENT_ID);
        let created_at = 1_700_000_000;
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        insert_live_codex_row(&db, PARENT_ID, 2, 20, "2", created_at + 1)?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO session_log_sync
                 (file_path, last_modified, last_line_offset, last_synced_at)
                 VALUES (?1, 20, 20, 20)",
                [cursor_path.to_string_lossy().to_string()],
            )?;
        }

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at)],
            vec![StagedCodexCursor {
                file_path: cursor_path.to_string_lossy().to_string(),
                last_modified: 10,
                last_line_offset: 10,
                last_byte_offset: None,
                last_synced_at: 10,
            }],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (0, 1));
        let conn = lock_conn!(db.conn);
        let values = [1u32, 2u32]
            .into_iter()
            .map(|event_index| {
                conn.query_row(
                    "SELECT input_tokens, total_cost_usd
                     FROM proxy_request_logs WHERE request_id = ?1",
                    [thread_request_id(PARENT_ID, event_index)],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(values, vec![(10, "1".to_string()), (20, "2".to_string())]);
        let cursor_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [cursor_path.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(cursor_offset, 20);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_protects_any_existing_codex_rollup_date() -> Result<(), AppError> {
        let db = Database::memory()?;
        let created_at = 1_700_000_000;
        let protected_date = local_date_from_unix(created_at).expect("valid local date");
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO usage_daily_rollups (date, app_type, provider_id, model)
                 VALUES (?1, 'codex', 'real-provider', 'gpt-5.6-sol')",
                [&protected_date],
            )?;
        }

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![
                staged_codex_row(PARENT_ID, 1, 99, "9", created_at),
                staged_codex_row(CHILD_A_ID, 1, 33, "3", created_at),
                staged_codex_row(CHILD_B_ID, 1, 44, "4", created_at + 86_400),
                staged_codex_row(CHILD_B_ID, 2, 55, "5", created_at + 2 * 86_400),
                staged_codex_row(CHILD_B_ID, 3, 66, "6", created_at + 3 * 86_400),
            ],
            vec![],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (1, 4));
        let conn = lock_conn!(db.conn);
        let detail: (i64, String) = conn.query_row(
            "SELECT input_tokens, total_cost_usd
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let rollup_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM usage_daily_rollups
             WHERE date = ?1 AND app_type = 'codex' AND provider_id = 'real-provider'",
            [&protected_date],
            |row| row.get(0),
        )?;
        let detail_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'codex_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(detail, (10, "1".to_string()));
        let outside_halo_input: i64 = conn.query_row(
            "SELECT input_tokens FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(CHILD_B_ID, 3)],
            |row| row.get(0),
        )?;
        assert_eq!(detail_count, 2);
        assert_eq!(outside_halo_input, 66);
        assert_eq!(rollup_count, 1);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_blocks_source_without_completed_staging_cursor() {
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(10, 2, 1),
            ],
        );

        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        assert!(!audit.blocked_threads.contains(PARENT_ID));
        let blocked = finalize_codex_rebuild_audit(audit, &[]);
        assert!(blocked.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_propagates_changed_parent_to_all_descendants() {
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        let grandchild = rollout_path(temp.path(), CHILD_B_ID);
        write_jsonl(
            &parent,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(10, 2, 1),
            ],
        );
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:03Z"),
                token_count_at(20, 4, 2, "2026-07-10T03:00:04Z"),
            ],
        );
        write_jsonl(
            &grandchild,
            &[
                session_meta_at(CHILD_B_ID, Some(CHILD_A_ID), None, "2026-07-10T03:00:05Z"),
                token_count_at(30, 6, 3, "2026-07-10T03:00:06Z"),
            ],
        );

        let files = vec![parent.clone(), child.clone(), grandchild.clone()];
        let audit = audit_codex_rebuild_sources(&files);
        assert!(audit.blocked_threads.is_empty());
        let staged_cursors = files
            .iter()
            .map(|path| StagedCodexCursor {
                file_path: path.to_string_lossy().to_string(),
                last_modified: 1,
                last_line_offset: 3,
                last_byte_offset: None,
                last_synced_at: 1,
            })
            .collect::<Vec<_>>();

        let mut output = fs::OpenOptions::new().append(true).open(&parent).unwrap();
        writeln!(output, "{}", turn_context_at("2026-07-10T03:00:07Z")).unwrap();
        drop(output);

        let blocked = finalize_codex_rebuild_audit(audit, &staged_cursors);
        assert!(blocked.contains(PARENT_ID));
        assert!(blocked.contains(CHILD_A_ID));
        assert!(blocked.contains(CHILD_B_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_blocks_billable_event_without_trusted_timestamp() {
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_without_timestamp(10, 2, 1),
            ],
        );
        let staged_cursor = StagedCodexCursor {
            file_path: file.to_string_lossy().to_string(),
            last_modified: 1,
            last_line_offset: 3,
            last_byte_offset: None,
            last_synced_at: 1,
        };

        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        let blocked = finalize_codex_rebuild_audit(audit, &[staged_cursor]);
        assert!(blocked.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_blocks_divergent_sources_for_same_thread() {
        let temp = tempdir().unwrap();
        let active_dir = temp.path().join("sessions");
        let archived_dir = temp.path().join("archived_sessions");
        fs::create_dir_all(&active_dir).unwrap();
        fs::create_dir_all(&archived_dir).unwrap();
        let active = rollout_path(&active_dir, PARENT_ID);
        let archived = rollout_path(&archived_dir, PARENT_ID);
        write_jsonl(
            &active,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(10, 2, 1),
            ],
        );
        write_jsonl(
            &archived,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count(11, 2, 1),
            ],
        );

        let audit = audit_codex_rebuild_sources(&[active, archived]);
        assert!(audit.blocked_threads.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_blocks_same_thread_sources_with_different_root_timestamps() {
        let temp = tempdir().unwrap();
        let active_dir = temp.path().join("sessions");
        let archived_dir = temp.path().join("archived_sessions");
        fs::create_dir_all(&active_dir).unwrap();
        fs::create_dir_all(&archived_dir).unwrap();
        let active = rollout_path(&active_dir, PARENT_ID);
        let archived = rollout_path(&archived_dir, PARENT_ID);
        write_jsonl(
            &active,
            &[
                session_meta_at(PARENT_ID, None, None, "2026-07-10T03:00:00Z"),
                turn_context(),
                token_count(10, 2, 1),
            ],
        );
        write_jsonl(
            &archived,
            &[
                session_meta_at(PARENT_ID, None, None, "2026-07-10T03:00:01Z"),
                turn_context(),
                token_count(10, 2, 1),
            ],
        );

        let audit = audit_codex_rebuild_sources(&[active, archived]);
        assert!(audit.blocked_threads.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_normalizes_equivalent_event_timestamp_offsets() {
        let temp = tempdir().unwrap();
        let active_dir = temp.path().join("sessions");
        let archived_dir = temp.path().join("archived_sessions");
        fs::create_dir_all(&active_dir).unwrap();
        fs::create_dir_all(&archived_dir).unwrap();
        let active = rollout_path(&active_dir, PARENT_ID);
        let archived = rollout_path(&archived_dir, PARENT_ID);
        write_jsonl(
            &active,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(10, 2, 1, "2026-07-10T03:00:02Z"),
            ],
        );
        write_jsonl(
            &archived,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(10, 2, 1, "2026-07-10T11:00:02+08:00"),
            ],
        );

        let audit = audit_codex_rebuild_sources(&[active, archived]);
        assert!(!audit.blocked_threads.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn rebuild_audit_includes_zero_delta_signatures_in_duplicate_source_fingerprint() {
        let temp = tempdir().unwrap();
        let active_dir = temp.path().join("sessions");
        let archived_dir = temp.path().join("archived_sessions");
        fs::create_dir_all(&active_dir).unwrap();
        fs::create_dir_all(&archived_dir).unwrap();
        let active = rollout_path(&active_dir, PARENT_ID);
        let archived = rollout_path(&archived_dir, PARENT_ID);
        write_jsonl(
            &active,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(10, 2, 1, "2026-07-10T03:00:02Z"),
                token_count_at(5, 1, 0, "2026-07-10T03:00:03Z"),
            ],
        );
        write_jsonl(
            &archived,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(10, 2, 1, "2026-07-10T03:00:02Z"),
                token_count_at(6, 1, 0, "2026-07-10T03:00:03Z"),
            ],
        );

        let active_parsed = parse_codex_file(&active, Some(PARENT_ID.to_string())).unwrap();
        assert!(active_parsed.token_events[1].event_index.is_none());
        let audit = audit_codex_rebuild_sources(&[active, archived]);
        assert!(audit.blocked_threads.contains(PARENT_ID));
    }

    #[test]
    #[serial_test::serial]
    fn unterminated_tail_is_excluded_from_rebuild_and_retried_after_completion(
    ) -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        let meta_line = session_meta(PARENT_ID).to_string();
        let context_line = turn_context().to_string();
        let token_line = token_count(10, 2, 1).to_string();
        let split = token_line
            .find("token_count")
            .expect("token event contains its payload type");
        fs::write(
            &file,
            format!("{meta_line}\n{context_line}\n{}", &token_line[..split]),
        )
        .unwrap();

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert!(parsed.has_unterminated_tail);
        assert_eq!(parsed.line_offset, 2);
        assert!(parsed.token_events.is_empty());
        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        assert!(audit.blocked_threads.contains(PARENT_ID));

        let partial_mtime = fs::metadata(&file).unwrap().modified().unwrap();
        let partial = sync_test_file(&db, &file, &[&file])?;
        assert_eq!((partial.imported, partial.deferred), (0, true));
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?.1, 2);
        assert_unchanged_codex_file_is_skipped(&db, &file)?;

        {
            let mut output = fs::OpenOptions::new().append(true).open(&file).unwrap();
            output.write_all(&token_line.as_bytes()[split..]).unwrap();
            output.write_all(b"\n").unwrap();
            output
                .set_times(fs::FileTimes::new().set_modified(partial_mtime))
                .unwrap();
        }

        let completed = sync_test_file(&db, &file, &[&file])?;
        assert_eq!(completed.imported, 1);
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?.1, 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn complete_final_json_without_newline_is_imported() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        fs::write(
            &file,
            [
                session_meta(PARENT_ID).to_string(),
                turn_context().to_string(),
                token_count(10, 2, 1).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

        let parsed = parse_codex_file(&file, Some(PARENT_ID.to_string()))?;
        assert!(!parsed.has_unterminated_tail);
        assert_eq!(parsed.line_offset, 3);
        assert_eq!(parsed.token_events.len(), 1);
        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        assert!(!audit.blocked_threads.contains(PARENT_ID));

        let result = sync_test_file(&db, &file, &[&file])?;
        assert_eq!((result.imported, result.deferred), (1, false));
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?.1, 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn partial_parent_defers_child_until_parent_tail_completes() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let parent = rollout_path(temp.path(), PARENT_ID);
        let child = rollout_path(temp.path(), CHILD_A_ID);
        let parent_tail = token_count_at(200, 100, 20, "2026-07-10T03:00:02Z").to_string();
        let split = parent_tail.len() / 2;
        fs::write(
            &parent,
            format!(
                "{}\n{}\n{}\n{}",
                session_meta(PARENT_ID),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                turn_context_at("2026-07-10T03:00:10Z"),
                &parent_tail[..split]
            ),
        )
        .unwrap();
        write_jsonl(
            &child,
            &[
                session_meta_at(CHILD_A_ID, Some(PARENT_ID), None, "2026-07-10T03:00:03Z"),
                token_count_at(100, 50, 10, "2026-07-10T03:00:01Z"),
                token_count_at(200, 100, 20, "2026-07-10T03:00:02Z"),
                token_count_at(250, 120, 25, "2026-07-10T03:00:04Z"),
            ],
        );

        let deferred = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!((deferred.imported, deferred.deferred), (0, true));
        assert_eq!(get_sync_state(&db, &child.to_string_lossy())?, (0, 0));

        {
            let mut output = fs::OpenOptions::new().append(true).open(&parent).unwrap();
            output.write_all(&parent_tail.as_bytes()[split..]).unwrap();
            output.write_all(b"\n").unwrap();
        }
        let recovered = sync_test_file(&db, &child, &[&parent, &child])?;
        assert_eq!((recovered.imported, recovered.deferred), (1, false));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_excludes_audited_incomplete_thread() -> Result<(), AppError> {
        let db = Database::memory()?;
        let created_at = 1_700_000_000;
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        let blocked_threads = HashSet::from([PARENT_ID.to_string()]);

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at)],
            vec![],
            &blocked_threads,
        )?;

        assert_eq!((imported, skipped), (0, 1));
        let conn = lock_conn!(db.conn);
        let live: (i64, String) = conn.query_row(
            "SELECT input_tokens, total_cost_usd
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(live, (10, "1".to_string()));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_does_not_apply_synced_prefix_of_incomplete_rollout() -> Result<(), AppError> {
        let db = Database::memory()?;
        let staging = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        let timestamp = "2026-07-10T03:00:02Z";
        let created_at = DateTime::parse_from_rfc3339(timestamp).unwrap().timestamp();
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                turn_context(),
                token_count_at(100, 20, 10, timestamp),
            ],
        );
        let mut writer = fs::OpenOptions::new().append(true).open(&file).unwrap();
        writer.write_all(b"{\"type\":\"event_msg\"").unwrap();
        let audit = audit_codex_rebuild_sources(std::slice::from_ref(&file));
        let result = sync_test_file(&staging, &file, &[&file])?;
        assert_eq!((result.imported, result.deferred), (1, true));
        assert_unchanged_codex_file_is_skipped(&staging, &file)?;
        let cursors = load_staged_codex_cursors(&staging)?;
        let blocked = finalize_codex_rebuild_audit(audit, &cursors);
        assert!(blocked.contains(PARENT_ID));
        assert_eq!(
            apply_staged_codex_usage(&db, load_staged_codex_rows(&staging)?, cursors, &blocked)?,
            (0, 1)
        );
        let conn = lock_conn!(db.conn);
        let live: (i64, String) = conn.query_row(
            "SELECT input_tokens, total_cost_usd FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(live, (10, "1".to_string()));
        let cursors: i64 = conn.query_row("SELECT COUNT(*) FROM session_log_sync", [], |row| {
            row.get(0)
        })?;
        assert_eq!(cursors, 0);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_rolls_back_deleted_detail_when_insert_fails() -> Result<(), AppError> {
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let cursor_path = rollout_path(&temp.path().join("sessions"), PARENT_ID);
        let created_at = 1_700_000_000;
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "1", created_at)?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO session_log_sync
                 (file_path, last_modified, last_line_offset, last_synced_at)
                 VALUES (?1, 3, 3, 3)",
                [cursor_path.to_string_lossy().to_string()],
            )?;
            conn.execute_batch(
                "CREATE TRIGGER reject_rebuilt_codex_row
                 BEFORE INSERT ON proxy_request_logs
                 WHEN NEW.data_source = 'codex_session' AND NEW.input_tokens = 99
                 BEGIN
                   SELECT RAISE(ABORT, 'synthetic rebuild insert failure');
                 END;",
            )?;
        }

        let result = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at)],
            vec![StagedCodexCursor {
                file_path: cursor_path.to_string_lossy().to_string(),
                last_modified: 9,
                last_line_offset: 9,
                last_byte_offset: None,
                last_synced_at: 9,
            }],
            &HashSet::new(),
        );
        assert!(result.is_err());

        let conn = lock_conn!(db.conn);
        let live: (i64, String) = conn.query_row(
            "SELECT input_tokens, total_cost_usd
             FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let cursor_offset: i64 = conn.query_row(
            "SELECT last_line_offset FROM session_log_sync WHERE file_path = ?1",
            [cursor_path.to_string_lossy().to_string()],
            |row| row.get(0),
        )?;
        assert_eq!(live, (10, "1".to_string()));
        assert_eq!(cursor_offset, 3);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn safe_rebuild_fails_closed_on_invalid_live_cost() -> Result<(), AppError> {
        let db = Database::memory()?;
        let created_at = 1_700_000_000;
        insert_live_codex_row(&db, PARENT_ID, 1, 10, "invalid", created_at)?;

        let (imported, skipped) = apply_staged_codex_usage(
            &db,
            vec![staged_codex_row(PARENT_ID, 1, 99, "9", created_at)],
            vec![],
            &HashSet::new(),
        )?;

        assert_eq!((imported, skipped), (0, 1));
        let conn = lock_conn!(db.conn);
        let live_cost: String = conn.query_row(
            "SELECT total_cost_usd FROM proxy_request_logs WHERE request_id = ?1",
            [thread_request_id(PARENT_ID, 1)],
            |row| row.get(0),
        )?;
        assert_eq!(live_cost, "invalid");
        Ok(())
    }

    // ── 模型名归一化测试 ──

    #[test]
    fn test_normalize_codex_model_lowercase() {
        assert_eq!(normalize_codex_model("GLM-4.6"), "glm-4.6");
        assert_eq!(normalize_codex_model("DeepSeek-Chat"), "deepseek-chat");
        assert_eq!(normalize_codex_model("GPT-5.4"), "gpt-5.4");
    }

    #[test]
    fn test_normalize_codex_model_strip_prefix() {
        assert_eq!(normalize_codex_model("openai/gpt-5.4"), "gpt-5.4");
        assert_eq!(
            normalize_codex_model("azure/gpt-5.2-codex"),
            "gpt-5.2-codex"
        );
        assert_eq!(normalize_codex_model("OPENAI/GPT-5.4"), "gpt-5.4");
    }

    #[test]
    fn test_normalize_codex_model_strip_iso_date() {
        assert_eq!(normalize_codex_model("gpt-5.4-2026-03-05"), "gpt-5.4");
        assert_eq!(
            normalize_codex_model("gpt-5.4-pro-2026-03-05"),
            "gpt-5.4-pro"
        );
    }

    #[test]
    fn test_normalize_codex_model_strip_compact_date() {
        assert_eq!(normalize_codex_model("gpt-5.4-20260305"), "gpt-5.4");
        assert_eq!(
            normalize_codex_model("claude-opus-4-6-20260206"),
            "claude-opus-4-6"
        );
    }

    #[test]
    fn test_normalize_codex_model_no_change() {
        assert_eq!(normalize_codex_model("gpt-5.4"), "gpt-5.4");
        assert_eq!(normalize_codex_model("gpt-5.2-codex"), "gpt-5.2-codex");
        assert_eq!(normalize_codex_model("o3"), "o3");
        assert_eq!(normalize_codex_model("deepseek-chat"), "deepseek-chat");
    }

    #[test]
    fn test_normalize_codex_model_combined() {
        // prefix + uppercase + ISO date
        assert_eq!(
            normalize_codex_model("openai/GPT-5.4-2026-03-05"),
            "gpt-5.4"
        );
        // prefix + compact date
        assert_eq!(normalize_codex_model("openai/gpt-5.4-20260305"), "gpt-5.4");
    }

    #[test]
    fn test_cached_clamped_to_input() {
        // cached > input 的异常场景应被 min() 钳制
        let prev = Some(CumulativeTokens {
            input: 100,
            cached_input: 0,
            cache_write_input: 0,
            output: 50,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 110,       // delta = 10
            cached_input: 80, // delta = 80（异常：大于 input delta）
            cache_write_input: 0,
            output: 60,
            ..CumulativeTokens::default()
        };
        let delta = compute_delta(&prev, &current);
        // 钳制前：cached_input = 80, input = 10
        assert_eq!(delta.cached_input, 80);
        assert_eq!(delta.input, 10);
        // 实际钳制在调用侧：delta.cached_input.min(delta.input)
        let clamped = delta.cached_input.min(delta.input);
        assert_eq!(clamped, 10);
    }

    /// 真实语料回放验收 harness（仅手动运行，勿在 CI 跑）。
    ///
    /// 把真实 `~/.codex/sessions` 语料在内存库上做一次全量重导，输出计时与
    /// 结果快照。用于性能改动的行为等价验证：改动前后各跑一次，两侧
    /// `CODEX_REPLAY_OUT` 文件必须逐字节相同。
    ///
    /// ```bash
    /// CODEX_REPLAY_OUT=/tmp/replay.tsv \
    ///   cargo test --release replay_real_codex_corpus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn replay_real_codex_corpus() -> Result<(), AppError> {
        let Some(real_home) = dirs::home_dir() else {
            eprintln!("[REPLAY] no home dir, skipping");
            return Ok(());
        };
        let real_sessions = real_home.join(".codex").join("sessions");
        if !real_sessions.is_dir() {
            eprintln!("[REPLAY] {} not found, skipping", real_sessions.display());
            return Ok(());
        }

        // 临时 HOME 里只放一个指向真实语料的只读 symlink，避免测试
        // 触碰真实 ~/.cc-switch / ~/.codex 下的任何其他内容。
        let temp = tempfile::tempdir().expect("create temp home");
        fs::create_dir_all(temp.path().join(".codex")).expect("mkdir .codex");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_sessions, temp.path().join(".codex").join("sessions"))
            .expect("symlink sessions");
        let previous_home = std::env::var_os("CC_SWITCH_TEST_HOME");
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());

        clear_codex_replay_caches();
        // CODEX_REPLAY_DISK=1 时用临时 HOME 下的磁盘库：逐行 autocommit 的
        // 主要成本是磁盘 journal fsync，内存库测不出真实写库开销。
        let db = if std::env::var("CODEX_REPLAY_DISK").is_ok() {
            Database::init()?
        } else {
            Database::memory()?
        };
        let start = std::time::Instant::now();
        let result = sync_codex_usage(&db)?;
        let full_elapsed = start.elapsed();
        eprintln!(
            "[REPLAY] full reimport: imported={} skipped={} suspected_dup={} deferred={} files={} errors={} elapsed={:.2?}",
            result.imported,
            result.skipped,
            result.suspected_duplicates,
            result.deferred_files,
            result.files_scanned,
            result.errors.len(),
            full_elapsed
        );

        let start = std::time::Instant::now();
        let steady = sync_codex_usage(&db)?;
        eprintln!(
            "[REPLAY] steady pass: imported={} deferred={} elapsed={:.2?}",
            steady.imported,
            steady.deferred_files,
            start.elapsed()
        );

        if let Ok(out_path) = std::env::var("CODEX_REPLAY_OUT") {
            use std::io::Write;
            let conn = lock_conn!(db.conn);
            let mut stmt = conn
                .prepare(
                    "SELECT request_id, model, request_model, input_tokens, output_tokens,
                            cache_read_tokens, cache_creation_tokens,
                            input_cost_usd, output_cost_usd, cache_read_cost_usd,
                            cache_creation_cost_usd, total_cost_usd,
                            session_id, provider_id, provider_type, status_code,
                            is_streaming, cost_multiplier, created_at, data_source
                     FROM proxy_request_logs
                     WHERE data_source = 'codex_session'
                     ORDER BY request_id",
                )
                .map_err(|e| AppError::Database(e.to_string()))?;
            let rows = stmt
                .query_map([], |row| {
                    let mut fields = Vec::with_capacity(20);
                    for idx in 0..20 {
                        fields.push(match row.get_ref(idx)? {
                            rusqlite::types::ValueRef::Null => "NULL".to_string(),
                            rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                            rusqlite::types::ValueRef::Real(v) => v.to_string(),
                            rusqlite::types::ValueRef::Text(v) => {
                                String::from_utf8_lossy(v).into_owned()
                            }
                            rusqlite::types::ValueRef::Blob(v) => format!("blob:{}", v.len()),
                        });
                    }
                    Ok(fields.join("\t"))
                })
                .map_err(|e| AppError::Database(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| AppError::Database(e.to_string()))?;
            let mut out = fs::File::create(&out_path).expect("create replay out file");
            for line in &rows {
                writeln!(out, "{line}").expect("write replay row");
            }
            eprintln!("[REPLAY] wrote {} rows to {out_path}", rows.len());
        }

        match previous_home {
            Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
            None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
        }
        Ok(())
    }

    /// 按 Codex 实际写出的键顺序（timestamp、type、payload）拼一行；`json!` 会把键
    /// 按字母排序，行头判断种类靠的是真实顺序
    fn raw_line(timestamp: &str, kind: &str, payload: &str) -> String {
        format!(r#"{{"timestamp":"{timestamp}","type":"{kind}","payload":{payload}}}"#)
    }

    fn raw_item(timestamp: &str, payload: &str) -> String {
        raw_line(timestamp, "response_item", payload)
    }

    fn raw_usage(input: u64, output: u64) -> String {
        format!(
            r#"{{"input_tokens":{input},"cached_input_tokens":0,"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}}"#,
            input + output
        )
    }

    /// `total` 是累计用量，`last` 是这一次请求的用量
    fn raw_token_count(timestamp: &str, total: (u64, u64), last: (u64, u64)) -> String {
        raw_line(
            timestamp,
            "event_msg",
            &format!(
                r#"{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{}}},"rate_limits":{{"limit_id":"codex"}}}}"#,
                raw_usage(total.0, total.1),
                raw_usage(last.0, last.1)
            ),
        )
    }

    fn parsed_timings(lines: &[String]) -> Vec<Option<EstimatedTiming>> {
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        fs::write(&file, lines.join("\n") + "\n").unwrap();
        parse_codex_file(&file, Some(PARENT_ID.to_string()))
            .unwrap()
            .token_events
            .iter()
            .filter(|event| event.event_index.is_some())
            .map(|event| event.timing)
            .collect()
    }

    fn parsed_latencies(lines: &[String]) -> Vec<Option<i64>> {
        parsed_timings(lines)
            .into_iter()
            .map(|timing| timing.map(|t| t.latency_ms))
            .collect()
    }

    fn parsed_first_tokens(lines: &[String]) -> Vec<Option<i64>> {
        parsed_timings(lines)
            .into_iter()
            .map(|timing| timing.and_then(|t| t.first_token_ms))
            .collect()
    }

    /// 新版 Codex 的 `item_completed`，键顺序照实际日志
    fn raw_item_completed(timestamp: &str, item_type: &str, started_at_ms: i64) -> String {
        raw_line(
            timestamp,
            "event_msg",
            &format!(
                r#"{{"type":"item_completed","thread_id":"t","turn_id":"u","item":{{"type":"{item_type}","id":"i1","content":[]}},"started_at_ms":{started_at_ms},"completed_at_ms":{started_at_ms}}}"#
            ),
        )
    }

    const USER_MESSAGE: &str = r#"{"type":"message","role":"user","content":[]}"#;
    const ASSISTANT_MESSAGE: &str = r#"{"type":"message","role":"assistant","content":[]}"#;
    const REASONING: &str = r#"{"type":"reasoning","summary":[]}"#;
    const TOOL_CALL: &str = r#"{"type":"custom_tool_call","call_id":"c1","name":"shell"}"#;
    const TOOL_OUTPUT: &str = r#"{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}"#;

    #[test]
    fn test_latency_ends_at_usage_record_not_at_delayed_token_count() -> Result<(), AppError> {
        let lines = vec![
            raw_line(
                "2026-10-02T03:00:00.000Z",
                "turn_context",
                r#"{"model":"gpt-5"}"#,
            ),
            raw_item("2026-10-02T03:00:01.000Z", USER_MESSAGE),
            raw_item("2026-10-02T03:00:05.000Z", REASONING),
            raw_item("2026-10-02T03:00:09.000Z", TOOL_CALL),
            // 响应在这里结束；token_count 要等工具跑完才写
            raw_line(
                "2026-10-02T03:00:09.500Z",
                "token_usage_record",
                &format!(r#"{{"turn_id":"t1","usage":{}}}"#, raw_usage(100, 600)),
            ),
            raw_item("2026-10-02T03:00:20.000Z", TOOL_OUTPUT),
            raw_token_count("2026-10-02T03:00:20.001Z", (100, 600), (100, 600)),
            // 第二次请求：用量记录对不上这次请求，不采信，退回 token_count 的时刻
            raw_item("2026-10-02T03:00:25.000Z", REASONING),
            raw_item("2026-10-02T03:00:30.000Z", ASSISTANT_MESSAGE),
            raw_line(
                "2026-10-02T03:00:30.200Z",
                "token_usage_record",
                &format!(r#"{{"turn_id":"other","usage":{}}}"#, raw_usage(1, 999)),
            ),
            raw_token_count("2026-10-02T03:00:30.300Z", (300, 1300), (200, 700)),
        ];

        assert_eq!(parsed_latencies(&lines), vec![Some(8_500), Some(10_299)]);

        // 估出来的耗时要落进库里
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let mut contents = session_meta(PARENT_ID).to_string();
        contents.push('\n');
        contents.push_str(&(lines.join("\n") + "\n"));
        fs::write(&file, contents).unwrap();
        let db = Database::memory()?;
        sync_test_file(&db, &file, &[&file])?;
        let conn = lock_conn!(db.conn);
        let mut stmt = conn
            .prepare("SELECT latency_ms FROM proxy_request_logs ORDER BY request_id")
            .unwrap();
        let stored: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(stored, vec![8_500, 10_299]);
        Ok(())
    }

    #[test]
    fn test_first_token_is_the_earliest_model_output_item_start() -> Result<(), AppError> {
        // 2026-10-02T03:00:00Z 的毫秒时间戳
        let base = 1_790_910_000_000;
        let lines = vec![
            raw_line(
                "2026-10-02T03:00:00.000Z",
                "turn_context",
                r#"{"model":"gpt-5"}"#,
            ),
            raw_item("2026-10-02T03:00:01.000Z", USER_MESSAGE),
            // 第一次请求：思考从 3.2 秒开始，回复晚一些；工具执行项不算模型输出
            raw_item_completed("2026-10-02T03:00:05.000Z", "Reasoning", base + 3_200),
            raw_item("2026-10-02T03:00:05.001Z", REASONING),
            raw_item_completed("2026-10-02T03:00:06.000Z", "AgentMessage", base + 5_500),
            raw_item("2026-10-02T03:00:06.001Z", ASSISTANT_MESSAGE),
            raw_line(
                "2026-10-02T03:00:06.100Z",
                "token_usage_record",
                &format!(r#"{{"turn_id":"t1","usage":{}}}"#, raw_usage(100, 600)),
            ),
            raw_token_count("2026-10-02T03:00:06.101Z", (100, 600), (100, 600)),
            // 第二次请求一开头就是工具调用，没有思考和回复项，推不出首字
            raw_item_completed("2026-10-02T03:00:07.000Z", "CommandExecution", base + 6_500),
            raw_item("2026-10-02T03:00:09.000Z", TOOL_CALL),
            raw_line(
                "2026-10-02T03:00:09.100Z",
                "token_usage_record",
                &format!(r#"{{"turn_id":"t1","usage":{}}}"#, raw_usage(200, 700)),
            ),
            raw_token_count("2026-10-02T03:00:09.101Z", (300, 1300), (200, 700)),
            // 第三次请求：开始时刻比起点还早，说明对不上，不要
            raw_item_completed("2026-10-02T03:00:11.000Z", "AgentMessage", base + 9_000),
            raw_item("2026-10-02T03:00:11.001Z", ASSISTANT_MESSAGE),
            raw_line(
                "2026-10-02T03:00:11.100Z",
                "token_usage_record",
                &format!(r#"{{"turn_id":"t1","usage":{}}}"#, raw_usage(300, 800)),
            ),
            raw_token_count("2026-10-02T03:00:11.101Z", (600, 2100), (300, 800)),
        ];

        assert_eq!(
            parsed_latencies(&lines),
            vec![Some(5_100), Some(2_999), Some(1_999)]
        );
        assert_eq!(parsed_first_tokens(&lines), vec![Some(2_200), None, None]);

        // 推出来的首字要落进库里
        let dir = tempdir().unwrap();
        let file = rollout_path(dir.path(), PARENT_ID);
        let mut contents = session_meta(PARENT_ID).to_string();
        contents.push('\n');
        contents.push_str(&(lines.join("\n") + "\n"));
        fs::write(&file, contents).unwrap();
        let db = Database::memory()?;
        sync_test_file(&db, &file, &[&file])?;
        let conn = lock_conn!(db.conn);
        let mut stmt = conn
            .prepare("SELECT first_token_ms FROM proxy_request_logs ORDER BY request_id")
            .unwrap();
        let stored: Vec<Option<i64>> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(stored, vec![Some(2_200), None, None]);
        Ok(())
    }

    #[test]
    fn test_latency_without_usage_record_stops_before_tool_execution() {
        let lines = vec![
            raw_item("2026-08-03T03:00:01.000Z", USER_MESSAGE),
            raw_item("2026-08-03T03:00:06.000Z", TOOL_CALL),
            // 工具跑了 10 秒，token_count 紧跟着工具结果写出
            raw_item("2026-08-03T03:00:16.000Z", TOOL_OUTPUT),
            raw_token_count("2026-08-03T03:00:16.001Z", (100, 600), (100, 600)),
            // 限额刷新时重发的快照不是一次请求，不能把下一次请求的起点往后挪
            raw_token_count("2026-08-03T03:00:17.000Z", (100, 600), (100, 600)),
            raw_item("2026-08-03T03:00:20.000Z", REASONING),
            raw_item("2026-08-03T03:00:22.000Z", ASSISTANT_MESSAGE),
            raw_token_count("2026-08-03T03:00:22.100Z", (300, 1300), (200, 700)),
        ];

        assert_eq!(parsed_latencies(&lines), vec![Some(5_000), Some(6_099)]);
    }

    #[test]
    fn test_latency_ignores_items_written_after_their_token_count() {
        // 2025 年的布局：输出项和工具结果补写在它们那次响应的 token_count 之后
        let lines = vec![
            raw_item("2025-10-30T08:00:01.000Z", USER_MESSAGE),
            raw_token_count("2025-10-30T08:00:08.000Z", (100, 600), (100, 600)),
            raw_item("2025-10-30T08:00:08.000Z", REASONING),
            raw_item("2025-10-30T08:00:08.001Z", TOOL_CALL),
            raw_item("2025-10-30T08:00:08.001Z", TOOL_OUTPUT),
            raw_line(
                "2025-10-30T08:00:08.002Z",
                "turn_context",
                r#"{"model":"gpt-5"}"#,
            ),
            // 用户隔了一分多钟才发下一条消息，这段空闲不算进请求耗时
            raw_item("2025-10-30T08:01:40.000Z", USER_MESSAGE),
            raw_token_count("2025-10-30T08:01:50.000Z", (300, 1300), (200, 700)),
        ];

        assert_eq!(parsed_latencies(&lines), vec![Some(7_000), Some(10_000)]);
    }

    #[test]
    fn test_timing_line_is_classified_from_the_line_head() {
        let long_output = format!(
            r#"{{"type":"function_call_output","call_id":"c1","output":"{}"}}"#,
            "x".repeat(4 * LINE_HEAD_BYTES)
        );
        assert_eq!(
            classify_timing_line(&raw_item("2026-10-02T03:00:00.250Z", &long_output)),
            Some((1_790_910_000_250, TimingLine::ToolOutput))
        );
        // 正文里出现的字样不影响判断：种类只看信封上的 type
        let quoted =
            r#"{"type":"message","role":"user","content":"\"type\":\"token_usage_record\""}"#;
        assert_eq!(
            classify_timing_line(&raw_item("2026-10-02T03:00:00.250Z", quoted)).map(|line| line.1),
            Some(TimingLine::Boundary)
        );
        assert_eq!(
            classify_timing_line(&raw_line(
                "2026-10-02T03:00:00.250Z",
                "event_msg",
                r#"{"type":"task_started"}"#
            ))
            .map(|line| line.1),
            Some(TimingLine::TurnReset)
        );
        assert_eq!(
            classify_timing_line(&raw_line(
                "2026-10-02T03:00:00.250Z",
                "event_msg",
                r#"{"type":"item_completed"}"#
            )),
            None
        );
    }

    #[test]
    fn test_latency_does_not_inherit_the_start_of_an_aborted_request() {
        const DEVELOPER_MESSAGE: &str = r#"{"type":"message","role":"developer","content":[]}"#;
        let lines = vec![
            raw_item("2026-10-02T03:00:01.000Z", USER_MESSAGE),
            // 这次请求出了一段思考就被用户中断，等不到 token_count
            raw_item("2026-10-02T03:00:10.000Z", REASONING),
            raw_line(
                "2026-10-02T03:00:20.000Z",
                "event_msg",
                r#"{"type":"turn_aborted","reason":"interrupted"}"#,
            ),
            raw_line(
                "2026-10-02T03:02:00.000Z",
                "event_msg",
                r#"{"type":"task_started"}"#,
            ),
            raw_item("2026-10-02T03:02:00.010Z", USER_MESSAGE),
            raw_item("2026-10-02T03:02:20.000Z", REASONING),
            raw_item("2026-10-02T03:02:30.000Z", ASSISTANT_MESSAGE),
            raw_token_count("2026-10-02T03:02:30.100Z", (100, 600), (100, 600)),
            // 响应中途插进来的 developer 消息不是新一轮，起点不动
            raw_item("2026-10-02T03:02:35.000Z", REASONING),
            raw_item("2026-10-02T03:02:36.000Z", DEVELOPER_MESSAGE),
            raw_item("2026-10-02T03:02:40.000Z", ASSISTANT_MESSAGE),
            raw_token_count("2026-10-02T03:02:40.200Z", (300, 1300), (200, 700)),
        ];

        assert_eq!(parsed_latencies(&lines), vec![Some(30_090), Some(10_100)]);
    }
}
