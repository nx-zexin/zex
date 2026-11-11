//! 重试与退避策略。
//!
//! 原则（`docs/ARCHITECTURE.md` §4）：**重试只覆盖"拿到 2xx 之前"的失败。**
//! 流已经开始就不能重试 —— 否则会重复输出已经发给用户的 token。

use std::future::Future;
use std::time::Duration;

use zex_core::{Error, Result};

/// 最多尝试次数（含首次）。
pub const MAX_ATTEMPTS: u32 = 4;

/// 首次退避基数。
pub const BASE_DELAY: Duration = Duration::from_millis(500);

/// 退避上限。
pub const MAX_DELAY: Duration = Duration::from_secs(20);

/// 计算第 `attempt` 次重试前应等待多久（`attempt` 从 1 开始）。
///
/// 指数退避 + 抖动：抖动是为了避免多个并发请求在同一时刻齐步重试，
/// 把刚在恢复的服务端重新打回限流。
pub fn backoff_delay(attempt: u32, server_hint: Option<Duration>) -> Duration {
    // 服务端明确说了等多久就听它的，但要封顶 —— 否则一个异常值能把程序挂死
    if let Some(hint) = server_hint {
        return hint.min(MAX_DELAY);
    }
    let shift = attempt.saturating_sub(1).min(6);
    let exp = BASE_DELAY.saturating_mul(1u32 << shift).min(MAX_DELAY);

    // 抖动 0.5x ~ 1.5x
    let millis = exp.as_millis() as u64;
    let jittered = millis / 2 + (pseudo_random() % millis.max(1));
    // 抖动后必须再封一次顶：1.5 × MAX_DELAY 会突破上限
    Duration::from_millis(jittered).min(MAX_DELAY)
}

/// 便宜的伪随机（不引 rand 依赖）。
fn pseudo_random() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as u64) ^ (d.as_secs() << 13))
        .unwrap_or(12345)
}

/// 检查响应状态码，把非 2xx 翻译成带重试语义的错误。
///
/// 2xx 原样返回。429 / 5xx 标记为可重试，其余为致命错误。
/// 429 的 `Retry-After` 会被记进日志（退避仍走默认策略，因为 `Error`
/// 不带 hint 字段；把 hint 塞进错误类型是 M1 的改进点）。
pub async fn check_status(url: &str, resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let code = status.as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());

    // 必须读掉 body：既为了给出可诊断的错误，也为了连接能被复用
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(500).collect();

    if let Some(secs) = retry_after {
        tracing::warn!(url, code, retry_after_secs = secs, "服务端要求延后重试");
    }

    let message = format!("{url} 返回 {code}: {snippet}");
    let retryable = code == 429 || (500..600).contains(&code);

    if retryable {
        Err(Error::llm_retryable(message, Some(code)))
    } else {
        Err(Error::llm_fatal(message, Some(code)))
    }
}

/// 带重试地执行一个"建立连接"的异步操作。
///
/// `op` 必须幂等 —— 这里只用于发起请求，绝不用于流中途续传。
pub async fn with_retry<F, Fut, T>(url: &str, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !e.is_retryable() || attempt >= MAX_ATTEMPTS {
                    return Err(e);
                }
                let delay = backoff_delay(attempt, None);
                tracing::warn!(
                    url = %url,
                    attempt,
                    delay_ms = delay.as_millis(),
                    error = %e,
                    "请求失败，将重试"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        for a in 1..12 {
            let d = backoff_delay(a, None);
            assert!(
                d <= MAX_DELAY,
                "attempt {a} 退避 {} 超过上限",
                d.as_millis()
            );
        }
    }

    #[test]
    fn backoff_respects_server_hint() {
        let d = backoff_delay(1, Some(Duration::from_secs(3)));
        assert_eq!(d, Duration::from_secs(3));
    }

    #[test]
    fn backoff_caps_absurd_server_hint() {
        // 服务端说等 1 小时，我们不能真等 1 小时
        let d = backoff_delay(1, Some(Duration::from_secs(3600)));
        assert_eq!(d, MAX_DELAY);
    }

    #[test]
    fn backoff_grows_with_attempt() {
        // 抖动存在，比较区间而非精确值
        let a1 = backoff_delay(1, None).as_millis();
        let a5 = backoff_delay(5, None).as_millis();
        assert!(a5 > a1, "退避应随尝试次数增长: a1={a1} a5={a5}");
    }

    #[test]
    fn backoff_never_zero() {
        // 立即重试会打爆服务端
        for a in 1..8 {
            assert!(backoff_delay(a, None) > Duration::ZERO);
        }
    }

    #[tokio::test]
    async fn with_retry_stops_on_fatal_error() {
        let mut calls = 0;
        let res: Result<()> = with_retry("test", || {
            calls += 1;
            async { Err(Error::llm_fatal("bad request", Some(400))) }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls, 1, "fatal 错误不应重试，实际调用 {calls} 次");
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_max_attempts() {
        let mut calls = 0;
        let res: Result<()> = with_retry("test", || {
            calls += 1;
            async { Err(Error::llm_retryable("rate limited", Some(429))) }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(calls, MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn with_retry_succeeds_after_transient_failure() {
        let mut calls = 0;
        let res: Result<u32> = with_retry("test", || {
            calls += 1;
            let n = calls;
            async move {
                if n < 2 {
                    Err(Error::llm_retryable("boom", Some(503)))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(res.unwrap_or(0), 42);
        assert_eq!(calls, 2);
    }
}
