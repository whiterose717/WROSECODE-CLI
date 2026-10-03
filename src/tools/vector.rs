use anyhow::Result;
use serde_json::json;
use std::hash::{Hash, Hasher};

const COLLECTION: &str = "wrose_writeups";
const DIMENSIONS: usize = 32;

pub async fn archive(
    client: &reqwest::Client,
    base: &str,
    title: &str,
    text: &str,
) -> Result<String> {
    let base = base.trim_end_matches('/');
    let _ = client
        .put(format!("{base}/collections/{COLLECTION}"))
        .json(&json!({"vectors":{"size":DIMENSIONS,"distance":"Cosine"}}))
        .send()
        .await;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    title.hash(&mut hasher);
    text.hash(&mut hasher);
    let id = hasher.finish();
    let response = client
        .put(format!("{base}/collections/{COLLECTION}/points?wait=true"))
        .json(&json!({"points":[{"id":id,"vector":embed(text),"payload":{"title":title,"text":text}}]}))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("Qdrant archive failed with {}", response.status());
    }
    Ok(format!("archived writeup {title} as point {id}"))
}

pub async fn search(client: &reqwest::Client, base: &str, query: &str) -> Result<String> {
    let response = client
        .post(format!(
            "{}/collections/{COLLECTION}/points/search",
            base.trim_end_matches('/')
        ))
        .json(&json!({"vector":embed(query),"limit":5,"with_payload":true}))
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("Qdrant search failed with {}", response.status());
    }
    let value: serde_json::Value = response.json().await?;
    Ok(serde_json::to_string_pretty(&value["result"])?)
}

fn embed(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0_f32; DIMENSIONS];
    for token in text.split(|character: char| !character.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        token.to_ascii_lowercase().hash(&mut hasher);
        vector[hasher.finish() as usize % DIMENSIONS] += 1.0;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_embedding_is_normalized() {
        let vector = embed("buffer overflow return address");
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.001);
    }
}
