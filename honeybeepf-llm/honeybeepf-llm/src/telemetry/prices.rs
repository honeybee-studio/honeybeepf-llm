//! User price file loading and periodic reload.

use arc_swap::ArcSwapOption;
use honeybeepf_llm_core::pricing::PriceTable;
use log::info;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{HoneyBeeMetrics, Prices};

/// How often the user price file is reread; kubelet refreshes a ConfigMap volume only about once a minute.
const PRICE_RELOAD_INTERVAL_SECS: u64 = 10;

/// The user price file named by `LLM_PRICES_CONFIG_FILE`, read again whenever its contents change.
pub(super) struct PriceFile {
    path: String,
    /// Contents last read, so a rewrite with the same bytes reloads nothing.
    seen: Option<String>,
    unreadable: bool,
}

impl PriceFile {
    pub(super) fn new(path: String) -> Self {
        Self {
            path,
            seen: None,
            unreadable: false,
        }
    }

    /// The file's contents if they differ from the last read; a read error is logged once.
    pub(super) fn changed(&mut self) -> Option<&str> {
        match std::fs::read_to_string(&self.path) {
            Ok(json) => {
                self.unreadable = false;
                if self.seen.as_ref() == Some(&json) {
                    return None;
                }
                Some(self.seen.insert(json).as_str())
            }
            Err(e) => {
                if !std::mem::replace(&mut self.unreadable, true) {
                    log::warn!("Custom price file {} not read: {e}", self.path);
                }
                None
            }
        }
    }
}

/// Inline user prices in `LLM_PRICES_CONFIG` (chart), used when no price file was read.
pub(super) fn inline_prices() -> Option<String> {
    std::env::var("LLM_PRICES_CONFIG")
        .ok()
        .filter(|json| !json.is_empty())
}

/// Swap in the built-in table merged with the user file when the file changed and passes the checks.
pub(super) fn reload_prices(prices: &ArcSwapOption<Prices>, file: &mut PriceFile) {
    let Some(current) = prices.load_full() else {
        return;
    };
    let Some(json) = file.changed() else {
        return;
    };
    let merged = PriceTable::builtin().and_then(|mut t| t.merge_custom(json).map(|o| (t, o)));
    match merged {
        // Back to the contents already loaded, after a rejected file in between.
        Ok((table, _)) if table.version() == current.table.version() => {}
        Ok((table, o)) => {
            info!(
                "Custom prices reloaded: price table {} -> {}, replaced [{}], added [{}]",
                current.table.version(),
                table.version(),
                o.replaced.join(", "),
                o.added.join(", ")
            );
            prices.store(Some(Arc::new(Prices::new(table))));
        }
        Err(e) => log::warn!(
            "Custom price file {} rejected, keeping price table {}: {e}",
            file.path,
            current.table.version()
        ),
    }
}

/// Reread the user price file every PRICE_RELOAD_INTERVAL_SECS until shutdown.
pub(super) fn start_price_reload(metrics: &'static HoneyBeeMetrics, mut file: PriceFile) {
    let shutdown = crate::probes::shutdown_flag();
    std::thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(PRICE_RELOAD_INTERVAL_SECS));
            reload_prices(&metrics.prices, &mut file);
        }
    });
}

/// Load the built-in price table and merge user prices over it; a rejected user file keeps the built-in one.
pub(super) fn load_prices(custom: Option<&str>) -> Option<Prices> {
    let mut table = match PriceTable::builtin() {
        Ok(table) => table,
        Err(e) => {
            log::warn!("Built-in price table rejected, LLM cost not recorded: {e}");
            return None;
        }
    };
    if let Some(json) = custom {
        match table.merge_custom(json) {
            Ok(o) => info!(
                "Custom prices: replaced [{}], added [{}]",
                o.replaced.join(", "),
                o.added.join(", ")
            ),
            Err(e) => log::warn!("Custom price table rejected, using built-in prices only: {e}"),
        }
    }
    for s in table.sources() {
        info!(
            "Price table {}: {} prices from {} (checked {})",
            table.version(),
            s.provider,
            s.url,
            s.checked
        );
    }
    Some(Prices::new(table))
}
