use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

/// Configuration file formats, selected by file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Json,
    Yaml,
}

const JSON_EXTENSION: &str = "json";
const YAML_EXTENSIONS: [&str; 2] = ["yaml", "yml"];

/// Reads JSON or YAML and merges a sibling `<stem>.local.<ext>` when present.
/// `<stem>.example.<ext>` and `<stem>.<ext>` use the same override name, and the
/// override keeps the base file's extension. An explicit `.local.<ext>` path has
/// no further override. Returns read or parse errors.
pub fn load_with_local_override(path: &Path) -> io::Result<Value> {
    let format = format_of(path)?;
    let raw = fs::read_to_string(path)?;
    let mut base = parse(path, &raw, format)?;

    if let Some(local_path) = local_override_path(path)
        && local_path.exists()
    {
        let local_raw = fs::read_to_string(&local_path)?;
        let overlay = parse(&local_path, &local_raw, format)?;
        merge_json(&mut base, overlay);
    }

    Ok(base)
}

fn format_of(path: &Path) -> io::Result<Format> {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case(JSON_EXTENSION) => Ok(Format::Json),
        Some(e) if YAML_EXTENSIONS.iter().any(|y| e.eq_ignore_ascii_case(y)) => Ok(Format::Yaml),
        _ => Err(io::Error::other(format!(
            "{}: expected a .json, .yaml or .yml configuration file",
            path.display()
        ))),
    }
}

fn parse(path: &Path, raw: &str, format: Format) -> io::Result<Value> {
    let parsed = match format {
        Format::Json => serde_json::from_str(raw).map_err(|e| e.to_string()),
        Format::Yaml => serde_yaml::from_str(raw).map_err(|e| e.to_string()),
    };
    parsed.map_err(|error| io::Error::other(format!("{}: {error}", path.display())))
}

fn local_override_path(path: &Path) -> Option<std::path::PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    let extension = [JSON_EXTENSION, YAML_EXTENSIONS[0], YAML_EXTENSIONS[1]]
        .into_iter()
        .find(|e| {
            file_name
                .rsplit_once('.')
                .is_some_and(|(_, found)| found.eq_ignore_ascii_case(e))
        })?;
    if file_name.ends_with(&format!(".local.{extension}")) {
        return None;
    }
    let stem = file_name
        .strip_suffix(&format!(".example.{extension}"))
        .or_else(|| file_name.strip_suffix(&format!(".{extension}")))?;
    Some(path.with_file_name(format!("{stem}.local.{extension}")))
}

/// Recursively merges object keys. A null object value removes that key.
/// Non-object values, including arrays, replace the base value.
pub fn merge_json(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                if v.is_null() {
                    b.remove(&k);
                } else {
                    merge_json(b.entry(k).or_insert(Value::Null), v);
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_nested_objects() {
        let mut base = json!({ "a": { "x": 1, "y": 2 }, "b": 3 });
        merge_json(&mut base, json!({ "a": { "y": 20, "z": 30 } }));
        assert_eq!(base, json!({ "a": { "x": 1, "y": 20, "z": 30 }, "b": 3 }));
    }

    #[test]
    fn overlay_scalar_replaces_base() {
        let mut base = json!({ "a": 1 });
        merge_json(&mut base, json!({ "a": "two" }));
        assert_eq!(base, json!({ "a": "two" }));
    }

    #[test]
    fn overlay_array_replaces_base_array() {
        let mut base = json!({ "xs": [1, 2, 3] });
        merge_json(&mut base, json!({ "xs": [9] }));
        assert_eq!(base, json!({ "xs": [9] }));
    }

    #[test]
    fn null_overlay_deletes_key() {
        let mut base = json!({ "a": 1, "b": 2 });
        merge_json(&mut base, json!({ "a": null }));
        assert_eq!(base, json!({ "b": 2 }));
    }

    #[test]
    fn overlay_adds_new_key() {
        let mut base = json!({ "a": 1 });
        merge_json(&mut base, json!({ "b": 2 }));
        assert_eq!(base, json!({ "a": 1, "b": 2 }));
    }

    #[test]
    fn object_replaces_scalar() {
        let mut base = json!({ "a": 1 });
        merge_json(&mut base, json!({ "a": { "nested": true } }));
        assert_eq!(base, json!({ "a": { "nested": true } }));
    }

    #[test]
    fn local_override_path_for_plain_json() {
        assert_eq!(
            local_override_path(Path::new("/x/radio.json")),
            Some(Path::new("/x/radio.local.json").to_path_buf()),
        );
    }

    #[test]
    fn local_override_path_for_example_json() {
        assert_eq!(
            local_override_path(Path::new("/x/radiod.example.json")),
            Some(Path::new("/x/radiod.local.json").to_path_buf()),
        );
    }

    #[test]
    fn local_override_path_skips_local_json() {
        assert_eq!(local_override_path(Path::new("/x/radio.local.json")), None);
    }

    #[test]
    fn local_override_path_keeps_the_yaml_extension() {
        assert_eq!(
            local_override_path(Path::new("/x/radio.yaml")),
            Some(Path::new("/x/radio.local.yaml").to_path_buf()),
        );
        assert_eq!(
            local_override_path(Path::new("/x/radio.yml")),
            Some(Path::new("/x/radio.local.yml").to_path_buf()),
        );
        assert_eq!(
            local_override_path(Path::new("/x/radiod.example.yaml")),
            Some(Path::new("/x/radiod.local.yaml").to_path_buf()),
        );
        assert_eq!(local_override_path(Path::new("/x/radio.local.yaml")), None);
        assert_eq!(local_override_path(Path::new("/x/radio.local.yml")), None);
    }

    #[test]
    fn yaml_and_json_parse_to_the_same_value() {
        let yaml = parse(
            Path::new("radio.yaml"),
            "kind: noop\ntx_sample_rate_hz: 2000000\nrx_bandwidth_hz: null\n",
            Format::Yaml,
        )
        .unwrap();
        assert_eq!(
            yaml,
            json!({ "kind": "noop", "tx_sample_rate_hz": 2000000, "rx_bandwidth_hz": null })
        );
    }

    #[test]
    fn format_follows_the_extension_and_rejects_others() {
        assert_eq!(format_of(Path::new("radio.json")).unwrap(), Format::Json);
        assert_eq!(format_of(Path::new("radio.YAML")).unwrap(), Format::Yaml);
        assert_eq!(format_of(Path::new("radio.yml")).unwrap(), Format::Yaml);
        let error = format_of(Path::new("radio.toml")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected a .json, .yaml or .yml")
        );
        assert!(format_of(Path::new("radio")).is_err());
    }

    #[test]
    fn parse_error_names_the_config_file() {
        let error = parse(Path::new("config/radio.local.json"), "", Format::Json).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("config/radio.local.json: EOF")
        );
        let error = parse(Path::new("config/radio.yaml"), "kind: [", Format::Yaml).unwrap_err();
        assert!(error.to_string().starts_with("config/radio.yaml: "));
    }
}
