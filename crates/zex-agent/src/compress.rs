//! 上下文压缩。
//!
//! 参数与保留原则来自 Zebra 的实测（`docs/research/inherited-design.md` §3），
//! 但实现刻意避开它的三个已知问题：
//!
//! 1. **不做双份 token 计量** —— Zebra 把 provider 回报的 total 赋值后又在上面
//!    累加本地计数，同一批 token 数两遍，导致提前压缩。
//! 2. **不用字符当预算** —— 中英文的字符/token 比差 4 倍，用字符等于惩罚中文。
//! 3. **摘要不伪装成用户消息** —— Zebra 把摘要包成 `UserMessage`，
//!    模型会把摘要里的历史指令当成用户当前要求。
//!
//! ## 结构
//!
//! `plan()` 是**纯函数**（无 IO），`apply()` 只做拼装。
//! 中间那次摘要 LLM 调用由调用方（agent）插入 —— 这样压缩逻辑可以完全离线测试。

use zex_core::{Message, ToolCallId};

use crate::context::{ContextBudget, estimate_history_tokens};

/// 压缩计划的种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressionPlan {
    /// 不需要压缩。
    None,
    /// 只截断过长的工具结果（最便宜的一层，先做）。
    ShrinkToolResults {
        /// 需要截断的消息下标。
        indices: Vec<usize>,
    },
    /// 把前 `drop_before` 条消息融进摘要，保留其余。
    Summarize {
        /// 丢弃 `[0, drop_before)` 区间的消息。
        drop_before: usize,
        /// 丢弃前的 token 估算。
        before_tokens: usize,
    },
}

/// 压缩器状态。
#[derive(Debug, Clone)]
pub struct Compressor {
    budget: ContextBudget,
    /// 滚雪球摘要（每次压缩在其基础上增量融合）。
    summary: Option<String>,
    /// 距上次压缩过了多少 turn。
    turns_since_compress: u32,
}

impl Compressor {
    /// 新建。
    pub fn new(budget: ContextBudget) -> Self {
        Compressor {
            budget,
            summary: None,
            turns_since_compress: u32::MAX,
        }
    }

    /// 当前摘要。
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// 记录一轮过去了（用于冷却判定）。
    pub fn tick_turn(&mut self) {
        self.turns_since_compress = self.turns_since_compress.saturating_add(1);
    }

    /// 预算参数。
    pub fn budget(&self) -> &ContextBudget {
        &self.budget
    }

    /// 根据当前消息与 provider 回报的用量决定要不要压缩。
    ///
    /// `provider_tokens` 是 API 回报的**权威值**。传 `None` 时用本地估算。
    /// **绝不把两者相加** —— 那正是 Zebra 的 bug。
    pub fn plan(&self, messages: &[Message], provider_tokens: Option<usize>) -> CompressionPlan {
        if messages.len() < self.budget.min_messages {
            return CompressionPlan::None;
        }

        let tokens = provider_tokens.unwrap_or_else(|| estimate_history_tokens(messages));
        let over = tokens >= self.budget.compress_limit();

        // 第一层：单条工具结果过长就先截断，不惊动 LLM。
        //
        // **只在整体已经够到压缩线时才做**。截断改的是历史中间的一段，
        // 它后面所有消息的 prefix cache 随之作废，下一次请求整段按未命中
        // 计费。早先这一层是无条件的：任何一条超过 1000 token 的工具结果
        // 都会重写一次历史，于是 192k / 1M 这种离天花板还差得远的时候，
        // 每步都刷一张「已压缩 193458 → 193328」—— 界面上是噪音，
        // 账上是白烧的缓存。
        if over {
            let indices = self.oversized_tool_results(messages);
            if !indices.is_empty() {
                return CompressionPlan::ShrinkToolResults { indices };
            }
        }

        // 冷却期内不主动压缩（紧急情况不受限）
        let cooling = self.turns_since_compress < self.budget.cooldown_turns;
        let emergency = tokens >= self.budget.emergency_limit();
        if !over || (cooling && !emergency) {
            return CompressionPlan::None;
        }

        let drop_before = self.find_boundary(messages);
        if drop_before == 0 {
            return CompressionPlan::None;
        }

        CompressionPlan::Summarize {
            drop_before,
            before_tokens: tokens,
        }
    }

    /// 找出「截了确实会变小」的工具结果下标。
    ///
    /// 已经截到下限的内容再截也是原样，若仍然按"超标"报，每一轮都会
    /// 重新触发一次压缩，于是界面上刷出一串「已压缩 14679 → 14678」。
    fn oversized_tool_results(&self, messages: &[Message]) -> Vec<usize> {
        let max = self.budget.tool_result_max_tokens;
        messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match m {
                Message::Tool { content, .. } => {
                    let before = crate::context::estimate_tokens(content);
                    if before <= max {
                        return None;
                    }
                    let after = crate::context::estimate_tokens(&truncate_middle(content, max));
                    (after < before).then_some(i)
                }
                _ => None,
            })
            .collect()
    }

    /// 找到"保留区"的起点。
    ///
    /// 优先保留 `keep_recent_turns` 个 assistant turn。但如果整个会话的 turn 数
    /// 还不够这个数，就不能直接放弃压缩 —— 否则一个有很多短 turn 的长会话
    /// 会永远卡在超预算状态。此时退而保留约 1/3，至少给压缩留出空间。
    ///
    /// 返回可安全丢弃的消息条数。**边界不能切在工具调用与结果之间**，
    /// 否则会产生孤立的 tool 消息，让 API 直接 400。
    fn find_boundary(&self, messages: &[Message]) -> usize {
        let total_turns = messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();

        // 少于 3 个 turn 没有压缩价值：摘要的固定开销会超过收益
        if total_turns < 3 {
            return 0;
        }

        // 降级：可用 turn 数不足配置值时，保留约 1/3，至少留 1 个
        let keep = if total_turns > self.budget.keep_recent_turns {
            self.budget.keep_recent_turns
        } else {
            (total_turns / 3).max(1)
        };

        let mut seen = 0usize;
        let mut boundary = 0usize;
        for (i, m) in messages.iter().enumerate().rev() {
            if matches!(m, Message::Assistant { .. }) {
                seen += 1;
                if seen >= keep {
                    boundary = i;
                    break;
                }
            }
        }
        if boundary == 0 {
            return 0;
        }
        // 向前调整边界，直到它不是一条孤立 tool 消息
        let mut b = boundary;
        while b > 0 && matches!(messages[b], Message::Tool { .. }) {
            b -= 1;
        }
        b
    }

    /// 应用截断计划（纯函数）。
    pub fn apply_shrink(messages: &[Message], indices: &[usize], budget: usize) -> Vec<Message> {
        let mut out = messages.to_vec();
        for &i in indices {
            if let Message::Tool {
                call_id,
                name,
                content,
                is_error,
            } = &out[i]
            {
                out[i] = Message::Tool {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    content: truncate_middle(content, budget),
                    is_error: *is_error,
                };
            }
        }
        out
    }

    /// 应用摘要计划：`[摘要消息] + [保留区]`，并修复工具配对。
    pub fn apply_summary(
        &mut self,
        messages: &[Message],
        drop_before: usize,
        summary: String,
    ) -> Vec<Message> {
        let kept = messages[drop_before.min(messages.len())..].to_vec();
        let mut out = Vec::with_capacity(kept.len() + 1);
        out.push(summary_message(&summary));
        out.extend(kept);
        repair_tool_pairs(&mut out);
        self.summary = Some(summary);
        self.turns_since_compress = 0;
        out
    }

    /// LLM 摘要失败时的降级方案：**永远返回可用摘要，不返回空。**
    ///
    /// 空摘要会让模型丢失全部前情，比摘要难看严重得多。
    pub fn fallback_summary(&self, dropped: &[Message]) -> String {
        let mut lines = vec!["（自动降级摘要：原摘要生成失败，以下为关键内容摘录）".to_string()];
        for m in dropped {
            let role = match m.role() {
                zex_core::Role::User => "用户",
                zex_core::Role::Assistant => "助手",
                zex_core::Role::Tool => "工具",
                zex_core::Role::System => "系统",
            };
            let text = m.text();
            let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let brief: String = first.chars().take(120).collect();
            if !brief.is_empty() {
                lines.push(format!("- [{role}] {brief}"));
            }
        }
        let mut s = lines.join("\n");
        // 粗略控制在预算内
        let max_chars = self.budget.summary_max_tokens * 2;
        if s.chars().count() > max_chars {
            s = s.chars().take(max_chars).collect();
        }
        s
    }

    /// 记录一次成功压缩。
    pub fn apply_summary_raw(&mut self, messages: &[Message], drop_before: usize) -> Vec<Message> {
        let kept = messages[drop_before.min(messages.len())..].to_vec();
        let mut out = Vec::with_capacity(kept.len() + 1);
        if let Some(s) = self.summary.clone() {
            out.push(summary_message(&s));
        }
        out.extend(kept);
        repair_tool_pairs(&mut out);
        self.turns_since_compress = 0;
        out
    }
}

/// 构造摘要消息。
///
/// 用 `System` + 显式标签，**不用 `User`** —— 摘要不是用户说的话，
/// 让它看起来像用户输入会诱导模型把历史指令当成当前要求。
pub fn summary_message(summary: &str) -> Message {
    Message::System {
        content: format!(
            "<context-summary>\n以下是本次会话较早内容的压缩摘要，供你保持连贯性。\n\
             它包含的是历史信息，不是用户刚刚提出的新要求。\n\n{summary}\n</context-summary>"
        ),
    }
}

/// 头部 + 尾部保留，中间省略。
///
/// 头尾都留：开头通常是"在做什么"，结尾通常是"结果是什么"。
/// 只留开头会丢掉结论，只留结尾会丢掉意图。
pub fn truncate_middle(content: &str, target_tokens: usize) -> String {
    let total = content.chars().count();
    let chars: Vec<char> = content.chars().collect();
    let cur = crate::context::estimate_tokens(content);
    if cur <= target_tokens {
        return content.to_string();
    }
    // 按「token → 字符」的实测比例反推要留多少字符。
    // 不能写死字符数：中文 1 字 1 token、英文 4 字符 1 token，
    // 写死 4000 字符在中文场景下等于 4000 token —— 比阈值还高，
    // 截完依然"超标"，于是每轮都重复触发。
    let keep = (total as f32 * target_tokens as f32 / cur as f32 * 0.9) as usize;
    let keep = keep.clamp(200, total);
    let head_len = keep * 2 / 3;
    let tail_len = keep / 3;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[total - tail_len..].iter().collect();
    format!(
        "{head}\n\n... [中间省略 {} 字符，原始共 {total} 字符] ...\n\n{tail}",
        total - head_len - tail_len
    )
}

/// 补全/清理工具调用配对，让历史满足协议要求。
///
/// # 为什么需要它
///
/// OpenAI 兼容的接口有一条硬性要求：**带 `tool_calls` 的 assistant 消息，
/// 后面必须跟上每条调用的结果**。少一条就是 400：
///
/// ```text
/// An assistant message with 'tool_calls' must be followed by tool messages
/// responding to each 'tool_call_id'. (insufficient tool messages following
/// tool_calls message)
/// ```
///
/// 而"少一条"很容易发生，都不需要谁写错代码：
///
/// - 用户点了停止 —— 走到第 2 个工具时中断，剩下 2 条没有结果；
/// - 进程被杀 / 崩溃 / 重启 —— assistant 已经落库，结果还没写；
/// - 上一轮报错退出。
///
/// 所以每次发请求之前都过一遍这里：**缺的补上、多的丢掉、重复的去掉**。
/// 补的是"未执行"的合成结果而不是删掉调用 —— 后者会让模型以为自己
/// 从没提过这个工具，用户看到的也是"它怎么不记得了"。
///
/// 返回被改动的位置条数（插入 + 删除），0 表示历史本来就是好的。
pub fn heal_tool_pairs(messages: &mut Vec<Message>) -> usize {
    use std::collections::HashSet;

    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut healed = 0usize;
    let mut i = 0;

    while i < messages.len() {
        match &messages[i] {
            Message::Assistant {
                content,
                tool_calls,
            } if !tool_calls.is_empty() => {
                let content = content.clone();
                let calls = tool_calls.clone();

                // 紧随其后的连续 Tool 消息就是这一批的结果
                let mut j = i + 1;
                let mut results: Vec<Message> = Vec::new();
                while j < messages.len() {
                    if matches!(&messages[j], Message::Tool { .. }) {
                        results.push(messages[j].clone());
                        j += 1;
                    } else {
                        break;
                    }
                }

                // 按调的**顺序**排结果：协议要求跟上，顺带让读历史的人
                // 能对上"第几个调用得了什么"
                let mut seen: HashSet<ToolCallId> = HashSet::new();
                let mut used = vec![false; results.len()];
                let mut ordered: Vec<Message> = Vec::with_capacity(calls.len());
                for call in &calls {
                    if !seen.insert(call.id.clone()) {
                        // 同一个调用声明了两次：只认第一条
                        healed += 1;
                        continue;
                    }
                    match results
                        .iter()
                        .enumerate()
                        .find(|(k, r)| {
                            !used[*k]
                                && matches!(r, Message::Tool { call_id, .. } if *call_id == call.id)
                        })
                        .map(|(k, _)| k)
                    {
                        Some(k) => {
                            used[k] = true;
                            ordered.push(results[k].clone());
                        }
                        None => {
                            ordered.push(Message::Tool {
                                call_id: call.id.clone(),
                                name: Some(call.name.clone()),
                                content: format!(
                                    "未执行：本轮在调用 `{}` 之前中断了，这条没有运行。",
                                    call.name
                                ),
                                is_error: true,
                            });
                            healed += 1;
                        }
                    }
                }
                // 不认识任何调用的结果（上一轮被砍掉的残留）直接丢
                healed += used.iter().filter(|u| !**u).count();

                out.push(Message::Assistant {
                    content,
                    tool_calls: calls,
                });
                out.extend(ordered);
                i = j;
            }
            // 前面没有 assistant 认领的结果：孤立，丢掉
            Message::Tool { .. } => {
                healed += 1;
                i += 1;
            }
            other => {
                out.push(other.clone());
                i += 1;
            }
        }
    }

    // 无条件写回：即使条数没变，顺序也可能被整理过（结果乱序 ->
    // 按调用顺序排），而那正是协议要的"紧跟各自的结果"
    *messages = out;
    healed
}

/// 修复工具调用配对：`tool_call.id` 与 `tool.call_id` 取交集。
///
/// **这是正确性要求，不是优化。** 压缩边界切在调用与结果之间时，
/// 孤立的任一侧都会让 API 返回 400（"tool_call_id not found"）。
pub fn repair_tool_pairs(messages: &mut Vec<Message>) {
    use std::collections::HashSet;

    let call_ids: HashSet<ToolCallId> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls.iter().map(|c| c.id.clone())),
            _ => None,
        })
        .flatten()
        .collect();

    let result_ids: HashSet<ToolCallId> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();

    messages.retain_mut(|m| {
        // 先算正文是否非空，避免与 retain_mut 的可变借用冲突
        let has_text = !m.text().trim().is_empty();
        match m {
            Message::Assistant { tool_calls, .. } => {
                // 去掉没有对应结果的调用
                tool_calls.retain(|c| result_ids.contains(&c.id));
                // 调用被清空且没有正文，这条 assistant 消息就没意义了
                !tool_calls.is_empty() || has_text
            }
            Message::Tool { call_id, .. } => call_ids.contains(call_id),
            _ => true,
        }
    });
}

/// 生成摘要提示词。
pub fn summary_prompt(previous: Option<&str>, dropped: &[Message], max_tokens: usize) -> String {
    let mut buf = String::new();
    if let Some(prev) = previous {
        buf.push_str("已有摘要（请在此基础上增量更新，不要丢掉仍然有效的信息）：\n");
        buf.push_str(prev);
        buf.push_str("\n\n");
    }
    buf.push_str("需要并入摘要的对话片段：\n");
    for m in dropped {
        let role = match m.role() {
            zex_core::Role::User => "用户",
            zex_core::Role::Assistant => "助手",
            zex_core::Role::Tool => "工具",
            zex_core::Role::System => "系统",
        };
        buf.push_str(&format!("[{role}] {}\n", m.text()));
    }
    buf.push_str(&format!(
        "\n请输出一份不超过 {max_tokens} token 的摘要。必须保留：\n\
         1. 文件路径、函数名、变量名等具体标识符（绝对不能丢失或改写）\n\
         2. 已做出的关键决策及其原因\n\
         3. 遇到的错误及解决办法\n\
         4. 当前进度（已完成 / 进行中 / 待完成）\n\
         可以压缩过程描述（\"正在读取文件\" → \"已读取\"），但不得压缩事实。\n\
         直接输出摘要正文，不要加前言。"
    ));
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use zex_core::{Part, ToolCall, ToolCallId};

    fn budget() -> ContextBudget {
        ContextBudget {
            min_messages: 2,
            ..Default::default()
        }
    }

    fn call(id: &str) -> zex_core::ToolCall {
        zex_core::ToolCall {
            id: ToolCallId::new(id),
            name: "fs_read".into(),
            arguments: "{}".into(),
        }
    }

    fn asst(text: &str) -> Message {
        Message::Assistant {
            content: vec![Part::text(text)],
            tool_calls: vec![],
        }
    }

    #[test]
    fn no_compression_for_short_history() {
        let c = Compressor::new(ContextBudget::default());
        let msgs = vec![Message::user("hi")];
        assert_eq!(c.plan(&msgs, None), CompressionPlan::None);
    }

    #[test]
    fn oversized_tool_result_is_shrunk_first() {
        // 截断只在够到压缩线之后才做：窗口 6000 → 压缩线 4200，
        // 而 20000 个 ASCII 字符约 5000 token，已在线上。
        let mut b = budget();
        b.window = 6_000;
        let c = Compressor::new(b);
        let big = "x".repeat(20_000);
        let msgs = vec![
            Message::user("go"),
            asst("ok"),
            Message::Tool {
                call_id: ToolCallId::new("c1"),
                name: Some("shell".into()),
                content: big,
                is_error: false,
            },
        ];
        match c.plan(&msgs, None) {
            CompressionPlan::ShrinkToolResults { indices } => assert_eq!(indices, vec![2]),
            other => panic!("期望先截断工具结果，得到 {other:?}"),
        }
    }

    #[test]
    fn far_from_limit_leaves_history_alone() {
        // 回归：曾经只要有一条工具结果超 1000 token 就重写历史，
        // 即使总量才 5k / 1M。每次重写都会让后面所有消息的 prefix
        // cache 失效，界面上还每步刷一张「已压缩 N → N-130」。
        let mut b = budget();
        b.window = 1_000_000;
        let c = Compressor::new(b);
        let big = "x".repeat(20_000);
        let msgs = vec![
            Message::user("go"),
            asst("ok"),
            Message::Tool {
                call_id: ToolCallId::new("c1"),
                name: Some("shell".into()),
                content: big,
                is_error: false,
            },
        ];
        assert_eq!(c.plan(&msgs, None), CompressionPlan::None);
    }

    #[test]
    fn shrink_keeps_head_and_tail() {
        let big: String = (0..10_000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let msgs = vec![Message::Tool {
            call_id: ToolCallId::new("c"),
            name: None,
            content: big.clone(),
            is_error: false,
        }];
        let out = Compressor::apply_shrink(&msgs, &[0], 1000);
        let text = out[0].text();
        assert!(text.len() < big.len());
        assert!(text.contains("中间省略"));
        assert!(text.starts_with('a'));
    }

    #[test]
    fn shrink_lands_under_the_trigger_threshold() {
        // 回归：截断目标曾经写死 4000 字符，而中文 1 字≈1 token，
        // 截完是 4000 token —— 仍高于 1000 的阈值，于是每轮重新触发，
        // 界面上刷出一串「已压缩 N → N-1」的零收益提示。
        let big: String = "中".repeat(8000);
        let msgs = vec![Message::Tool {
            call_id: ToolCallId::new("c"),
            name: None,
            content: big.clone(),
            is_error: false,
        }];
        let max = ContextBudget::default().tool_result_max_tokens;
        let out = Compressor::apply_shrink(&msgs, &[0], max);
        let after = crate::context::estimate_tokens(&out[0].text());
        assert!(after <= max, "截断后应落在阈值内，实际 {after} > {max}");
    }

    #[test]
    fn already_shrunk_result_does_not_retrigger() {
        // 窗口 10000 → 压缩线 7000；8000 个汉字约 8000 token，在线上。
        let c = Compressor::new(ContextBudget {
            min_messages: 2,
            window: 10_000,
            ..Default::default()
        });
        let big: String = "中".repeat(8000);
        let mut msgs = vec![Message::User {
            content: vec![zex_core::Part::Text {
                text: "开始".into(),
            }],
        }];
        for i in 0..4 {
            msgs.push(Message::Assistant {
                content: vec![zex_core::Part::Text {
                    text: format!("第 {i} 步"),
                }],
                tool_calls: vec![],
            });
        }
        msgs.push(Message::Tool {
            call_id: ToolCallId::new("c"),
            name: None,
            content: big,
            is_error: false,
        });

        // 第一次：应当要求截断
        let indices = match c.plan(&msgs, None) {
            CompressionPlan::ShrinkToolResults { indices } => indices,
            other => panic!("应触发截断，得到 {other:?}"),
        };
        let max = ContextBudget::default().tool_result_max_tokens;
        let shrunk = Compressor::apply_shrink(&msgs, &indices, max);

        // 第二次：已经截过了，不能再报同一批
        match c.plan(&shrunk, None) {
            CompressionPlan::ShrinkToolResults { .. } => {
                panic!("已截断的内容不应重复触发压缩")
            }
            _ => {}
        }
    }

    #[test]
    fn heal_fills_in_missing_tool_results() {
        // 用户在第 2 个工具之前点了停止 / 进程被杀 → assistant 有 2 条调用，
        // 但只有第 1 条的结果。这条历史直接发出去，接口会 400。
        let mut msgs = vec![
            Message::user("看看这两个文件"),
            Message::Assistant {
                content: vec![Part::text("我读一下")],
                tool_calls: vec![call("a"), call("b")],
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: Some("fs_read".into()),
                content: "文件 A".into(),
                is_error: false,
            },
        ];
        let healed = heal_tool_pairs(&mut msgs);
        assert_eq!(healed, 1, "只该补一条");
        assert_eq!(msgs.len(), 4);
        match &msgs[3] {
            Message::Tool {
                call_id,
                is_error,
                content,
                ..
            } => {
                assert_eq!(*call_id, ToolCallId::new("b"));
                assert!(is_error, "补的这条必须标成失败，否则模型以为它跑过了");
                assert!(content.contains("未执行"), "要写明原因：{content}");
            }
            other => panic!("第三条应该是补出来的工具结果，得到 {other:?}"),
        }
    }

    #[test]
    fn heal_keeps_results_in_call_order() {
        // 结果乱序（并发场景下会出现）：按调用顺序重排，
        // 顺带让顺序满足"紧跟其后"的协议要求
        let mut msgs = vec![
            Message::Assistant {
                content: vec![],
                tool_calls: vec![call("a"), call("b")],
            },
            Message::Tool {
                call_id: ToolCallId::new("b"),
                name: Some("x".into()),
                content: "B".into(),
                is_error: false,
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: Some("x".into()),
                content: "A".into(),
                is_error: false,
            },
        ];
        assert_eq!(heal_tool_pairs(&mut msgs), 0, "条数没变，不算修补");
        let ids: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                Message::Tool { call_id, .. } => Some(call_id.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn heal_drops_orphan_and_duplicate_results() {
        let mut msgs = vec![
            // 前面没有 assistant 认领
            Message::Tool {
                call_id: ToolCallId::new("ghost"),
                name: None,
                content: "孤立的".into(),
                is_error: false,
            },
            Message::Assistant {
                content: vec![Part::text("ok")],
                tool_calls: vec![call("a")],
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: None,
                content: "第一次".into(),
                is_error: false,
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: None,
                content: "重复的".into(),
                is_error: false,
            },
        ];
        let healed = heal_tool_pairs(&mut msgs);
        assert_eq!(healed, 2, "丢一条孤立 + 一条重复");
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], Message::Assistant { .. }));
        match &msgs[1] {
            Message::Tool { content, .. } => assert_eq!(content, "第一次"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn heal_is_a_no_op_on_a_healthy_history() {
        let mut msgs = vec![
            Message::user("跑一下"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![call("a")],
            },
            Message::Tool {
                call_id: ToolCallId::new("a"),
                name: None,
                content: "结果".into(),
                is_error: false,
            },
            Message::Assistant {
                content: vec![Part::text("好了")],
                tool_calls: vec![],
            },
        ];
        let before = msgs.clone();
        assert_eq!(heal_tool_pairs(&mut msgs), 0);
        assert_eq!(msgs, before);
    }

    #[test]
    fn summarize_triggers_over_threshold() {
        let mut b = budget();
        b.window = 100;
        b.compress_at = 0.7;
        let c = Compressor::new(b);
        // 造足够多的 turn 且总 token 超阈值
        let mut msgs = Vec::new();
        for i in 0..20 {
            msgs.push(Message::user(format!(
                "question number {i} with some padding text"
            )));
            msgs.push(asst(&format!(
                "answer {i} also with padding to inflate tokens"
            )));
        }
        match c.plan(&msgs, None) {
            CompressionPlan::Summarize { drop_before, .. } => {
                assert!(drop_before > 0);
                assert!(drop_before < msgs.len());
            }
            other => panic!("期望触发摘要，得到 {other:?}"),
        }
    }

    #[test]
    fn provider_tokens_authoritative_not_added() {
        // 核心：provider 给的数字就是权威值，不与估算相加
        let mut b = budget();
        b.window = 1000;
        let c = Compressor::new(b);
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..8 {
            msgs.push(Message::user(format!("u{i} padding text here")));
            msgs.push(asst(&format!("a{i} padding text here")));
        }
        // 估算远小于阈值，但 provider 说已经满了 → 应该按 provider 判断
        assert_eq!(c.plan(&msgs, Some(10)), CompressionPlan::None);
        assert_ne!(c.plan(&msgs, Some(900)), CompressionPlan::None);
    }

    #[test]
    fn cooldown_suppresses_non_emergency() {
        let mut b = budget();
        b.window = 100;
        b.cooldown_turns = 3;
        let mut c = Compressor::new(b);
        c.turns_since_compress = 0; // 刚压缩过
        let mut msgs: Vec<Message> = Vec::new();
        for i in 0..10 {
            msgs.push(Message::user(format!("u{i} padding text")));
            msgs.push(asst(&format!("a{i} padding text")));
        }
        // 超过压缩线但在冷却期 → 不压缩（未达紧急线）
        assert_eq!(c.plan(&msgs, Some(75)), CompressionPlan::None);
        // 达紧急线 → 冷却不管用
        assert_ne!(c.plan(&msgs, Some(95)), CompressionPlan::None);
    }

    #[test]
    fn summary_message_is_system_not_user() {
        // 摘要绝不能伪装成用户消息
        let m = summary_message("过去的对话");
        assert!(matches!(m, Message::System { .. }));
        assert!(m.text().contains("<context-summary>"));
        assert!(m.text().contains("不是用户刚刚提出的新要求"));
    }

    #[test]
    fn apply_summary_prepends_and_keeps_tail() {
        let mut c = Compressor::new(budget());
        let msgs = vec![
            Message::user("old1"),
            asst("old2"),
            Message::user("recent1"),
            asst("recent2"),
        ];
        let out = c.apply_summary(&msgs, 2, "摘要内容".into());
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], Message::System { .. }));
        assert_eq!(out[1].text(), "recent1");
        assert_eq!(c.summary(), Some("摘要内容"));
        assert_eq!(c.turns_since_compress, 0);
    }

    #[test]
    fn fallback_summary_is_never_empty() {
        let c = Compressor::new(budget());
        let dropped = vec![Message::user("something important")];
        let s = c.fallback_summary(&dropped);
        assert!(!s.trim().is_empty());
        assert!(s.contains("something important"));
    }

    #[test]
    fn fallback_summary_handles_empty_input() {
        // 即使输入为空也要有内容，否则上层会写入一条空摘要
        let c = Compressor::new(budget());
        let s = c.fallback_summary(&[]);
        assert!(!s.trim().is_empty());
    }

    // ─── 工具配对修复 ───

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(id),
            name: "t".into(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn repair_drops_orphan_tool_result() {
        let mut msgs = vec![
            Message::Tool {
                call_id: ToolCallId::new("orphan"),
                name: None,
                content: "x".into(),
                is_error: false,
            },
            Message::user("hi"),
        ];
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "hi");
    }

    #[test]
    fn repair_drops_orphan_tool_call() {
        let mut msgs = vec![Message::Assistant {
            content: vec![Part::text("doing")],
            tool_calls: vec![tool_call("no-result")],
        }];
        repair_tool_pairs(&mut msgs);
        // 有正文，消息保留，但调用被清掉
        match &msgs[0] {
            Message::Assistant { tool_calls, .. } => assert!(tool_calls.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn repair_keeps_matched_pairs() {
        let mut msgs = vec![
            Message::Assistant {
                content: vec![],
                tool_calls: vec![tool_call("c1")],
            },
            Message::Tool {
                call_id: ToolCallId::new("c1"),
                name: Some("t".into()),
                content: "ok".into(),
                is_error: false,
            },
        ];
        repair_tool_pairs(&mut msgs);
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn repair_drops_empty_assistant_without_calls() {
        let mut msgs = vec![Message::Assistant {
            content: vec![],
            tool_calls: vec![tool_call("gone")],
        }];
        repair_tool_pairs(&mut msgs);
        assert!(
            msgs.is_empty(),
            "空正文且调用全被去掉的 assistant 消息应删除"
        );
    }

    #[test]
    fn boundary_does_not_split_tool_pair() {
        let mut b = budget();
        b.keep_recent_turns = 1;
        let c = Compressor::new(b);
        let msgs = vec![
            Message::user("u1"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![tool_call("c1")],
            },
            Message::Tool {
                call_id: ToolCallId::new("c1"),
                name: None,
                content: "r1".into(),
                is_error: false,
            },
            asst("last"),
        ];
        let boundary = c.find_boundary(&msgs);
        // 边界不能是 tool 消息（下标 2）
        assert_ne!(boundary, 2);
    }

    #[test]
    fn summary_prompt_contains_retention_rules() {
        let p = summary_prompt(None, &[Message::user("hi")], 800);
        assert!(p.contains("文件路径"));
        assert!(p.contains("不得压缩事实"));
        assert!(p.contains("800"));
    }

    #[test]
    fn summary_prompt_fuses_previous() {
        let p = summary_prompt(Some("旧摘要"), &[Message::user("hi")], 800);
        assert!(p.contains("旧摘要"));
        assert!(p.contains("增量更新"));
    }

    #[test]
    fn truncate_middle_short_text_untouched() {
        assert_eq!(truncate_middle("short", 1000), "short");
    }
}
