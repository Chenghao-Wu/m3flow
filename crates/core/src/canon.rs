//! Canonical JSON + content hashing (plan §32, §56).
//!
//! Cache keys and artifact fingerprints must be independent of map ordering,
//! whitespace and float surface syntax. `canonicalize` produces a JSON value
//! with recursively sorted object keys and normalized numbers; `hash_json`
//! hashes its compact serialization.

use sha2::{Digest, Sha256};

pub fn canonicalize(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(m.len());
            for k in keys {
                out.insert(k.clone(), canonicalize(&m[k]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(canonicalize).collect())
        }
        // Normalize numbers through f64 so 300 and 300.0 hash identically.
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 9.0e15 {
                    serde_json::Value::Number(serde_json::Number::from(f as i64))
                } else {
                    // Stable float repr: shortest round-trip via ryu-like to_string
                    let s = format!("{f}");
                    match s.parse::<f64>() {
                        Ok(back) if back == f => serde_json::json!(s.parse::<f64>().unwrap()),
                        _ => v.clone(),
                    }
                }
            } else {
                v.clone()
            }
        }
        other => other.clone(),
    }
}

pub fn canonical_string(v: &serde_json::Value) -> String {
    serde_json::to_string(&canonicalize(v)).expect("canonical JSON serialization is infallible")
}

pub fn hash_json(v: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(canonical_string(v).as_bytes());
    hex::encode(h.finalize())
}

pub fn hash_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    hex::encode(h.finalize())
}

pub fn short_hash(full: &str, n: usize) -> String {
    full.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordering_and_number_normalization() {
        let a = json!({"b": 1, "a": 300.0});
        let b = json!({"a": 300, "b": 1});
        assert_eq!(hash_json(&a), hash_json(&b));
    }

    #[test]
    fn nested_arrays_stable() {
        let a = json!({"x": [{"z": 2, "y": 1}]});
        let b = json!({"x": [{"y": 1, "z": 2}]});
        assert_eq!(hash_json(&a), hash_json(&b));
    }
}
