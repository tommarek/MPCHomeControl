//! Shared MQTT topic-filter helpers for the bridge / source adapters.
//!
//! `+` matches exactly one level, `#` matches the remaining levels. The broker only delivers topics
//! that match a subscription, but several signals may subscribe overlapping wildcards, so each
//! delivered topic is matched back against every filter to decide which destination(s) it feeds.

/// Does `filter` match the concrete `topic`? Assumes `filter` is valid (see [`validate_filter`]), so a
/// `#` is the final level and matching the rest of the topic there is correct.
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => continue,
            (Some(a), Some(b)) if a == b => continue,
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Validate a topic **filter** against the MQTT spec so a malformed one is rejected at config load
/// rather than silently mis-routing: `+` must occupy a whole single level, and `#` must be a whole
/// level **and** the last one (e.g. `a/#/b` would otherwise match anything under `a`).
pub fn validate_filter(filter: &str) -> Result<(), String> {
    let levels: Vec<&str> = filter.split('/').collect();
    for (i, level) in levels.iter().enumerate() {
        if level.contains('#') && (*level != "#" || i != levels.len() - 1) {
            return Err(format!(
                "'#' must be its own final level in filter {filter:?}"
            ));
        }
        if level.contains('+') && *level != "+" {
            return Err(format!(
                "'+' must occupy a whole level in filter {filter:?}"
            ));
        }
    }
    Ok(())
}

/// Reject a misconfigured `scale` (JSON5 happily parses NaN/inf/negative) at config load. `what` is a
/// caller-built context label (e.g. `signal "teslamate/#" → ev/battery_level`).
pub fn validate_scale(scale: f64, what: &str) -> Result<(), String> {
    if scale.is_finite() && scale > 0.0 {
        Ok(())
    } else {
        Err(format!(
            "{what}: scale must be finite and > 0 (got {scale})"
        ))
    }
}

/// Reject a malformed JSON pointer (RFC-6901: empty or starting with `/`) at config load. `what` is a
/// caller-built context label.
pub fn validate_pointer(pointer: Option<&str>, what: &str) -> Result<(), String> {
    match pointer {
        Some(p) if !p.is_empty() && !p.starts_with('/') => Err(format!(
            "{what}: JSON pointer {p:?} must be empty or start with '/'"
        )),
        _ => Ok(()),
    }
}

/// Extract a numeric value from an MQTT payload — a bare number/bool, or a field of a JSON object via
/// `pointer`. Booleans map to 1.0 / 0.0.
pub fn parse_value(payload: &[u8], pointer: Option<&str>) -> Option<f64> {
    let text = std::str::from_utf8(payload).ok()?.trim();
    if let Some(ptr) = pointer {
        let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
        return json_number(v.pointer(ptr)?);
    }
    if let Ok(f) = text.parse::<f64>() {
        return Some(f);
    }
    json_number(&serde_json::from_str(text).ok()?)
}

fn json_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse a payload to a number and apply `scale`, returning the finite scaled value or a short reason
/// it was dropped — un-parseable, or non-finite after scaling (a misconfigured / overflowing scale).
pub fn parse_and_scale(payload: &[u8], pointer: Option<&str>, scale: f64) -> Result<f64, String> {
    let raw = parse_value(payload, pointer).ok_or("could not parse a number from payload")?;
    let value = raw * scale;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("non-finite value (raw {raw} × scale {scale})"))
    }
}

/// Subscribe every `topic` (QoS 1); returns how many succeeded. A 0 (with topics configured) is logged
/// so it isn't a silent "healthy but idle" state. `log_prefix` tags the log lines, e.g. `bridge`.
pub async fn subscribe_all(
    client: &rumqttc::AsyncClient,
    topics: &[&str],
    log_prefix: &str,
) -> usize {
    let mut ok = 0;
    for &topic in topics {
        match client.subscribe(topic, rumqttc::QoS::AtLeastOnce).await {
            Ok(()) => ok += 1,
            Err(e) => eprintln!("[{log_prefix}] subscribe {topic} failed: {e}"),
        }
    }
    if !topics.is_empty() && ok == 0 {
        eprintln!(
            "[{log_prefix}] WARNING: 0/{} subscriptions succeeded",
            topics.len()
        );
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_value_handles_number_bool_and_pointer() {
        assert_eq!(parse_value(b"80", None), Some(80.0));
        assert_eq!(parse_value(b"true", None), Some(1.0));
        assert_eq!(parse_value(br#"{"soc": 90}"#, Some("/soc")), Some(90.0));
        assert_eq!(parse_value(b"not a number", None), None);
    }

    #[test]
    fn validators_reject_bad_input() {
        assert!(validate_scale(0.001, "x").is_ok());
        assert!(validate_scale(0.0, "x").is_err());
        assert!(validate_scale(f64::NAN, "x").is_err());
        assert!(validate_pointer(Some("/a/b"), "x").is_ok());
        assert!(validate_pointer(None, "x").is_ok());
        assert!(validate_pointer(Some("a/b"), "x").is_err());
    }

    #[test]
    fn exact_and_wildcards() {
        assert!(topic_matches("a/b/c", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/x/c"));
        assert!(topic_matches("a/b/#", "a/b/c/d"));
        assert!(topic_matches("a/#", "a/b/c"));
        assert!(topic_matches("a/#", "a")); // `a/#` also matches the parent level `a`
        assert!(!topic_matches("a/+/c", "a/x/d"));
        assert!(!topic_matches("a/2/#", "a/1/b"));
        assert!(!topic_matches("a/+", "a/b/c"));
    }

    #[test]
    fn validate_filter_enforces_wildcard_placement() {
        assert!(validate_filter("a/+/c").is_ok());
        assert!(validate_filter("a/b/#").is_ok());
        assert!(validate_filter("a/#").is_ok());
        assert!(validate_filter("a/b/c").is_ok());
        assert!(validate_filter("a/#/b").is_err()); // `#` not the final level
        assert!(validate_filter("a/b#").is_err()); // `#` not a whole level
        assert!(validate_filter("a/1+/x").is_err()); // `+` not a whole level
    }
}
