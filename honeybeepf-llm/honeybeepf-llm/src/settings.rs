use config::{Config, ConfigError, Environment, Map};
use serde::{Deserialize, Deserializer};

const DEFAULT_PROBE_INTERVAL_SECONDS: u32 = 60;

/// Deserializes a comma-separated string (or an actual sequence) into `Option<Vec<String>>`.
/// Use `#[serde(default, deserialize_with = "deserialize_comma_separated")]` on any
/// `Option<Vec<String>>` field that is populated via an environment variable like
/// `MY_FIELD=foo,bar,baz`.
fn deserialize_comma_separated<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(split_comma_separated(&s))),
        Some(serde_json::Value::Array(arr)) => {
            let items = arr
                .into_iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .ok_or_else(|| D::Error::custom("expected string in array"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(items))
        }
        Some(other) => Err(D::Error::custom(format!(
            "expected string or array, got {other}"
        ))),
    }
}

fn split_comma_separated(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Filesystem probe configuration
#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct FilesystemProbes {
    pub file_access: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_comma_separated")]
    pub watched_paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct BuiltinProbes {
    #[serde(default)]
    pub filesystem: FilesystemProbes,
    pub llm: Option<bool>,
    /// Probe executables with OpenSSL built in (Node.js); on unless set to false.
    pub llm_executables: Option<bool>,
    /// Pod labels copied onto LLM metrics (k8s feature); each adds series, so none by default.
    #[serde(skip)]
    pub llm_pod_labels: Option<Vec<String>>,
    pub process_lifecycle: Option<bool>,
    pub interval: Option<u32>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct Settings {
    pub otel_exporter_otlp_endpoint: Option<String>,
    pub otel_exporter_otlp_protocol: Option<String>,
    #[serde(default)]
    pub builtin_probes: BuiltinProbes,
    pub custom_probe_config: Option<String>,
    pub debug: Option<bool>,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        dotenvy::dotenv().ok();

        // Debug: print relevant environment variables
        for (key, value) in std::env::vars() {
            if key.starts_with("BUILTIN") || key.starts_with("RUST_LOG") {
                eprintln!("ENV: {}={}", key, value);
            }
        }

        let settings = Self::from_vars(std::env::vars().collect())?;
        eprintln!("Parsed settings: {:?}", settings);
        Ok(settings)
    }

    /// Settings from environment-style variables.
    fn from_vars(vars: Map<String, String>) -> Result<Self, ConfigError> {
        // Read raw: try_parsing turns a lone key like "2024" or "True" into a number or bool.
        let pod_labels = vars
            .get("BUILTIN_PROBES__LLM_POD_LABELS")
            .map(|v| split_comma_separated(v));
        let env = Environment::default().separator("__").try_parsing(true);
        let s = Config::builder()
            .add_source(env.source(Some(vars)))
            .build()?;

        let mut settings: Self = s.try_deserialize()?;
        settings.builtin_probes.llm_pod_labels = pod_labels;
        Ok(settings)
    }

    pub fn to_common_config(&self) -> honeybeepf_llm_common::CommonConfig {
        // LLM probe
        let probe_llm = self.builtin_probes.llm.unwrap_or(false);

        let probe_interval = self
            .builtin_probes
            .interval
            .unwrap_or(DEFAULT_PROBE_INTERVAL_SECONDS);

        honeybeepf_llm_common::CommonConfig {
            // LLM
            probe_llm: probe_llm as u8,
            // Interval
            probe_interval,
            _pad: [0; 7],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn test_load_settings() {
        dotenvy::dotenv().ok();

        unsafe {
            std::env::set_var("BUILTIN_PROBES__FILESYSTEM__FILE_ACCESS", "true");
            std::env::set_var("BUILTIN_PROBES__INTERVAL", "42");
            std::env::set_var("BUILTIN_PROBES__LLM_EXECUTABLES", "false");
            std::env::set_var(
                "BUILTIN_PROBES__LLM_POD_LABELS",
                "variant, app.kubernetes.io/Ver",
            );
        }

        let settings = Settings::new().expect("Failed to load settings");

        assert_eq!(settings.builtin_probes.filesystem.file_access, Some(true));
        assert_eq!(settings.builtin_probes.interval, Some(42));
        assert_eq!(settings.builtin_probes.llm_executables, Some(false));
        let pod_labels = settings.builtin_probes.llm_pod_labels.as_deref();
        assert_eq!(
            pod_labels,
            Some(&["variant", "app.kubernetes.io/Ver"].map(String::from)[..])
        );

        // 환경변수 정리
        unsafe {
            std::env::remove_var("BUILTIN_PROBES__FILESYSTEM__FILE_ACCESS");
            std::env::remove_var("BUILTIN_PROBES__INTERVAL");
            std::env::remove_var("BUILTIN_PROBES__LLM_EXECUTABLES");
            std::env::remove_var("BUILTIN_PROBES__LLM_POD_LABELS");
        }
    }

    /// `try_parsing` turns a single list value that looks like a number or bool into one.
    #[test]
    fn test_single_numeric_pod_label_still_loads() {
        for value in ["2024", "True", "007"] {
            let vars = Map::from([("BUILTIN_PROBES__LLM_POD_LABELS".into(), value.into())]);
            let pod_labels = Settings::from_vars(vars)
                .expect("settings load")
                .builtin_probes
                .llm_pod_labels;
            assert_eq!(pod_labels, Some(vec![value.to_string()]));
        }
    }

    #[test]
    fn test_to_common_config() {
        let settings = Settings {
            otel_exporter_otlp_endpoint: None,
            otel_exporter_otlp_protocol: None,
            builtin_probes: BuiltinProbes {
                filesystem: FilesystemProbes {
                    file_access: Some(true),
                    watched_paths: None,
                },
                llm: None,
                llm_executables: None,
                llm_pod_labels: None,
                process_lifecycle: None,
                interval: None,
            },
            custom_probe_config: None,
            debug: None,
        };

        let _common = settings.to_common_config();
        // Basic validation that it doesn't panic
    }
}
