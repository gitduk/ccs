// All metrics types are defined in `crate::metrics` — a neutral shared module.
// This re-export keeps existing `crate::proxy::metrics::X` paths working for
// proxy-internal code without requiring a wholesale import-site migration there.
pub use crate::metrics::{
    LastRequestError, ModelStats, ProviderStats, RequestLog, RequestLogEntry, SharedMetrics,
    SharedRequestLog, TokenMetrics,
};
