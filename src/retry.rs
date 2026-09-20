use crate::config::RateLimitConfig;
use http::header::RETRY_AFTER;
use http::HeaderMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAX_RATE_LIMIT_RETRIES: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitSource {
    RetryAfter,
    RateLimitReset,
    RateLimitResetMs,
    Backoff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryWait {
    pub delay: Duration,
    pub raw_delay: Duration,
    pub clamped: bool,
    pub source: WaitSource,
}

pub fn select_retry_wait(
    headers: &HeaderMap,
    retry_index: u32,
    config: &RateLimitConfig,
    now: SystemTime,
) -> RetryWait {
    if let Some(delay) = retry_after(headers, now) {
        return retry_wait(delay, config.max_sleep, WaitSource::RetryAfter);
    }

    if let Some(delay) = epoch_seconds_header(headers, "x-ratelimit-reset", now) {
        return retry_wait(delay, config.max_sleep, WaitSource::RateLimitReset);
    }

    if let Some(delay) = epoch_millis_header(headers, "x-ratelimit-reset-ms", now) {
        return retry_wait(delay, config.max_sleep, WaitSource::RateLimitResetMs);
    }

    let delay = fallback_delay(retry_index, config);
    retry_wait(delay, config.max_sleep, WaitSource::Backoff)
}

pub fn retry_budget_exceeded(elapsed: Duration, next_delay: Duration, budget: Duration) -> bool {
    elapsed.saturating_add(next_delay) >= budget
}

fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let when = httpdate::parse_http_date(raw).ok()?;
    duration_until(when, now)
}

fn epoch_seconds_header(
    headers: &HeaderMap,
    name: &'static str,
    now: SystemTime,
) -> Option<Duration> {
    let seconds = headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let when = UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    duration_until(when, now)
}

fn epoch_millis_header(
    headers: &HeaderMap,
    name: &'static str,
    now: SystemTime,
) -> Option<Duration> {
    let millis = headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let when = UNIX_EPOCH.checked_add(Duration::from_millis(millis))?;
    duration_until(when, now)
}

fn duration_until(when: SystemTime, now: SystemTime) -> Option<Duration> {
    when.duration_since(now).ok()
}

fn fallback_delay(retry_index: u32, config: &RateLimitConfig) -> Duration {
    let exponent = i32::try_from(retry_index).unwrap_or(i32::MAX);
    let multiplier = config.backoff_multiplier.powi(exponent);
    let jitter = f64::from(fastrand::u32(500..=1500)) / 1000.0;
    let millis = (config.initial_backoff.as_millis() as f64 * multiplier * jitter).round();
    let millis = millis.max(1.0).min(u64::MAX as f64) as u64;
    clamp(Duration::from_millis(millis), config.max_sleep)
}

fn retry_wait(raw_delay: Duration, max_sleep: Duration, source: WaitSource) -> RetryWait {
    let delay = clamp(raw_delay, max_sleep);
    RetryWait {
        delay,
        raw_delay,
        clamped: raw_delay > delay,
        source,
    }
}

fn clamp(value: Duration, max: Duration) -> Duration {
    if value > max {
        max
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn config() -> RateLimitConfig {
        RateLimitConfig {
            max_total_wait: Duration::from_millis(10),
            max_sleep: Duration::from_millis(5),
            initial_backoff: Duration::from_millis(2),
            backoff_multiplier: 2.0,
        }
    }

    #[test]
    fn retry_after_seconds_wins_and_is_clamped() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("10"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("9999999999"));

        let wait = select_retry_wait(&headers, 0, &config(), UNIX_EPOCH);

        assert_eq!(wait.source, WaitSource::RetryAfter);
        assert_eq!(wait.delay, Duration::from_millis(5));
    }

    #[test]
    fn x_ratelimit_reset_is_used_when_retry_after_is_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("2"));

        let wait = select_retry_wait(&headers, 0, &config(), UNIX_EPOCH);

        assert_eq!(wait.source, WaitSource::RateLimitReset);
        assert_eq!(wait.delay, Duration::from_millis(5));
    }

    #[test]
    fn x_ratelimit_reset_ms_is_used_when_higher_precedence_headers_are_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset-ms", HeaderValue::from_static("2000"));

        let wait = select_retry_wait(&headers, 0, &config(), UNIX_EPOCH);

        assert_eq!(wait.source, WaitSource::RateLimitResetMs);
        assert_eq!(
            wait.delay,
            Duration::from_millis(5),
            "Header-derived waits should be clamped to the configured per-sleep cap."
        );
    }

    #[test]
    fn past_rate_limit_reset_headers_fall_back_to_backoff() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1"));
        headers.insert("x-ratelimit-reset-ms", HeaderValue::from_static("1000"));

        let wait = select_retry_wait(&headers, 0, &config(), UNIX_EPOCH + Duration::from_secs(2));

        assert_eq!(
            wait.source,
            WaitSource::Backoff,
            "Expired provider reset windows should not create zero-length header sleeps."
        );
    }

    #[test]
    fn retry_after_http_date_uses_the_time_until_that_date() {
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(
                UNIX_EPOCH + Duration::from_secs(3),
            ))
            .unwrap(),
        );
        let mut config = config();
        config.max_sleep = Duration::from_secs(10);

        let wait = select_retry_wait(&headers, 0, &config, UNIX_EPOCH);

        assert_eq!(wait.source, WaitSource::RetryAfter);
        assert_eq!(wait.delay, Duration::from_secs(3));
    }

    #[test]
    fn fallback_backoff_is_used_without_headers() {
        let headers = HeaderMap::new();

        let wait = select_retry_wait(&headers, 0, &config(), UNIX_EPOCH);

        assert_eq!(wait.source, WaitSource::Backoff);
        assert!(wait.delay >= Duration::from_millis(1));
        assert!(wait.delay <= Duration::from_millis(5));
    }

    #[test]
    fn elapsed_retry_budget_includes_the_boundary_and_zero_delay() {
        assert!(retry_budget_exceeded(
            Duration::from_millis(10),
            Duration::ZERO,
            Duration::from_millis(10)
        ));
        assert!(retry_budget_exceeded(
            Duration::from_millis(9),
            Duration::from_millis(1),
            Duration::from_millis(10)
        ));
        assert!(!retry_budget_exceeded(
            Duration::from_millis(8),
            Duration::from_millis(1),
            Duration::from_millis(10)
        ));
    }
}
