// spike-windows-01-02-detail-design.md §5.9 (process_events.jsonl) /
// spike-plan.md SPIKE-09 (device_events.jsonl) 共通のイベントログ書式。
// 1行1JSONで追記する。

pub struct JsonlWriter {
    writer: std::io::BufWriter<std::fs::File>,
}

impl JsonlWriter {
    pub fn create(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            writer: std::io::BufWriter::new(std::fs::File::create(path)?),
        })
    }

    /// 書き込み失敗自体で録音を止めたくないため、エラーはログのみに留める
    /// (§3.8の他のI/O経路と同じ方針)。
    pub fn write(&mut self, event: serde_json::Value) {
        use std::io::Write;
        if let Err(e) = serde_json::to_writer(&mut self.writer, &event) {
            tracing::warn!(error = %e, "failed to write jsonl entry");
            return;
        }
        let _ = self.writer.write_all(b"\n");
        let _ = self.writer.flush();
    }

    pub fn now_ns() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
}
