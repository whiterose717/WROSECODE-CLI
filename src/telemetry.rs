use crate::metrics::MetricsSnapshot;

pub async fn export(client: &reqwest::Client, snapshot: &MetricsSnapshot) {
    let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") else {
        return;
    };
    let payload = serde_json::json!({
        "resourceMetrics": [{
            "resource": {"attributes": [{"key":"service.name","value":{"stringValue":"wrosecode"}}]},
            "scopeMetrics": [{"metrics": [
                gauge("wrosecode.tokens.input", snapshot.usage.input as f64),
                gauge("wrosecode.tokens.output", snapshot.usage.output as f64),
                gauge("wrosecode.cache.read", snapshot.usage.cache_read as f64),
                gauge("wrosecode.tool.calls", snapshot.tool_calls as f64),
                gauge("wrosecode.cost.usd", snapshot.cost_usd.unwrap_or(0.0)),
            ]}]
        }]
    });
    let _ = client
        .post(format!("{}/v1/metrics", endpoint.trim_end_matches('/')))
        .json(&payload)
        .send()
        .await;
}

fn gauge(name: &str, value: f64) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "gauge": {"dataPoints": [{"asDouble": value, "timeUnixNano": now_nanos().to_string()}]}
    })
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
