use super::{App, MessageKind, MetricsSnapshot};

impl App {
    /// Load a metrics snapshot from the DB without holding any locks longer than needed.
    pub(crate) fn load_metrics_snapshot(&self) -> MetricsSnapshot {
        let (metrics, provider_models) = self.db.load_all();
        MetricsSnapshot {
            metrics,
            provider_models,
        }
    }

    /// Apply a previously loaded snapshot: merge DB metrics into the in-memory
    /// `SharedMetrics`, log newly appearing provider errors, and refresh
    /// `models.provider_models`. Returns `true` if anything changed.
    pub(crate) fn apply_metrics_snapshot(&mut self, snapshot: MetricsSnapshot) -> bool {
        let MetricsSnapshot {
            metrics,
            provider_models,
        } = snapshot;

        let error_snapshot: Vec<(String, u16, String)> = if let Ok(mut m) = self.metrics.lock() {
            let saved_errors = std::mem::take(&mut m.last_error);
            *m = metrics;
            m.last_error = saved_errors;
            m.last_error
                .iter()
                .map(|(p, e)| (p.clone(), e.status, e.message.clone()))
                .collect()
        } else {
            vec![]
        };

        self.seen_provider_errors
            .retain(|p, _| error_snapshot.iter().any(|(ep, _, _)| ep == p));

        for (provider, status, message) in error_snapshot {
            let key = format!("{status}:{message}");
            if self
                .seen_provider_errors
                .get(&provider)
                .is_none_or(|k| k != &key)
            {
                self.seen_provider_errors.insert(provider.clone(), key);
                let text = if status == 0 {
                    format!("[{provider}] network error — {message}")
                } else {
                    format!("[{provider}] HTTP {status} — {message}")
                };
                self.push_message_log(text, MessageKind::Error);
            }
        }

        self.models.provider_models = provider_models;
        true
    }

    /// Convenience: load a snapshot from DB and apply it immediately (synchronous path).
    pub(crate) fn reload_metrics_from_db(&mut self) {
        let snapshot = self.load_metrics_snapshot();
        self.apply_metrics_snapshot(snapshot);
    }
}
