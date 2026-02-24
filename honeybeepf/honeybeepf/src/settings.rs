use config::{Config, ConfigError, Environment};
use serde::Deserialize;

const DEFAULT_PROBE_INTERVAL_SECONDS: u32 = 60;

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct BuiltinProbes {
    pub block_io: Option<bool>,
    pub network_latency: Option<bool>,
    pub llm: Option<bool>,
    pub gpu_usage: Option<bool>,
    pub interval: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    pub otel_exporter_otlp_endpoint: Option<String>,
    pub otel_exporter_otlp_protocol: Option<String>,
    pub builtin_probes: BuiltinProbes,
    pub custom_probe_config: Option<String>,
    pub debug: Option<bool>,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        // Load .env file if it exists
        dotenvy::dotenv().ok();

        let s = Config::builder()
            // Map flat environment variables to nested structure keys
            // Use __ as separator for nested keys (e.g. BUILTIN_PROBES__BLOCK_IO -> builtin_probes.block_io)
            .add_source(Environment::default().separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn to_common_config(&self) -> honeybeepf_common::CommonConfig {
        // Convert Option<bool> / Option<u32> to primitive POD types
        let probe_block_io = self.builtin_probes.block_io.unwrap_or(false);
        let probe_network_latency = self.builtin_probes.network_latency.unwrap_or(false);
        let probe_gpu_usage = self.builtin_probes.gpu_usage.unwrap_or(false);
        let probe_llm = self.builtin_probes.llm.unwrap_or(false);
        // Use a sensible non-zero default interval (in seconds) when not configured
        let probe_interval = self
            .builtin_probes
            .interval
            .unwrap_or(DEFAULT_PROBE_INTERVAL_SECONDS);

        honeybeepf_common::CommonConfig {
            probe_block_io: probe_block_io as u8,
            probe_network_latency: probe_network_latency as u8,
            probe_llm: probe_llm as u8,
            probe_gpu_usage: probe_gpu_usage as u8,
            probe_interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    #[test]
    #[serial]
    fn test_load_settings() {
        // Ensure .env is loaded
        dotenvy::dotenv().ok();

        // Manual override for testing deterministic values
        unsafe {
            std::env::set_var("BUILTIN_PROBES__BLOCK_IO", "true");
            std::env::set_var("BUILTIN_PROBES__INTERVAL", "42");
        }

        let settings = Settings::new().expect("Failed to load settings");

        assert_eq!(settings.builtin_probes.block_io, Some(true));
        assert_eq!(settings.builtin_probes.interval, Some(42));
    }

    #[test]
    fn test_to_common_config() {
        let settings = Settings {
            otel_exporter_otlp_endpoint: None,
            otel_exporter_otlp_protocol: None,
            builtin_probes: BuiltinProbes {
                block_io: Some(true),
                network_latency: None, // Should default to false (0)
                gpu_usage: None,       // Should default to false
                llm: None,             // Should default to false
                interval: None,        // Should default to constant
            },
            custom_probe_config: None,
        };

        let common = settings.to_common_config();

        assert_eq!(common.probe_block_io, 1);
        assert_eq!(common.probe_network_latency, 0);
        assert_eq!(common.probe_llm, 0);
        assert_eq!(common.probe_interval, DEFAULT_PROBE_INTERVAL_SECONDS);
    }
}
