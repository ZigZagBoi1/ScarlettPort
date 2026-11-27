//! One tracing setup for every binary.
//!
//! Filter with `SCARLETT_LOG` (falls back to `RUST_LOG`), e.g.
//! `SCARLETT_LOG=scarlett_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for. `scarlett_gpu::host`
//! logs at info unless the filter names `scarlett_gpu` itself.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

/// The GPU host's lines (frame times, refused pipelines, heap caps, the
/// upscaler) are what a player's log is read for; a filter that does not
/// name `scarlett_gpu` itself (`SCARLETT_LOG=warn`, the launcher's) keeps
/// them at info.
const GPU_HOST: &str = "scarlett_gpu::host=info";

/// The filter text: `asked`, or `default_level`, with [`GPU_HOST`] unless it
/// already says something of `scarlett_gpu`.
pub fn filter_text(asked: Option<&str>, default_level: &str) -> String {
    let base = asked.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(default_level);
    if base.contains("scarlett_gpu") {
        base.to_owned()
    } else {
        format!("{base},{GPU_HOST}")
    }
}

pub fn init(default_level: &str) {
    let asked = std::env::var("SCARLETT_LOG").or_else(|_| std::env::var("RUST_LOG")).ok();
    let filter = EnvFilter::try_new(filter_text(asked.as_deref(), default_level))
        .or_else(|_| EnvFilter::try_new(filter_text(None, default_level)))
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = tracing_subscriber::registry()
        .with(CAP)
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::filter_text;

    #[test]
    fn gpu_host_logs_info() {
        assert_eq!(filter_text(None, "info"), "info,scarlett_gpu::host=info");
        assert_eq!(filter_text(Some("warn"), "info"), "warn,scarlett_gpu::host=info");
        assert_eq!(filter_text(Some("warn,scarlett_gpu=error"), "info"), "warn,scarlett_gpu=error");
        assert_eq!(filter_text(Some(" "), "info"), "info,scarlett_gpu::host=info");
    }
}
