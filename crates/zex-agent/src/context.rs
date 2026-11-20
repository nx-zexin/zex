//! 上下文预算与 token 估算。
//!
//! M0 只提供**参数与估算**，压缩逻辑在 M1 接入。
//! 参数取自 Zebra 的实测经验（见 `docs/research/inherited-design.md` §3）。
//!
//! 三条不可违背的规则：
//! 1. 一个 turn 内只允许一个 token 数 —— 优先 provider 回报的 `usage`，
//!    没有就全量重算。**绝不累加**（Zebra 累加导致提前压缩）。
//! 2. 截断与预算按 token 算，字符只用于 UI 显示。
//! 3. 压缩后必须修复工具调用配对。

use zex_core::{Message, Part};

/// 上下文预算参数。
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// 模型上下文窗口（token）。
    pub window: usize,
    /// 达到该比例触发压缩。
    pub compress_at: f32,
    /// 达到该比例走紧急压缩（跳过冷却）。
    pub emergency_at: f32,
    /// 保留最近多少个 assistant turn 的原文。
    pub keep_recent_turns: usize,
    /// 摘要本身的目标长度。
    pub summary_max_tokens: usize,
    /// 两次压缩之间至少隔多少 turn。
    pub cooldown_turns: u32,
    /// 单条工具结果的 token 上限，超出则截断。
    ///
    /// 由 [`ContextBudget::with_output_tokens`] 按模型的 `max_tokens` 改写，
    /// 这里的 1000 只是“没接上模型参数”时的兜底值（也是地板）。
    pub tool_result_max_tokens: usize,
    /// 消息数少于该值不做压缩。
    pub min_messages: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        ContextBudget {
            // 保守默认：大多数现代模型的窗口都远大于此，
            // 真正的窗口值应在 M1 从模型元数据读取。
            window: 128_000,
            compress_at: 0.70,
            emergency_at: 0.85,
            keep_recent_turns: 10,
            summary_max_tokens: 800,
            cooldown_turns: 3,
            // Zebra 用 3000 字符；这里改成 token，约为其 1/3 的信息量。
            // 用 token 是因为中文的字符/token 比与英文差 4 倍，
            // 用字符当预算等于对中文惩罚 4 倍。
            tool_result_max_tokens: 1000,
            min_messages: 6,
        }
    }
}

impl ContextBudget {
    /// 按模型名给一份预算。
    ///
    /// 只对**查得到官方数字**的模型调窗口，其余一律走保守默认：
    /// 窗口估大了会撞 API 报错，估小了会过早压缩、白丢上下文 ——
    /// 两个方向的错都难受，所以宁可不猜。
    pub fn for_model(model: &str) -> Self {
        let mut b = Self::default();
        if let Some(w) = known_window(model) {
            b.window = w;
        }
        b
    }

    /// 压缩阈值对应的绝对 token 数。
    pub fn compress_limit(&self) -> usize {
        (self.window as f32 * self.compress_at) as usize
    }

    /// 紧急压缩阈值对应的绝对 token 数。
    pub fn emergency_limit(&self) -> usize {
        (self.window as f32 * self.emergency_at) as usize
    }

    /// 让工具结果预算跟着模型的单次输出上限走。
    ///
    /// # 为什么要关联 `max_tokens`
    ///
    /// `max_tokens` 是“模型一次能吃掉多少”的唯一现成读数。一条工具结果
    /// 比它还大，等于逼模型在一堆自己消化不了的内容里找重点；反过来，
    /// 用户把上限调到 32k 就是想看长输出，这时还按 1000 token 砍工具结果
    /// 纯属帮倒忙。所以不另造一个旋钮，直接跟它走。
    ///
    /// 上下限只是防护：`max_tokens = 64` 这种极端配置不该把每条结果
    /// 都砍成残包（下限 1000，也是历史默认值）；而真的填了 512k 也不是
    /// “能不能读懂”的问题了，是往窗口里灌 —— 封顶在 64k。
    pub fn with_output_tokens(mut self, max_tokens: u32) -> Self {
        self.tool_result_max_tokens =
            (max_tokens as usize).clamp(TOOL_RESULT_FLOOR_TOKENS, TOOL_RESULT_CEILING_TOKENS);
        self
    }
}

/// 工具结果预算的地板：低于这个值，正常的一条 `read` 输出会被砍成残包。
pub const TOOL_RESULT_FLOOR_TOKENS: usize = 1_000;

/// 工具结果预算的天花板：再大就不是“读不读得懂”，而是往窗口里灌水。
pub const TOOL_RESULT_CEILING_TOKENS: usize = 65_536;

/// 估算一段文本的 token 数。
///
/// 分档估算（来自 Zebra 的实测口径）：
/// - CJK 字符：1.0 token/字
/// - ASCII 字母与数字：0.25 token/字（约 4 字符 1 token）
/// - 其他（符号、空白、其他文字）：0.6 token/字
///
/// 这是估算，不是精确值。M1 会换成 tiktoken；在那之前，
/// 用它做"是否接近上限"的判断已经足够，因为它偏保守。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut ascii = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else if c.is_ascii_alphanumeric() || c.is_ascii_whitespace() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    // 用整数运算避免浮点误差：cjk*1.0 + ascii*0.25 + other*0.6
    // → 放大 20 倍后取整再除
    let scaled = cjk * 20 + ascii * 5 + other * 12;
    scaled.div_ceil(20)
}

/// 粗略判断是否是 CJK 字符。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF      // CJK 统一表意文字
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x3000..=0x303F    // CJK 标点
        | 0xFF00..=0xFFEF    // 全角字符
        | 0x3040..=0x30FF    // 日文假名
        | 0xAC00..=0xD7AF    // 韩文
    )
}

/// 单条消息的 token 估算（含固定开销）。
///
/// 固定开销 30 token 是消息格式（role、分隔符）的经验值。
pub fn estimate_message_tokens(msg: &Message) -> usize {
    const OVERHEAD: usize = 30;
    let body = match msg {
        Message::System { content } => estimate_tokens(content),
        Message::User { content } | Message::Assistant { content, .. } => content
            .iter()
            .map(|p| match p {
                Part::Text { text } | Part::Thinking { text } => estimate_tokens(text),
                Part::Image(_) => 800, // 图片按低分辨率档估
            })
            .sum(),
        Message::Tool { content, .. } => estimate_tokens(content),
    };
    body + OVERHEAD
}

/// 整个历史的 token 估算。
pub fn estimate_history_tokens(msgs: &[Message]) -> usize {
    msgs.iter().map(estimate_message_tokens).sum()
}

#[cfg(test)]
mod tests {
    #[test]
    fn budget_follows_the_model_window() {
        // DeepSeek V4 是 1M 上下文，按 128K 压缩等于每轮白扔上下文
        assert_eq!(
            super::ContextBudget::for_model("deepseek-flash").window,
            1_000_000
        );
        assert_eq!(
            super::ContextBudget::for_model("deepseek-v4-pro").window,
            1_000_000
        );
        // 不知道的模型不许瞎猜，保持保守默认
        assert_eq!(
            super::ContextBudget::for_model("some-local-llama").window,
            128_000
        );
    }

    use super::*;
    use zex_core::{Role, ToolCallId};

    #[test]
    fn tool_result_budget_follows_the_output_limit() {
        // 关联的意义：用户把单次输出从 2048 调到 32768，就是想看长内容，
        // 工具结果不该还按 1000 token 砍。
        let b = ContextBudget::default().with_output_tokens(2048);
        assert_eq!(b.tool_result_max_tokens, 2048);
        let b = ContextBudget::default().with_output_tokens(32_768);
        assert_eq!(b.tool_result_max_tokens, 32_768);
    }

    #[test]
    fn tool_result_budget_is_clamped() {
        // 极端的 max_tokens 不该把工具结果砍成残包，也不该往窗口里灌水
        let tiny = ContextBudget::default().with_output_tokens(64);
        assert_eq!(tiny.tool_result_max_tokens, TOOL_RESULT_FLOOR_TOKENS);
        let huge = ContextBudget::default().with_output_tokens(512_000);
        assert_eq!(huge.tool_result_max_tokens, TOOL_RESULT_CEILING_TOKENS);
    }

    #[test]
    fn default_budget_limits_are_consistent() {
        let b = ContextBudget::default();
        assert!(b.compress_limit() < b.emergency_limit());
        assert!(b.emergency_limit() <= b.window);
    }

    #[test]
    fn english_is_about_four_chars_per_token() {
        let t = estimate_tokens("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
        // 52 个 ASCII 字符 ≈ 13 token
        assert!((10..=16).contains(&t), "得到 {t}");
    }

    #[test]
    fn chinese_is_about_one_token_per_char() {
        let t = estimate_tokens("这是一段中文文本用于测试");
        // 12 个汉字 ≈ 12 token
        assert!((10..=14).contains(&t), "得到 {t}");
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn mixed_text_is_between() {
        let en = estimate_tokens("aaaaaaaaaaaaaaaaaaaa");
        let zh = estimate_tokens("啊啊啊啊啊啊啊啊啊啊");
        let mixed = estimate_tokens("aaaaa啊啊啊啊啊啊aaa");
        assert!(mixed >= en / 2);
        assert!(mixed <= zh * 2);
    }

    #[test]
    fn message_overhead_is_counted() {
        let msg = Message::user("hi");
        assert!(estimate_message_tokens(&msg) >= 30);
    }

    #[test]
    fn history_accumulates() {
        let msgs = vec![Message::user("a"), Message::system("b"), Message::user("c")];
        let total = estimate_history_tokens(&msgs);
        assert!(total >= 90, "得到 {total}");
    }

    #[test]
    fn tool_message_uses_content() {
        let msg = Message::Tool {
            call_id: ToolCallId::new("c"),
            name: None,
            content: "x".repeat(400),
            is_error: false,
        };
        let t = estimate_message_tokens(&msg);
        assert!(t > 100, "得到 {t}");
    }

    #[test]
    fn roles_do_not_affect_estimate_much() {
        // role 不同但正文相同，估算应接近
        let a = estimate_message_tokens(&Message::user("hello world"));
        let b = estimate_message_tokens(&Message::system("hello world"));
        assert_eq!(a, b);
        assert_eq!(Message::user("x").role(), Role::User);
    }
}

/// 已知模型的上下文窗口（token）。
///
/// 来源：DeepSeek 官方定价页（2026-11）标注 deepseek-flash / deepseek-v4-pro
/// 上下文 1M、输出上限 384K。其余 provider 的数字没查到出处，保持默认 128K。
fn known_window(model: &str) -> Option<usize> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("deepseek-flash") || m.starts_with("deepseek-v4-pro") {
        return Some(1_000_000);
    }
    None
}
