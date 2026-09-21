//! 热词（R10）—— 通过百炼 **上下文增强** 下发词表。
//!
//! 为什么不用即时热词：本产品默认模型 `fun-asr-realtime` 不支持
//! `parameters.vocabulary`（该字段仅 Qwen-Audio-3.0-ASR-Flash 系列支持），
//! 但官方明确支持 `input.context`（run-task 携带、continue-task 运行中更新）。
//! 参见：<https://help.aliyun.com/zh/model-studio/fun-asr-client-events>、
//! <https://help.aliyun.com/zh/model-studio/improve-asr-accuracy>。
//!
//! 官方约束（本模块据此切分，避免服务端静默截断）：
//!   * 每轮 `text` ≤ 400 字符（汉字/字母/数字/空格/标点各计 1），超出从末尾截断；
//!   * 最多保留最近 5 轮，超出的早期消息被忽略且不报错；
//!   * 生效机制是**词表匹配**，`text` 必须包含音频里的待识别**原词**。
//!
//! 额外做词形体检（官方热词文本规范，用于提前提醒用户而不是阻断保存）：
//!   * 含非 ASCII 字符：总字符数 ≤ 15；
//!   * 纯 ASCII：按空格切分 ≤ 7 个片段。

/// 官方约束：单轮上下文文本上限（字符数）。
pub const ROUND_TEXT_MAX_CHARS: usize = 400;
/// 官方约束：服务端最多保留的上下文轮数。
pub const MAX_ROUNDS: usize = 5;
/// 提示阈值：含非 ASCII 字符的热词总字符数上限。
pub const WORD_MAX_CHARS_NON_ASCII: usize = 15;
/// 提示阈值：纯 ASCII 热词按空格切分的片段数上限。
pub const WORD_MAX_ASCII_SEGMENTS: usize = 7;

/// 一个热词的问题描述（用于管理页可见告警，不阻断保存）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotwordWarning {
    pub word: String,
    pub message: String,
}

/// 词表 → 请求上下文的完整方案（不含任何密钥，可安全打印/序列化）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotwordPlan {
    /// 去重去空后的热词。
    pub words: Vec<String>,
    /// 已按 400 字符切好的上下文轮次（user/input_text）。
    pub rounds: Vec<String>,
    /// 被丢弃的过期轮数（超过 5 轮时只保留最近 5 轮）。
    pub dropped_rounds: usize,
    /// 词形与切分告警。
    pub warnings: Vec<HotwordWarning>,
}

impl HotwordPlan {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// 词表指纹：内容相同即指纹相同，用于"未变化不重发"。
    pub fn fingerprint(&self) -> String {
        self.rounds.join("\u{1f}")
    }

    /// `payload.input.context` 的 JSON 值（无热词时为 None，保持旧行为 `input: {}`）。
    pub fn context_json(&self) -> Option<serde_json::Value> {
        build_context_json(&self.rounds)
    }

    /// 供 run-task / continue-task 使用的 `input` 对象。
    pub fn input_json(&self) -> serde_json::Value {
        match self.context_json() {
            Some(context) => serde_json::json!({ "context": context }),
            None => serde_json::json!({}),
        }
    }
}

/// 构造 `input.context`（数组元素为 `{"role":"user","content":[{"type":"input_text","text":...}]}`）。
pub fn build_context_json(rounds: &[String]) -> Option<serde_json::Value> {
    let rounds: Vec<&String> = rounds.iter().filter(|r| !r.trim().is_empty()).collect();
    if rounds.is_empty() {
        return None;
    }
    let messages: Vec<serde_json::Value> = rounds
        .iter()
        .map(|text| {
            serde_json::json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": text }]
            })
        })
        .collect();
    Some(serde_json::Value::Array(messages))
}

fn take_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// 从原始热词列表构造方案：去空白、去重、体检、按 400 字符切轮、只留最近 5 轮。
pub fn plan(words: &[String]) -> HotwordPlan {
    let mut plan = HotwordPlan::default();
    for raw in words {
        let word = raw.trim().to_string();
        if word.is_empty() || plan.words.contains(&word) {
            continue;
        }
        plan.words.push(word);
    }
    for word in &plan.words {
        if let Some(message) = word_warning(word) {
            plan.warnings.push(HotwordWarning { word: word.clone(), message });
        }
    }

    // 词表文本：空格连接（英文热词天然按词分段，中文热词按条区分）。
    let joined = plan.words.join(" ");
    if joined.is_empty() {
        return plan;
    }
    let chars: Vec<char> = joined.chars().collect();
    if chars.len() <= ROUND_TEXT_MAX_CHARS {
        plan.rounds.push(joined);
        plan.warnings.push(HotwordWarning {
            word: String::new(),
            message: format!(
                "词表共 {} 字符，一轮即可下发（上限 {} 字符/轮）",
                chars.len(),
                ROUND_TEXT_MAX_CHARS
            ),
        });
        return plan;
    }

    let mut all: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in &plan.words {
        let word_len = word.chars().count();
        let separator = usize::from(!current.is_empty());
        if current_len + separator + word_len <= ROUND_TEXT_MAX_CHARS {
            if separator == 1 {
                current.push(' ');
                current_len += 1;
            }
            current.push_str(word);
            current_len += word_len;
        } else {
            if !current.is_empty() {
                all.push(std::mem::take(&mut current));
            }
            if word_len > ROUND_TEXT_MAX_CHARS {
                current = take_chars(word, ROUND_TEXT_MAX_CHARS);
                current_len = current.chars().count();
                all.push(std::mem::take(&mut current));
                current_len = 0;
                plan.warnings.push(HotwordWarning {
                    word: word.clone(),
                    message: format!(
                        "单条热词超过 {} 字符，已截断后才下发",
                        ROUND_TEXT_MAX_CHARS
                    ),
                });
            } else {
                current = word.clone();
                current_len = word_len;
            }
        }
    }
    if !current.is_empty() {
        all.push(current);
    }

    // 服务端只保留最近 5 轮：早期轮次丢掉即可，不需要报错。
    if all.len() > MAX_ROUNDS {
        plan.dropped_rounds = all.len() - MAX_ROUNDS;
        plan.warnings.push(HotwordWarning {
            word: String::new(),
            message: format!(
                "词表需要 {} 轮，服务端只保留最近 {} 轮，最早 {} 轮不会生效（请精简热词）",
                all.len(),
                MAX_ROUNDS,
                plan.dropped_rounds
            ),
        });
        all = all.split_off(plan.dropped_rounds);
    }
    plan.rounds = all;
    plan
}

/// 词形规范告警：非 ASCII 总字符数 ≤ 15；纯 ASCII 空格片段数 ≤ 7。
pub fn word_warning(word: &str) -> Option<String> {
    let word = word.trim();
    if word.is_empty() {
        return None;
    }
    let chars = word.chars().count();
    if word.is_ascii() {
        let segments = word.split_whitespace().count();
        if segments > WORD_MAX_ASCII_SEGMENTS {
            return Some(format!(
                "纯 ASCII 热词按空格切分有 {} 个片段，规范上限 {} 个（建议拆成多条短词）",
                segments, WORD_MAX_ASCII_SEGMENTS
            ));
        }
        None
    } else if chars > WORD_MAX_CHARS_NON_ASCII {
        Some(format!(
            "含非 ASCII 字符的热词共 {} 字符，规范上限 {} 个",
            chars, WORD_MAX_CHARS_NON_ASCII
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_list_produces_no_context() {
        let p = plan(&[]);
        assert!(p.is_empty());
        assert!(p.rounds.is_empty());
        assert!(p.context_json().is_none());
        assert_eq!(p.input_json(), serde_json::json!({}));
    }

    #[test]
    fn words_are_trimmed_deduplicated_and_kept_in_order() {
        let p = plan(&v(&[" 铨洲智造 ", "", "区域赛", "铨洲智造"]));
        assert_eq!(p.words, v(&["铨洲智造", "区域赛"]));
        assert_eq!(p.rounds, v(&["铨洲智造 区域赛"]));
        assert!(p.warnings.is_empty() || p.warnings.iter().all(|w| w.word.is_empty()));
    }

    #[test]
    fn context_json_matches_the_official_shape() {
        let p = plan(&v(&["铨洲智造"]));
        let context = p.context_json().expect("context");
        assert_eq!(
            context,
            serde_json::json!([
                {"role": "user", "content": [{"type": "input_text", "text": "铨洲智造"}]}
            ])
        );
    }

    #[test]
    fn rounds_never_exceed_400_chars() {
        // 60 个 8 字热词 = 480 字符 + 分隔符，必须切成多轮。
        let words: Vec<String> = (0..60).map(|i| format!("测试热词编号{:02}", i)).collect();
        let p = plan(&words);
        assert!(p.rounds.len() > 1, "must split: {:?}", p.rounds);
        for round in &p.rounds {
            assert!(
                round.chars().count() <= ROUND_TEXT_MAX_CHARS,
                "round too long: {} chars",
                round.chars().count()
            );
        }
        // 不丢词：所有热词仍出现在某轮里。
        for w in &words {
            assert!(p.rounds.iter().any(|r| r.contains(w.as_str())), "lost {w}");
        }
    }

    #[test]
    fn only_the_most_recent_five_rounds_are_sent() {
        // 300 个热词足以超过 5 轮。
        let words: Vec<String> = (0..300).map(|i| format!("热词{:04}", i)).collect();
        let p = plan(&words);
        assert_eq!(p.rounds.len(), MAX_ROUNDS);
        assert!(p.dropped_rounds > 0);
        assert!(
            p.warnings.iter().any(|w| w.message.contains("只保留最近")),
            "must warn about dropped rounds"
        );
        // 保留的是"最近的"轮次：最后一批热词必须在。
        assert!(p.rounds.last().unwrap().contains("热词0299"));
    }

    #[test]
    fn non_ascii_words_longer_than_15_chars_are_flagged() {
        let p = plan(&v(&["这是一个特别长的中文热词超过十五个字符了"]));
        assert_eq!(
            p.warnings.iter().filter(|w| !w.word.is_empty()).count(),
            1,
            "unexpected warnings: {:?}",
            p.warnings
        );
        assert!(p.warnings[0].message.contains("规范上限 15"));
        // 告警不阻断：词仍然会被下发。
        assert!(p.rounds[0].contains("这是一个特别长的中文热词超过十五个字符了"));
    }

    #[test]
    fn ascii_words_with_more_than_seven_segments_are_flagged() {
        let p = plan(&v(&["The effect of temperature variations on enzyme activity"]));
        assert_eq!(
            p.warnings.iter().filter(|w| !w.word.is_empty()).count(),
            1,
            "unexpected warnings: {:?}",
            p.warnings
        );
        assert!(p.warnings[0].message.contains("8 个片段"));
        assert!(word_warning("Human immunodeficiency virus type 1").is_none());
        assert!(word_warning("Bulge Bracket").is_none());
    }

    #[test]
    fn fingerprint_changes_only_when_the_word_list_changes() {
        let a = plan(&v(&["铨洲智造", "区域赛"]));
        let b = plan(&v(&[" 铨洲智造 ", "区域赛"]));
        let c = plan(&v(&["铨洲智造"]));
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
    }

    #[test]
    fn a_single_word_longer_than_a_round_is_truncated_not_dropped() {
        let long = "字".repeat(500);
        let p = plan(&[long]);
        assert_eq!(p.rounds.len(), 1);
        assert_eq!(p.rounds[0].chars().count(), ROUND_TEXT_MAX_CHARS);
        assert!(p.warnings.iter().any(|w| w.message.contains("已截断")));
    }

    #[test]
    fn hotword_feed_notifies_subscribers_and_keeps_the_latest_plan() {
        let feed = HotwordFeed::default();
        let mut rx = feed.subscribe();
        assert!(!rx.has_changed().unwrap_or(true));
        feed.set(plan(&v(&["铨洲智造"])));
        assert!(rx.has_changed().unwrap_or(false));
        assert_eq!(rx.borrow_and_update().words, v(&["铨洲智造"]));
        // 同样的词表再次写入：watch 仍会触发，但指纹相同，调用方据此跳过重发。
        let first = feed.plan();
        feed.set(plan(&v(&["铨洲智造"])));
        assert_eq!(first.fingerprint(), feed.plan().fingerprint());
    }

    #[test]
    fn hotword_status_reports_applied_and_skipped() {
        let mut status = HotwordStatus::default();
        let plan = plan(&v(&["铨洲智造", "区域赛"]));
        status.record_applied(&plan, "run-task", chrono::Utc::now());
        assert!(status.delivered);
        assert_eq!(status.word_count, 2);
        assert_eq!(status.mode, "run-task");
        assert!(status.last_applied_at.is_some());
        status.record_skipped(&plan);
        assert_eq!(status.skipped_unchanged, 1);
        assert!(status.last_result.contains("未变化") && status.last_result.contains("未重发"));
    }
}

/// 用一组自定义轮次替换 `source` 的轮次，保留词表与告警等元数据。
///
/// provider 拿到的是 pipeline 明确传入的轮次（会话开始时快照），而运行中的更新
/// 来自 watch 通道里的最新方案；两者都需要走同一套 `input_json()` 形状。
pub fn plan_from_rounds(source: &HotwordPlan, rounds: Vec<String>) -> HotwordPlan {
    HotwordPlan {
        words: source.words.clone(),
        rounds,
        dropped_rounds: source.dropped_rounds,
        warnings: source.warnings.clone(),
    }
}

/// 热词下发状态（管理页显示用）。**不包含任何密钥**。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HotwordStatus {
    /// 是否已有会话把词表下发到百炼（run-task 或 continue-task 成功）。
    pub delivered: bool,
    /// 最近一次实际下发的热词条数。
    pub word_count: usize,
    /// 最近一次实际下发的轮数。
    pub round_count: usize,
    /// 被丢弃的过期轮数（超过服务端 5 轮上限）。
    pub dropped_rounds: usize,
    /// 下发方式：`run-task` / `continue-task`。
    pub mode: String,
    /// 最近一次下发时间。
    pub last_applied_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 因"词表未变化"而跳过的重发次数。
    pub skipped_unchanged: u64,
    /// 最近一次同步的结果说明（含"未变化未重发"）。
    pub last_result: String,
}

impl HotwordStatus {
    /// 记录一次真实下发。
    pub fn record_applied(
        &mut self,
        plan: &HotwordPlan,
        mode: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        self.delivered = !plan.is_empty();
        self.word_count = plan.words.len();
        self.round_count = plan.rounds.len();
        self.dropped_rounds = plan.dropped_rounds;
        self.mode = mode.to_string();
        self.last_applied_at = Some(now);
        self.last_result = if plan.is_empty() {
            "词表为空：已按无热词启动".to_string()
        } else {
            format!(
                "已通过 {mode} 下发 {} 个热词 / {} 轮",
                plan.words.len(),
                plan.rounds.len()
            )
        };
    }

    /// 记录一次"内容未变化，因此没有重发"。
    pub fn record_skipped(&mut self, plan: &HotwordPlan) {
        self.skipped_unchanged += 1;
        self.last_result = format!(
            "热词未变化，未重发（{} 个热词 / {} 轮）",
            plan.words.len(),
            plan.rounds.len()
        );
    }
}

/// 运行期热词源：管理页保存 → pipeline 写入 → provider 订阅后 `continue-task`。
#[derive(Clone)]
pub struct HotwordFeed {
    /// 内层用 `RwLock` 包一层，`adopt` 才能在 `&self` 下把 feed 换成同一个实例。
    inner: std::sync::Arc<parking_lot::RwLock<std::sync::Arc<parking_lot::RwLock<FeedInner>>>>,
}

struct FeedInner {
    plan: HotwordPlan,
    /// 只有在有人订阅之后才存在；没有接收端时 `set` 只更新 plan。
    tx: Option<tokio::sync::watch::Sender<HotwordPlan>>,
}

impl Default for HotwordFeed {
    fn default() -> Self {
        Self {
            inner: std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
                parking_lot::RwLock::new(FeedInner {
                    plan: HotwordPlan::default(),
                    tx: None,
                }),
            ))),
        }
    }
}

impl HotwordFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前内层句柄（读锁只在这一步持有，不与内层锁叠加）。
    fn cell(&self) -> std::sync::Arc<parking_lot::RwLock<FeedInner>> {
        self.inner.read().clone()
    }

    /// 当前词表方案。
    pub fn plan(&self) -> HotwordPlan {
        self.cell().read().plan.clone()
    }

    /// 订阅变更。provider 在会话开始时调用一次；拿到的是订阅那一刻的值
    /// （之后用 `borrow_and_update` 读取即可）。
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<HotwordPlan> {
        let cell = self.cell();
        let mut inner = cell.write();
        match inner.tx.as_ref() {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = tokio::sync::watch::channel(inner.plan.clone());
                inner.tx = Some(tx);
                rx
            }
        }
    }

    /// 写入新词表（管理页保存 / 管线启动时都会调用）。没有订阅者时只更新值，
    /// 下一次会话的 `run-task` 会带上。
    pub fn set(&self, plan: HotwordPlan) {
        let cell = self.cell();
        let mut inner = cell.write();
        inner.plan = plan.clone();
        if let Some(tx) = inner.tx.as_ref() {
            let _ = tx.send(plan);
        }
    }
    /// 让 `self` 变成 `other` 的别名：此后两边读写的是同一份 plan 与同一个
    /// watch 通道。provider 用它来使用 pipeline 的那一份 feed，这样管理页保存
    /// 走 `AppState.hotwords` 时，正在跑的会话一定收得到。
    pub fn adopt(&self, other: &HotwordFeed) {
        if std::sync::Arc::ptr_eq(&self.inner, &other.inner) {
            return;
        }
        // 先把自己已有的订阅者接到对方的值上，再整体换成对方的句柄。
        let other_plan = other.plan();
        let cell = self.cell();
        {
            let mut inner = cell.write();
            inner.plan = other_plan;
            if let Some(tx) = inner.tx.as_ref() {
                let _ = tx.send(inner.plan.clone());
            }
        }
        *self.inner.write() = other.cell();
    }
}
