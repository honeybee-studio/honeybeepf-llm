use config::{Config, ConfigError, Environment};
use serde::Deserialize;

const DEFAULT_PROBE_INTERVAL_SECONDS: u32 = 60;

/// Filesystem probe configuration
#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct FilesystemProbes {
    pub file_access: Option<bool>,
    pub watched_paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct BuiltinProbes {
    #[serde(default)]
    pub filesystem: FilesystemProbes,
    pub llm: Option<bool>,
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

        let s = Config::builder()
            .add_source(
                Environment::default()
                    .separator("__")
                    .list_separator(",")
                    .try_parsing(true),
            )
            .build()?;

        let settings: Self = s.try_deserialize()?;
        eprintln!("Parsed settings: {:?}", settings);
        Ok(settings)
    }

    pub fn to_common_config(&self) -> honeybeepf_common::CommonConfig {
        // LLM probe
        let probe_llm = self.builtin_probes.llm.unwrap_or(false);

        let probe_interval = self
            .builtin_probes
            .interval
            .unwrap_or(DEFAULT_PROBE_INTERVAL_SECONDS);

        honeybeepf_common::CommonConfig {
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
        }

        let settings = Settings::new().expect("Failed to load settings");

        assert_eq!(settings.builtin_probes.filesystem.file_access, Some(true));
        assert_eq!(settings.builtin_probes.interval, Some(42));

        // 환경변수 정리
        unsafe {
            std::env::remove_var("BUILTIN_PROBES__FILESYSTEM__FILE_ACCESS");
            std::env::remove_var("BUILTIN_PROBES__INTERVAL");
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
                interval: None,
            },
            custom_probe_config: None,
            debug: None,
        };

        let _common = settings.to_common_config();
        // Basic validation that it doesn't panic
    }
}
