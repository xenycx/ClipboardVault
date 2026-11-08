//! In-memory fixed-window rate limits, keyed by a bucket name and a subject
//! (a client address or an API key ID). One process serves every request, so a
//! shared map is enough; limits reset when the process restarts.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::http::HeaderMap;

#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub max: u32,
    pub window: Duration,
}

pub const SIGN_IN: Rule = Rule {
    max: 10,
    window: Duration::from_secs(60),
};
pub const SIGN_UP: Rule = Rule {
    max: 5,
    window: Duration::from_secs(300),
};
pub const RESET_PASSWORD: Rule = Rule {
    max: 3,
    window: Duration::from_secs(300),
};
pub const SETUP: Rule = Rule {
    max: 10,
    window: Duration::from_secs(60),
};
/// Anything else under `/api/auth/*`, as Better Auth's global limit did.
pub const AUTH_DEFAULT: Rule = Rule {
    max: 100,
    window: Duration::from_secs(60),
};
pub const API_KEY: Rule = Rule {
    max: 120,
    window: Duration::from_secs(60),
};

const PRUNE_ABOVE: usize = 10_000;

/// `(bucket, subject)` → `(window start, requests counted in the window)`.
type Windows = HashMap<(&'static str, String), (Instant, u32)>;

#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<Windows>,
}

impl RateLimiter {
    /// Counts one request and reports whether it is within the rule.
    pub fn allow(&self, bucket: &'static str, subject: &str, rule: Rule) -> bool {
        let now = Instant::now();
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if windows.len() > PRUNE_ABOVE {
            windows
                .retain(|_, (started, _)| now.duration_since(*started) < Duration::from_secs(600));
        }
        let entry = windows
            .entry((bucket, subject.to_owned()))
            .or_insert((now, 0));
        if now.duration_since(entry.0) >= rule.window {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= rule.max
    }
}

/// The address a request came from.
///
/// With `trust_proxy` set, the right-most `X-Forwarded-For` entry is used: that is
/// the one the nearest proxy appended, and the only one a client cannot choose.
/// Otherwise (and when the header is missing) the TCP peer is used.
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, trust_proxy: bool) -> String {
    if trust_proxy
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .rsplit(',')
                    .map(str::trim)
                    .find(|part| !part.is_empty())
            })
            .and_then(|value| value.parse::<IpAddr>().ok())
    {
        return forwarded.to_string();
    }
    peer.map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_beyond_the_rule_are_refused_until_the_window_ends() {
        let limiter = RateLimiter::default();
        let rule = Rule {
            max: 2,
            window: Duration::from_millis(50),
        };
        assert!(limiter.allow("t", "a", rule));
        assert!(limiter.allow("t", "a", rule));
        assert!(!limiter.allow("t", "a", rule));
        assert!(limiter.allow("t", "b", rule), "subjects are independent");
        assert!(limiter.allow("u", "a", rule), "buckets are independent");
        std::thread::sleep(Duration::from_millis(60));
        assert!(limiter.allow("t", "a", rule));
    }

    #[test]
    fn forwarded_addresses_are_ignored_unless_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6, 10.0.0.9".parse().unwrap());
        let peer = Some("172.18.0.2".parse().unwrap());
        assert_eq!(client_ip(&headers, peer, false), "172.18.0.2");
        assert_eq!(client_ip(&headers, peer, true), "10.0.0.9");
        headers.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        assert_eq!(client_ip(&headers, peer, true), "172.18.0.2");
        assert_eq!(client_ip(&HeaderMap::new(), None, true), "unknown");
    }
}
