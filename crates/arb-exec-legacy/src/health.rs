use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct HealthThresholds {
    pub max_reject_rate_ema: f64,
    pub max_latency_ms_ema: f64,
    pub min_success_rate_ema: f64,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            max_reject_rate_ema: 0.4,
            max_latency_ms_ema: 1_250.0,
            min_success_rate_ema: 0.6,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HealthSnapshot {
    pub reject_rate_ema: Option<f64>,
    pub success_rate_ema: Option<f64>,
    pub latency_ms_ema: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct EndpointHealth {
    ema_reject_rate: Option<f64>,
    ema_success_rate: Option<f64>,
    ema_latency_ms: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct HealthTracker {
    alpha: f64,
    thresholds: HealthThresholds,
    endpoints: HashMap<String, EndpointHealth>,
}

impl HealthTracker {
    pub fn new(alpha: f64, thresholds: HealthThresholds) -> Self {
        let alpha = alpha.clamp(0.01, 1.0);
        Self {
            alpha,
            thresholds,
            endpoints: HashMap::new(),
        }
    }

    #[allow(dead_code)]
    pub fn thresholds(&self) -> HealthThresholds {
        self.thresholds
    }

    pub fn snapshot(&self, endpoint: &str) -> HealthSnapshot {
        let health = self.endpoints.get(endpoint);
        HealthSnapshot {
            reject_rate_ema: health.and_then(|entry| entry.ema_reject_rate),
            success_rate_ema: health.and_then(|entry| entry.ema_success_rate),
            latency_ms_ema: health.and_then(|entry| entry.ema_latency_ms),
        }
    }

    pub fn is_healthy(&self, endpoint: &str) -> bool {
        let snapshot = self.snapshot(endpoint);
        if let Some(reject_rate) = snapshot.reject_rate_ema {
            if reject_rate > self.thresholds.max_reject_rate_ema {
                return false;
            }
        }
        if let Some(success_rate) = snapshot.success_rate_ema {
            if success_rate < self.thresholds.min_success_rate_ema {
                return false;
            }
        }
        if let Some(latency_ms) = snapshot.latency_ms_ema {
            if latency_ms > self.thresholds.max_latency_ms_ema {
                return false;
            }
        }
        true
    }

    pub fn health_score(&self, endpoint: &str) -> f64 {
        let snapshot = self.snapshot(endpoint);
        let reject_rate = snapshot.reject_rate_ema.unwrap_or(0.0).clamp(0.0, 1.0);
        let success_rate = snapshot.success_rate_ema.unwrap_or(1.0).clamp(0.0, 1.0);
        let latency_ms = snapshot.latency_ms_ema.unwrap_or(0.0).max(0.0);
        let latency_ratio = if self.thresholds.max_latency_ms_ema > 0.0 {
            (latency_ms / self.thresholds.max_latency_ms_ema).clamp(0.0, 4.0)
        } else {
            0.0
        };
        let latency_penalty = 1.0 + 0.6 * latency_ratio;
        (success_rate * (1.0 - reject_rate)) / latency_penalty
    }

    pub fn record_success(&mut self, endpoint: &str, latency: Option<Duration>) {
        let alpha = self.alpha;
        let entry = self.entry_mut(endpoint);
        crate::util::update_float_ema(&mut entry.ema_success_rate, alpha, 1.0);
        crate::util::update_float_ema(&mut entry.ema_reject_rate, alpha, 0.0);
        if let Some(latency) = latency {
            let millis = latency.as_secs_f64() * 1_000.0;
            crate::util::update_float_ema(&mut entry.ema_latency_ms, alpha, millis);
        }
    }

    pub fn record_failure(&mut self, endpoint: &str, rejected: bool, latency: Option<Duration>) {
        let alpha = self.alpha;
        let entry = self.entry_mut(endpoint);
        crate::util::update_float_ema(&mut entry.ema_success_rate, alpha, 0.0);
        let reject_value = if rejected { 1.0 } else { 0.0 };
        crate::util::update_float_ema(&mut entry.ema_reject_rate, alpha, reject_value);
        if let Some(latency) = latency {
            let millis = latency.as_secs_f64() * 1_000.0;
            crate::util::update_float_ema(&mut entry.ema_latency_ms, alpha, millis);
        }
    }

    fn entry_mut(&mut self, endpoint: &str) -> &mut EndpointHealth {
        self.endpoints.entry(endpoint.to_string()).or_default()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_unhealthy_on_reject_rate() {
        let thresholds = HealthThresholds {
            max_reject_rate_ema: 0.3,
            max_latency_ms_ema: 5_000.0,
            min_success_rate_ema: 0.1,
        };
        let mut tracker = HealthTracker::new(0.5, thresholds);
        tracker.record_failure("relay-a", true, None);
        tracker.record_failure("relay-a", true, None);
        assert!(!tracker.is_healthy("relay-a"));
    }

    #[test]
    fn marks_unhealthy_on_latency() {
        let thresholds = HealthThresholds {
            max_reject_rate_ema: 0.9,
            max_latency_ms_ema: 200.0,
            min_success_rate_ema: 0.1,
        };
        let mut tracker = HealthTracker::new(0.4, thresholds);
        tracker.record_success("relay-b", Some(Duration::from_millis(500)));
        assert!(!tracker.is_healthy("relay-b"));
    }

    #[test]
    fn marks_unhealthy_on_success_rate() {
        let thresholds = HealthThresholds {
            max_reject_rate_ema: 1.0,
            max_latency_ms_ema: 5_000.0,
            min_success_rate_ema: 0.8,
        };
        let mut tracker = HealthTracker::new(0.6, thresholds);
        tracker.record_failure("rpc-a", false, None);
        tracker.record_failure("rpc-a", false, None);
        assert!(!tracker.is_healthy("rpc-a"));
    }

    #[test]
    fn health_score_prefers_lower_latency() {
        let thresholds = HealthThresholds::default();
        let mut tracker = HealthTracker::new(0.4, thresholds);
        tracker.record_success("fast", Some(Duration::from_millis(50)));
        tracker.record_success("slow", Some(Duration::from_millis(900)));
        assert!(tracker.health_score("fast") > tracker.health_score("slow"));
    }
}
