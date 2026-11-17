//! Log setup: `RUST_LOG` picks the levels and `LOG_FORMAT` the format; audit events stay on unless `RUST_LOG` names them.

use tracing_subscriber::EnvFilter;

/// How each log line is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// For people: level, target, message, then any fields.
    Text,
    /// One JSON object per line, for a log pipeline or a SIEM.
    Json,
}

impl LogFormat {
    /// The format `LOG_FORMAT` asks for; unset or empty is text.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).unwrap_or_default() {
            "" | "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "LOG_FORMAT={other:?} is not text or json; using text"
            )),
        }
    }
}

/// The filter from `rust_log`, or `info`/`warn` by `verbose`, with audit events at info unless it sets them itself.
pub fn filter(rust_log: Option<&str>, verbose: bool) -> EnvFilter {
    let base = rust_log
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(if verbose { "info" } else { "warn" }));
    if rust_log.is_some_and(|v| v.contains(crate::audit::TARGET)) {
        return base;
    }
    match format!("{}=info", crate::audit::TARGET).parse() {
        Ok(directive) => base.add_directive(directive),
        Err(_) => base,
    }
}

/// Install the global logger; `log` macros reach it through the bridge `init` sets up.
pub fn init(verbose: bool) {
    let rust_log = std::env::var("RUST_LOG").ok();
    let format = LogFormat::parse(std::env::var("LOG_FORMAT").ok().as_deref());
    let filter = filter(rust_log.as_deref(), verbose);
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if format == Ok(LogFormat::Json) {
        builder
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .init();
    } else {
        builder.init();
    }
    if let Err(e) = format {
        log::warn!("{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_is_text_unless_json_is_asked_for() {
        assert_eq!(LogFormat::parse(None), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("")), Ok(LogFormat::Text));
        assert_eq!(LogFormat::parse(Some("json")), Ok(LogFormat::Json));
        assert_eq!(LogFormat::parse(Some(" json ")), Ok(LogFormat::Json));
        assert!(LogFormat::parse(Some("JSON5")).is_err());
    }

    #[test]
    fn audit_events_stay_on_unless_rust_log_sets_them() {
        assert!(
            filter(Some("warn"), false)
                .to_string()
                .contains("audit=info")
        );
        assert!(filter(None, false).to_string().contains("audit=info"));
        let own = filter(Some("warn,audit=off"), false).to_string();
        assert!(
            own.contains("audit=off") && !own.contains("audit=info"),
            "{own}"
        );
    }
}
