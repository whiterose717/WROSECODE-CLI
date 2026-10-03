use crate::provider::Usage;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelMetrics {
    pub usage: Usage,
    pub calls: u64,
    pub cost_usd: Option<f64>,
}

/// One bucket of the latency histogram. `upper_ms == u64::MAX` is the overflow
/// bucket, so every observed latency always lands in exactly one bucket.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistogramBucket {
    pub upper_ms: u64,
    pub count: u64,
}

/// Upper bounds (milliseconds) for the latency histogram buckets.
pub const HISTOGRAM_BOUNDS: [u64; 11] = [
    50,
    100,
    250,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    30_000,
    60_000,
    u64::MAX,
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub usage: Usage,
    pub cost_usd: Option<f64>,
    pub calls: u64,
    pub tool_calls: u64,
    pub tool_failures: u64,
    pub rate_limits: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    /// Counts per bucket, aligned with [`HISTOGRAM_BOUNDS`].
    #[serde(default)]
    pub latency_histogram: Vec<HistogramBucket>,
    pub by_model: BTreeMap<String, ModelMetrics>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PricingFile {
    #[serde(default)]
    model: BTreeMap<String, Price>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct Price {
    input_per_million: f64,
    output_per_million: f64,
    #[serde(default)]
    cache_read_per_million: f64,
}

#[derive(Default)]
struct State {
    usage: Usage,
    calls: u64,
    tool_calls: u64,
    tool_failures: u64,
    rate_limits: u64,
    latencies_ms: VecDeque<u64>,
    by_model: BTreeMap<String, ModelMetrics>,
}

#[derive(Clone, Default)]
pub struct Metrics {
    state: Arc<Mutex<State>>,
    prices: Arc<BTreeMap<String, Price>>,
}

impl Metrics {
    pub fn load(root: &Path) -> Self {
        let personal = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|home| home.join(".wrosecode/pricing.toml"));
        let source = personal
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .or_else(|| std::fs::read_to_string(root.join("pricing.toml")).ok());
        let prices = source
            .and_then(|value| toml::from_str::<PricingFile>(&value).ok())
            .unwrap_or_default()
            .model;
        Self {
            state: Arc::new(Mutex::new(State::default())),
            prices: Arc::new(prices),
        }
    }

    pub fn record_model(&self, model: &str, usage: Usage, latency_ms: u64) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.usage.add(usage);
        state.calls += 1;
        state.latencies_ms.push_back(latency_ms);
        if state.latencies_ms.len() > 2_048 {
            state.latencies_ms.pop_front();
        }
        let model_metrics = state.by_model.entry(model.to_string()).or_default();
        model_metrics.usage.add(usage);
        model_metrics.calls += 1;
        model_metrics.cost_usd = self.cost(model, model_metrics.usage);
    }

    pub fn record_tool(&self, success: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.tool_calls += 1;
            state.tool_failures += u64::from(!success);
        }
    }

    /// Record a provider throttling response (HTTP 429 / quota) so the dashboard can
    /// warn before the configured budget or rate limit is exhausted.
    pub fn record_rate_limit(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.rate_limits += 1;
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let Ok(state) = self.state.lock() else {
            return MetricsSnapshot::default();
        };
        let mut latencies: Vec<u64> = state.latencies_ms.iter().copied().collect();
        latencies.sort_unstable();
        let costs: Vec<f64> = state
            .by_model
            .values()
            .filter_map(|metrics| metrics.cost_usd)
            .collect();
        let cost_usd = (!costs.is_empty()).then(|| costs.iter().sum());
        MetricsSnapshot {
            usage: state.usage,
            cost_usd,
            calls: state.calls,
            tool_calls: state.tool_calls,
            tool_failures: state.tool_failures,
            rate_limits: state.rate_limits,
            latency_p50_ms: percentile(&latencies, 50),
            latency_p95_ms: percentile(&latencies, 95),
            latency_p99_ms: percentile(&latencies, 99),
            latency_histogram: histogram(&latencies),
            by_model: state.by_model.clone(),
        }
    }

    pub fn export(&self, dir: &Path) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
        std::fs::create_dir_all(dir)?;
        let snapshot = self.snapshot();
        let json_path = dir.join("metrics.json");
        let csv_path = dir.join("metrics.csv");
        let latency_path = dir.join("latency.csv");
        std::fs::write(&json_path, serde_json::to_vec_pretty(&snapshot)?)?;
        let mut csv =
            String::from("model,calls,input,output,reasoning,cache_read,cache_write,cost_usd\n");
        for (model, value) in &snapshot.by_model {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{}\n",
                csv_escape(model),
                value.calls,
                value.usage.input,
                value.usage.output,
                value.usage.reasoning,
                value.usage.cache_read,
                value.usage.cache_write,
                value
                    .cost_usd
                    .map(|cost| format!("{cost:.8}"))
                    .unwrap_or_default()
            ));
        }
        std::fs::write(&csv_path, csv)?;
        let mut latency = String::from("bucket_upper_ms,count\n");
        for bucket in &snapshot.latency_histogram {
            let upper = if bucket.upper_ms == u64::MAX {
                "inf".to_string()
            } else {
                bucket.upper_ms.to_string()
            };
            latency.push_str(&format!("{upper},{}\n", bucket.count));
        }
        std::fs::write(&latency_path, latency)?;
        Ok((json_path, csv_path))
    }

    fn cost(&self, model: &str, usage: Usage) -> Option<f64> {
        let price = self.prices.get(model)?;
        Some(
            usage.input as f64 * price.input_per_million / 1_000_000.0
                + usage.output as f64 * price.output_per_million / 1_000_000.0
                + usage.cache_read as f64 * price.cache_read_per_million / 1_000_000.0,
        )
    }
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values[((values.len() - 1) * percentile / 100).min(values.len() - 1)]
}

/// Bucket sorted latencies into [`HISTOGRAM_BOUNDS`]. Returns one entry per bound so
/// the renderer always gets a stable shape, even before any latency is recorded.
pub fn histogram(values: &[u64]) -> Vec<HistogramBucket> {
    let mut counts = vec![0_u64; HISTOGRAM_BOUNDS.len()];
    for value in values {
        let index = HISTOGRAM_BOUNDS
            .iter()
            .position(|bound| *bound != u64::MAX && *value <= *bound)
            .unwrap_or(HISTOGRAM_BOUNDS.len() - 1);
        counts[index] += 1;
    }
    HISTOGRAM_BOUNDS
        .iter()
        .zip(counts)
        .map(|(upper_ms, count)| HistogramBucket {
            upper_ms: *upper_ms,
            count,
        })
        .collect()
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_latency_percentiles() {
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 50), 30);
        assert_eq!(percentile(&[10, 20, 30, 40, 50], 95), 40);
    }

    #[test]
    fn histogram_buckets_every_latency_exactly_once() {
        let values = [0, 50, 51, 1_000, 1_001, 60_000, 60_001, u64::MAX];
        let buckets = histogram(&values);
        assert_eq!(buckets.len(), HISTOGRAM_BOUNDS.len());
        assert_eq!(buckets.iter().map(|bucket| bucket.count).sum::<u64>(), 8);
        assert_eq!(buckets[0].count, 2, "0 and 50 fall in the first bucket");
        assert_eq!(buckets[4].count, 1, "1000 is the 1s bucket");
        assert_eq!(
            buckets.last().map(|bucket| bucket.count),
            Some(2),
            "60001 and MAX land in overflow"
        );
    }

    #[test]
    fn empty_histogram_has_stable_shape() {
        let buckets = histogram(&[]);
        assert_eq!(buckets.len(), HISTOGRAM_BOUNDS.len());
        assert!(buckets.iter().all(|bucket| bucket.count == 0));
    }

    #[test]
    fn rate_limits_are_counted() {
        let metrics = Metrics::load(std::path::Path::new("."));
        assert_eq!(metrics.snapshot().rate_limits, 0);
        metrics.record_rate_limit();
        metrics.record_rate_limit();
        assert_eq!(metrics.snapshot().rate_limits, 2);
    }

    #[test]
    fn export_writes_json_csv_and_latency_csv() {
        let directory = std::env::temp_dir().join(format!("wrose-metrics-{}", std::process::id()));
        let metrics = Metrics::load(std::path::Path::new("."));
        metrics.record_model("test-model", Usage::default(), 12);
        metrics.record_rate_limit();
        let (json, csv) = metrics.export(&directory).expect("export");
        assert!(json.exists() && csv.exists());
        let latency = std::fs::read_to_string(directory.join("latency.csv")).unwrap();
        assert!(latency.starts_with("bucket_upper_ms,count\n"));
        assert!(latency.contains("inf,"));
        let payload = std::fs::read_to_string(json).unwrap();
        assert!(payload.contains("rate_limits"));
        assert!(payload.contains("latency_histogram"));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
