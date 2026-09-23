//! Prometheus counters, by hand: a handful of labelled counts behind one lock. No request text is ever a label.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;

#[derive(Default)]
pub struct Metrics {
    counts: Mutex<BTreeMap<(&'static str, String), u64>>,
    /// Microseconds spent in tool calls, and in atlas within them.
    micros: Mutex<BTreeMap<&'static str, u64>>,
}

const HELP: &[(&str, &str)] = &[
    ("mcp_requests_total", "HTTP requests, by route and status."),
    ("mcp_tool_calls_total", "Tool calls, by tool and outcome (ok, error)."),
    ("mcp_auth_refused_total", "Requests to /mcp refused a token, by reason."),
    ("mcp_rate_limited_total", "Requests refused by the per-session rate limit."),
    ("mcp_busy_total", "Requests or tool calls turned away because as many as allowed were already running."),
];

impl Metrics {
    pub fn count(&self, name: &'static str, labels: String) {
        *self.counts.lock().unwrap_or_else(|e| e.into_inner()).entry((name, labels)).or_default() += 1;
    }

    pub fn time(&self, name: &'static str, micros: u64) {
        *self.micros.lock().unwrap_or_else(|e| e.into_inner()).entry(name).or_default() += micros;
    }

    /// `cached` is the atlas cache's entries and bytes; `used` its answers from the cache, revalidated, fetched, and
    /// stale on an atlas error.
    pub fn render(&self, cached: (usize, usize), used: [u64; 4]) -> String {
        let mut out = String::with_capacity(2048);
        let _ = writeln!(
            out,
            "# HELP mcp_atlas_answers_total Atlas answers used, by where they came from.\n\
             # TYPE mcp_atlas_answers_total counter\n\
             mcp_atlas_answers_total{{source=\"cache\"}} {}\n\
             mcp_atlas_answers_total{{source=\"revalidated\"}} {}\n\
             mcp_atlas_answers_total{{source=\"fetched\"}} {}\n\
             mcp_atlas_answers_total{{source=\"stale\"}} {}",
            used[0], used[1], used[2], used[3]
        );
        let _ = writeln!(
            out,
            "# HELP mcp_build_info The running build.\n# TYPE mcp_build_info gauge\nmcp_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        );
        let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        for (name, help) in HELP {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            for ((_, labels), n) in counts.iter().filter(|((n, _), _)| n == name) {
                let _ = writeln!(out, "{name}{{{labels}}} {n}");
            }
        }
        for (name, micros) in self.micros.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let _ = writeln!(
                out,
                "# HELP {name} Seconds spent answering tool calls, atlas included.\n# TYPE {name} counter\n{name} {:.6}",
                *micros as f64 / 1e6
            );
        }
        let _ = writeln!(
            out,
            "# HELP mcp_cache_entries Atlas answers held.\n# TYPE mcp_cache_entries gauge\nmcp_cache_entries {}\n\
             # HELP mcp_cache_bytes Bytes of atlas answers held.\n# TYPE mcp_cache_bytes gauge\nmcp_cache_bytes {}",
            cached.0, cached.1
        );
        out
    }
}
