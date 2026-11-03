use super::*;

pub(crate) const V8_PROVIDER_FAMILIES: [(&str, &str); 8] = [
    ("gemini-api-key", "gemini"),
    ("interactions-api-key", "interactions"),
    ("vertex-api-key", "vertex"),
    ("codex-api-key", "codex"),
    ("claude-api-key", "claude"),
    ("xai-api-key", "xai"),
    ("meta-api-key", "meta"),
    ("openai-compatibility", "openai-compatibility"),
];

fn is_v8_shared_provider_field(field: &str) -> bool {
    matches!(
        field,
        "priority"
            | "prefix"
            | "proxy-url"
            | "headers"
            | "models"
            | "excluded-models"
            | "disable-cooling"
            | "request-retry"
            | "request-scoped-errors"
            | "base-url"
    )
}

fn normalize_v8_provider_record(
    provider: &str,
    record: &serde_norway::Mapping,
) -> serde_norway::Mapping {
    let mut record = record.clone();
    if provider == "openai-compatibility" {
        record.remove(yaml_key("test-model"));
        record.remove(yaml_key("testModel"));
    }
    if let Some(models) = record
        .get_mut(yaml_key("models"))
        .and_then(serde_norway::Value::as_sequence_mut)
    {
        for model in models {
            let Some(thinking) = model
                .as_mapping_mut()
                .and_then(|model| model.get_mut(yaml_key("thinking")))
                .and_then(serde_norway::Value::as_mapping_mut)
            else {
                continue;
            };
            for (json, yaml) in [
                ("zero_allowed", "zero-allowed"),
                ("dynamic_allowed", "dynamic-allowed"),
            ] {
                if let Some(value) = thinking.remove(yaml_key(json)) {
                    thinking.entry(yaml_key(yaml)).or_insert(value);
                }
            }
        }
    }
    record
}

pub(crate) fn group_legacy_provider_records(
    provider: &str,
    records: &serde_norway::Value,
) -> Result<serde_norway::Value, String> {
    let records = records
        .as_sequence()
        .ok_or_else(|| format!("{provider} provider configuration must be an array"))?;
    let mut groups = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let record = record
            .as_mapping()
            .ok_or_else(|| format!("{provider} provider entry {index} must be an object"))?;
        let record = normalize_v8_provider_record(provider, record);
        if provider == "openai-compatibility" {
            let mut group = record.clone();
            let keys = group
                .remove(yaml_key("api-key-entries"))
                .unwrap_or_else(|| serde_norway::Value::Sequence(Vec::new()));
            group.insert(yaml_key("keys"), keys);
            groups.push(serde_norway::Value::Mapping(group));
            continue;
        }
        let mut group = serde_norway::Mapping::new();
        let name = yaml_mapping_value(&record, "name")
            .and_then(serde_norway::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{provider}-{}", index + 1));
        group.insert(yaml_key("name"), serde_norway::Value::String(name));
        let mut key = serde_norway::Mapping::new();
        for (field, value) in &record {
            if field.as_str() == Some("name") {
                continue;
            }
            if field
                .as_str()
                .is_some_and(is_v8_shared_provider_field)
            {
                group.insert(field.clone(), value.clone());
            } else {
                key.insert(field.clone(), value.clone());
            }
        }
        group.insert(
            yaml_key("keys"),
            serde_norway::Value::Sequence(vec![serde_norway::Value::Mapping(key)]),
        );
        groups.push(serde_norway::Value::Mapping(group));
    }
    Ok(serde_norway::Value::Sequence(groups))
}









pub(crate) fn truncate_for_error(value: &str) -> String {
    const LIMIT: usize = 240;
    let trimmed = value.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let shortened: String = trimmed.chars().take(LIMIT).collect();
    format!("{shortened}…")
}

pub(crate) fn open_external_url_inner(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Link is empty".to_string());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("Only http/https links can be opened".to_string());
    }

    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|err| format!("Failed to open browser: {err}"))
}

