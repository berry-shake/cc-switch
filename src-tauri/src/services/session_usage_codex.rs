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
//! - `turn_context` → 提取当前 model
//! - `event_msg` (type=thread_settings_applied) → 提取当前请求速度
//! - `event_msg` (type=token_count) → 提取累计 token 用量，计算 delta

use crate::codex_config::get_codex_config_dir;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::{CostCalculator, ModelPricing};
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    metadata_modified_nanos, update_sync_state, update_sync_state_on_conn, SessionSyncResult,
};
use crate::services::sql_helpers::{INPUT_TOKEN_SEMANTICS_LEGACY, INPUT_TOKEN_SEMANTICS_TOTAL};
use crate::services::usage_stats::{
    find_model_pricing, has_suspected_codex_session_duplicate, should_skip_session_insert, DedupKey,
};
use chrono::{DateTime, Local, TimeZone, Utc};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
};

const CODEX_THREAD_REQUEST_ID_PREFIX: &str = "codex_session:thread-v1";

/// Codex rollout 里记录的是请求时应用的 service tier，不保证等同于上游最终
/// served tier。个人版额度估算按用户脚本口径把 Fast 记为 Standard 的 2.5 倍；
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
                    "gpt-5.6" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" | "gpt-5.5"
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
#[derive(Debug, Clone, Default)]
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

#[derive(Debug)]
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
        Ok(self
            .events
            .iter()
            .filter(|event| event.timestamp <= cutoff)
            .map(|event| event.signature.clone())
            .collect())
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
    model: String,
    service_tier: CodexServiceTier,
    timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParentResolution {
    None,
    Parent(String),
    Deferred(String),
}

#[derive(Debug)]
struct ParsedCodexFile {
    root_thread_id: Option<String>,
    root_meta_seen: bool,
    root_timestamp: Option<DateTime<Utc>>,
    parent: ParentResolution,
    token_events: Vec<ParsedTokenEvent>,
    line_offset: i64,
    has_unterminated_tail: bool,
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
    cache_creation_tokens: u32,
    input_token_semantics: i64,
    input_cost_usd: String,
    output_cost_usd: String,
    cache_read_cost_usd: String,
    cache_creation_cost_usd: String,
    total_cost_usd: String,
    session_id: String,
    cost_multiplier: String,
    created_at: i64,
}

impl StagedCodexRow {
    fn thread_id(&self) -> Option<String> {
        let (thread_id, _) = thread_identity_from_request_id(&self.request_id)?;
        let session_id = normalize_thread_id(&self.session_id)?;
        (thread_id == session_id).then_some(thread_id)
    }

    fn local_date(&self) -> Option<String> {
        local_date_from_unix(self.created_at)
    }
}

#[derive(Debug, Clone)]
struct StagedCodexCursor {
    file_path: String,
    last_modified: i64,
    last_line_offset: i64,
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
    model: String,
    service_tier: CodexServiceTier,
    timestamp: CodexRebuildTimestampFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRebuildFileFingerprint {
    root_meta_seen: bool,
    root_timestamp: Option<DateTime<Utc>>,
    parent: ParentResolution,
    events: Vec<CodexRebuildEventFingerprint>,
}

impl CodexRebuildFileFingerprint {
    fn from_parsed(parsed: &ParsedCodexFile) -> Self {
        Self {
            root_meta_seen: parsed.root_meta_seen,
            root_timestamp: parsed.root_timestamp,
            parent: parsed.parent.clone(),
            events: parsed
                .token_events
                .iter()
                .map(|event| CodexRebuildEventFingerprint {
                    signature: event.signature.clone(),
                    input: event.delta.input,
                    cached_input: event.delta.cached_input,
                    cache_write_input: event.delta.cache_write_input,
                    cache_write_reported: event.delta.cache_write_reported,
                    output: event.delta.output,
                    event_index: event.event_index,
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
    Retryable(String),
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
    if let Ok(mut caches) = replay_caches().lock() {
        *caches = CodexReplayCaches::default();
    }
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
///   SELECT（尤其是 archived 继承的 `substr` 后缀匹配无法走索引，逐文件跑等于
///   每 pass 全表扫 N 次）。快照语义：同 pass 内其他文件刚写入的游标对后续
///   archived 继承不可见——影响仅是多一轮由 request_id 去重兜底的重扫，
///   不丢数据、不双算。
/// - `pricing`：模型定价 pass 级缓存。定价表在 pass 进行中被修改时本 pass
///   仍用旧价，下一个同步 pass 生效。
struct CodexSyncPass {
    cursors: HashMap<String, (i64, i64)>,
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
        Ok(Self {
            cursors,
            pricing: HashMap::new(),
        })
    }
}

fn get_codex_sync_state(
    db: &Database,
    file_path: &Path,
    cursors: &HashMap<String, (i64, i64)>,
) -> Result<(i64, i64), AppError> {
    let file_path_str = file_path.to_string_lossy().to_string();
    let state = cursors.get(&file_path_str).copied().unwrap_or((0, 0));
    if state != (0, 0)
        || file_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("archived_sessions")
    {
        return Ok(state);
    }

    let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) else {
        return Ok(state);
    };
    let slash_suffix = format!("/{file_name}");
    let backslash_suffix = format!("\\{file_name}");
    // 与原 SQL 等价：ORDER BY last_line_offset DESC, last_modified DESC LIMIT 1
    // → 在快照上按 (offset, modified) 取最大。
    let inherited = cursors
        .iter()
        .filter(|(path, _)| {
            path.as_str() != file_path_str
                && (path.ends_with(&slash_suffix) || path.ends_with(&backslash_suffix))
        })
        .map(|(_, &(modified, offset))| (offset, modified))
        .max();

    match inherited {
        Some((offset, modified)) => {
            update_sync_state(db, &file_path_str, modified, offset)?;
            Ok((modified, offset))
        }
        None => Ok(state),
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
        let parsed = parse_codex_file(path, Some(thread_id.clone()));
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
            cache_creation_tokens: row.cache_creation_tokens,
            model: Some(row.model.clone()),
            message_id: None,
        };
        let pricing = pricing_cache
            .entry(row.model.clone())
            .or_insert_with(|| find_codex_pricing(conn, &row.model));
        let (input, output, cache_read, cache_creation, total) = match pricing {
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
        row.cache_creation_cost_usd = cache_creation;
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
                cache_creation_tokens: row.get::<_, i64>(7)? as u32,
                input_token_semantics: row.get(8)?,
                input_cost_usd: row.get(9)?,
                output_cost_usd: row.get(10)?,
                cache_read_cost_usd: row.get(11)?,
                cache_creation_cost_usd: row.get(12)?,
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
            "SELECT file_path, last_modified, last_line_offset, last_synced_at
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
                let request_thread =
                    thread_identity_from_request_id(&request_id).map(|(thread_id, _)| thread_id);
                let session_thread = session_id.as_deref().and_then(normalize_thread_id);
                let thread_id = match (request_thread, session_thread) {
                    (Some(request_thread), Some(session_thread))
                        if request_thread == session_thread =>
                    {
                        Some(request_thread)
                    }
                    _ => None,
                };
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
        if let Some(thread_id) = row.thread_id() {
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
                cache_creation_tokens: row.cache_creation_tokens,
                cache_creation_known: row.input_token_semantics == INPUT_TOKEN_SEMANTICS_TOTAL
                    || row.cache_creation_tokens > 0,
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
                        row.cache_creation_tokens,
                        row.input_token_semantics,
                        row.input_cost_usd,
                        row.output_cost_usd,
                        row.cache_read_cost_usd,
                        row.cache_creation_cost_usd,
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
                        file_path, last_modified, last_line_offset, last_synced_at
                     ) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        cursor.file_path,
                        cursor.last_modified,
                        cursor.last_line_offset,
                        cursor.last_synced_at,
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
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    files
}

fn build_rollout_index(files: &[PathBuf]) -> RolloutIndex {
    let mut index = RolloutIndex::new();
    for path in files {
        if let Some(thread_id) = thread_id_from_filename(path) {
            index.entry(thread_id).or_default().push(path.clone());
        }
    }
    for paths in index.values_mut() {
        paths.sort();
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
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn parse_codex_file(
    file_path: &Path,
    root_thread_id: Option<String>,
) -> Result<ParsedCodexFile, AppError> {
    let file =
        fs::File::open(file_path).map_err(|e| AppError::Config(format!("无法打开文件: {e}")))?;
    let mut reader = BufReader::new(file);
    let mut root_meta_seen = false;
    let mut root_timestamp = None;
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
    let mut event_index = 0u32;
    let mut token_events = Vec::new();
    let mut line_offset = 0i64;
    let mut has_unterminated_tail = false;
    let mut has_billable_tokens = false;

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

        let is_event_msg = line.contains("\"event_msg\"");
        let is_turn_context = line.contains("\"turn_context\"");
        let is_session_meta = line.contains("\"session_meta\"");
        if !is_event_msg && !is_turn_context && !is_session_meta {
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

                let meta_thread_id = non_empty_string(
                    payload
                        .get("id")
                        .or_else(|| payload.get("thread_id"))
                        .or_else(|| payload.get("threadId")),
                );
                if let (Some(filename_id), Some(meta_id)) = (&root_thread_id, meta_thread_id) {
                    if filename_id != &meta_id {
                        parent = ParentResolution::Deferred(format!(
                            "文件名线程 ID ({filename_id}) 与 root meta ID ({meta_id}) 不一致"
                        ));
                    }
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

                token_events.push(ParsedTokenEvent {
                    line_offset,
                    signature,
                    delta,
                    event_index: nonzero_index,
                    model: current_model.clone(),
                    service_tier: current_service_tier,
                    timestamp: value
                        .get("timestamp")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                });
            }
            _ => {}
        }
    }

    Ok(ParsedCodexFile {
        root_thread_id,
        root_meta_seen,
        root_timestamp,
        parent,
        token_events,
        line_offset,
        has_unterminated_tail,
        has_billable_tokens,
    })
}

fn parent_signatures_before(
    parent_path: &Path,
    cutoff: DateTime<Utc>,
) -> Result<Vec<TokenUsageSignature>, String> {
    let file = fs::File::open(parent_path)
        .map_err(|error| format!("无法打开父 rollout {}: {error}", parent_path.display()))?;
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
        return timeline.signatures_before(parent_path, cutoff);
    }

    let mut events = Vec::new();
    let mut max_timestamp: Option<DateTime<Utc>> = None;
    let mut has_token_without_timestamp = false;

    // 必须扫描完整父文件，不能在首个未来时间戳处 break：rollout 写入顺序
    // 不承诺时间戳严格单调。缓存完整时间线后，不同 child cutoff 只需内存过滤。
    // 父时间线用于决定 child 需要剥离多少重放前缀，因此读取/解析失败必须
    // fail closed；静默忽略半行可能把父历史再次计入 child。
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_offset = 0i64;
    loop {
        line.clear();
        let next_line_offset = line_offset.saturating_add(1);
        let bytes_read = reader.read_line(&mut line).map_err(|error| {
            format!(
                "读取父 rollout {} 第 {next_line_offset} 行失败: {error}",
                parent_path.display()
            )
        })?;
        if bytes_read == 0 {
            break;
        }
        if line.trim().is_empty() {
            if line.ends_with('\n') {
                line_offset = next_line_offset;
                continue;
            }
            return Err(format!(
                "父 rollout {} 的尾行尚未写完",
                parent_path.display()
            ));
        }
        let value = serde_json::from_str::<serde_json::Value>(&line).map_err(|error| {
            if line.ends_with('\n') {
                format!(
                    "解析父 rollout {} 第 {next_line_offset} 行失败: {error}",
                    parent_path.display()
                )
            } else {
                format!(
                    "父 rollout {} 第 {next_line_offset} 行尚未写完: {error}",
                    parent_path.display()
                )
            }
        })?;
        line_offset = next_line_offset;
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
    }

    let timeline = Arc::new(ParentTokenTimeline {
        events,
        max_timestamp,
        has_token_without_timestamp,
    });
    let result = timeline.signatures_before(parent_path, cutoff);
    if let (Some(stamp), Ok(mut caches)) = (stamp, replay_caches().lock()) {
        caches.parent_timelines.insert(
            parent_path.to_path_buf(),
            CachedParentTimeline {
                stamp,
                timeline: Arc::clone(&timeline),
            },
        );
    }
    result
}

fn resolve_parent_signatures(
    parent_id: &str,
    cutoff: DateTime<Utc>,
    rollout_index: &RolloutIndex,
) -> Result<Vec<TokenUsageSignature>, String> {
    let Some(candidates) = rollout_index.get(parent_id) else {
        return Err(format!("找不到父 rollout: {parent_id}"));
    };

    let mut snapshots = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        snapshots.push(parent_signatures_before(candidate, cutoff)?);
    }
    let Some(first) = snapshots.first() else {
        return Err(format!("找不到父 rollout: {parent_id}"));
    };
    if snapshots.iter().skip(1).any(|snapshot| snapshot != first) {
        return Err(format!(
            "父 rollout UUID {parent_id} 对应多个内容不一致的文件"
        ));
    }
    Ok(first.clone())
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
            PendingReason::Stable(reason) | PendingReason::Retryable(reason) => reason.clone(),
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
    let (last_modified, last_offset) = get_codex_sync_state(db, file_path, &pass.cursors)?;

    // 文件未变化则跳过
    if file_modified <= last_modified {
        return Ok(CodexFileSyncResult::default());
    }

    if let Ok(mut caches) = replay_caches().lock() {
        if let Some(pending) = caches.pending.get(file_path).cloned() {
            if pending.modified == file_modified && pending.size == file_size {
                match &pending.reason {
                    PendingReason::MissingParent(parent) if !rollout_index.contains_key(parent) => {
                        return Ok(CodexFileSyncResult {
                            deferred: true,
                            ..CodexFileSyncResult::default()
                        });
                    }
                    PendingReason::Stable(_) => {
                        return Ok(CodexFileSyncResult {
                            deferred: true,
                            ..CodexFileSyncResult::default()
                        });
                    }
                    PendingReason::Retryable(_) => {
                        caches.pending.remove(file_path);
                    }
                    _ => {
                        caches.pending.remove(file_path);
                    }
                }
            }
        }
    }

    let parsed = parse_codex_file(file_path, thread_id_from_filename(file_path))?;
    if parsed.has_unterminated_tail {
        return Ok(mark_deferred(
            file_path,
            file_modified,
            file_size,
            PendingReason::Stable("JSONL 尾行尚未写完".to_string()),
        ));
    }
    if !parsed.has_billable_tokens {
        update_sync_state(db, &file_path_str, file_modified, parsed.line_offset)?;
        return Ok(CodexFileSyncResult::default());
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
        ParentResolution::None => 0,
        ParentResolution::Deferred(reason) => {
            return Ok(mark_deferred(
                file_path,
                file_modified,
                file_size,
                PendingReason::Stable(reason.clone()),
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
            if let Ok(caches) = replay_caches().lock() {
                if let Some(prefix) = caches
                    .replay_prefixes
                    .get(file_path)
                    .filter(|cached| cached.modified == file_modified && cached.size == file_size)
                    .map(|cached| cached.prefix)
                {
                    prefix
                } else {
                    drop(caches);
                    let parent_signatures =
                        match resolve_parent_signatures(parent_id, cutoff, rollout_index) {
                            Ok(signatures) => signatures,
                            Err(reason) => {
                                let pending_reason = if rollout_index.contains_key(parent_id) {
                                    PendingReason::Retryable(reason)
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
            } else {
                let parent_signatures = resolve_parent_signatures(parent_id, cutoff, rollout_index)
                    .map_err(AppError::Config)?;
                matching_replay_prefix(&parsed.token_events, &parent_signatures)
            }
        }
    };

    if let Ok(mut caches) = replay_caches().lock() {
        caches.pending.remove(file_path);
    }

    let mut result = CodexFileSyncResult::default();
    let mut to_insert: Vec<(&ParsedTokenEvent, u32)> = Vec::new();
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
        to_insert.push((event, event_index));
    }

    // 分批事务写库：逐行 autocommit（journal_mode=delete 下每行一整套
    // journal 建立/fsync/删除）是全量重导的最大耗时项。批内任一插入失败
    // 都回滚该批且不推进游标；下一 pass 重扫时由 request_id 主键 + 指纹
    // 去重兜底，不会丢数据或双算。
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
        for (event, event_index) in batch {
            let request_id =
                format!("{CODEX_THREAD_REQUEST_ID_PREFIX}:{root_thread_id}:{event_index}");
            match insert_codex_session_entry_on_conn(
                &tx,
                &request_id,
                &event.delta,
                &event.model,
                event.service_tier,
                Some(root_thread_id),
                event.timestamp.as_deref(),
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
            update_sync_state_on_conn(&tx, &file_path_str, file_modified, parsed.line_offset)?;
        }
        tx.commit()
            .map_err(|e| AppError::Database(format!("提交 Codex 会话写入事务失败: {e}")))?;

        result.imported = result.imported.saturating_add(batch_imported);
        result.skipped = result.skipped.saturating_add(batch_skipped);
        result.suspected_duplicates = result.suspected_duplicates.saturating_add(batch_suspected);
    }

    if to_insert.is_empty() {
        update_sync_state(db, &file_path_str, file_modified, parsed.line_offset)?;
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
        suspected_duplicates,
        &mut HashMap::new(),
    )
}

/// 插入单条 Codex 会话记录到 proxy_request_logs。
///
/// 调用方负责持锁/事务；`pricing_cache` 按原始 model 字符串键控（
/// `find_codex_pricing` 是纯函数式查找，同串必同结果），全量重导时把
/// 每事件一次的定价 SELECT 降为每模型一次。
#[allow(clippy::too_many_arguments)]
fn insert_codex_session_entry_on_conn(
    conn: &rusqlite::Connection,
    request_id: &str,
    delta: &DeltaTokens,
    model: &str,
    service_tier: CodexServiceTier,
    session_id: Option<&str>,
    timestamp: Option<&str>,
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
        cache_creation_known: delta.cache_write_reported,
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
                0i64,                // latency_ms
                Option::<i64>::None, // first_token_ms
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
    find_model_pricing(conn, &normalize_codex_model(model_id))
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

    fn thread_settings_at(
        service_tier: Option<&str>,
        model: &str,
        timestamp: &str,
    ) -> serde_json::Value {
        let mut settings = serde_json::json!({ "model": model });
        if let Some(service_tier) = service_tier {
            settings["service_tier"] = serde_json::json!(service_tier);
        }
        serde_json::json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": settings
            }
        })
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
            cache_creation_tokens: 4,
            input_token_semantics: INPUT_TOKEN_SEMANTICS_TOTAL,
            input_cost_usd: total_cost_usd.to_string(),
            output_cost_usd: "0".to_string(),
            cache_read_cost_usd: "0".to_string(),
            cache_creation_cost_usd: "0".to_string(),
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

    #[test]
    fn test_delta_first_event() {
        let prev = None;
        let current = CumulativeTokens {
            input: 17934,
            cached_input: 9600,
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
            output: 454,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 36722,
            cached_input: 27904,
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
            output: 1045,
            ..CumulativeTokens::default()
        });
        // task 边界：相同的累计值
        let current = CumulativeTokens {
            input: 58346,
            cached_input: 46976,
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
            output: 30,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 80,
            cached_input: 40,
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
                thread_settings_at(Some("priority"), "gpt-5.6-sol", timestamp),
                token_count_with_cache_write_at(100, 20, 10, 5, timestamp),
                thread_settings_at(Some("default"), "gpt-5.6-terra", timestamp),
                token_count_with_cache_write_at(200, 50, 20, 15, timestamp),
                thread_settings_at(None, "gpt-5.6-luna", timestamp),
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
        let first_timeline =
            Arc::clone(&replay_caches().lock().unwrap().parent_timelines[&parent].timeline);
        assert_eq!(parent_signatures_before(&parent, late).unwrap().len(), 2);

        let caches = replay_caches().lock().unwrap();
        assert_eq!(caches.parent_timelines.len(), 1);
        assert!(Arc::ptr_eq(
            &first_timeline,
            &caches.parent_timelines[&parent].timeline
        ));
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

        let caches = replay_caches().lock().unwrap();
        assert_eq!(caches.parent_timelines.len(), 1);
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
        assert!(first_error.contains("token_count 缺少有效 timestamp"));
        let cached_timeline =
            || Arc::clone(&replay_caches().lock().unwrap().parent_timelines[&parent].timeline);
        let first_timeline = cached_timeline();

        let second_error = parent_signatures_before(&parent, cutoff).unwrap_err();
        assert_eq!(second_error, first_error);
        assert!(Arc::ptr_eq(&first_timeline, &cached_timeline()));

        fs::remove_file(&parent).unwrap();
        let open_error = parent_signatures_before(&parent, cutoff).unwrap_err();
        assert!(open_error.contains("无法打开父 rollout"));
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
        assert_eq!(replay_caches().lock().unwrap().parent_timelines.len(), 1);
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

        let delta = DeltaTokens {
            input: 10,
            cached_input: 1,
            cache_write_input: 0,
            cache_write_reported: false,
            output: 2,
        };
        let mut suspected_duplicates = 0;
        let inserted = insert_codex_session_entry(
            &db,
            "codex-session-dup",
            &delta,
            "gpt-5.4",
            CodexServiceTier::Unknown,
            Some("session-1"),
            Some("1970-01-01T00:16:45Z"),
            &mut suspected_duplicates,
        )?;
        assert!(!inserted);

        let conn = lock_conn!(db.conn);
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| {
            row.get(0)
        })?;
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn test_fast_cache_write_is_persisted_and_costed() -> Result<(), AppError> {
        clear_codex_replay_caches();
        let db = Database::memory()?;
        let temp = tempdir().unwrap();
        let file = rollout_path(temp.path(), PARENT_ID);
        write_jsonl(
            &file,
            &[
                session_meta(PARENT_ID),
                thread_settings_at(Some("priority"), "gpt-5.6-sol", "2026-07-10T03:00:01Z"),
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
        assert_eq!(row.4.parse::<Decimal>().unwrap(), Decimal::from(4));
        assert_eq!(row.5.parse::<Decimal>().unwrap(), Decimal::new(3, 1));
        assert_eq!(row.6.parse::<Decimal>().unwrap(), Decimal::new(5, 2));
        assert_eq!(row.7.parse::<Decimal>().unwrap(), Decimal::new(625, 3));

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
        assert_eq!(base_cost, Decimal::new(4975, 3));
        assert_eq!(total_cost, Decimal::new(124375, 4));
        assert_eq!(total_cost, base_cost * Decimal::new(25, 1));
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
    fn unterminated_tail_is_blocked_and_reprocessed_after_same_line_completes(
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
        assert_eq!(get_sync_state(&db, &file.to_string_lossy())?, (0, 0));

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
            output: 50,
            ..CumulativeTokens::default()
        });
        let current = CumulativeTokens {
            input: 110,       // delta = 10
            cached_input: 80, // delta = 80（异常：大于 input delta）
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
}
