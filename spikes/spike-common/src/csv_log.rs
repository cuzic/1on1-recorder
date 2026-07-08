// spike-windows-01-02-detail-design.md §3.4

use crate::frame_record::CapturedFrameRecord;

pub struct FrameCsvWriter {
    writer: csv::Writer<std::fs::File>,
}

impl FrameCsvWriter {
    pub fn create(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut writer = csv::Writer::from_path(path)?;
        writer.write_record([
            "stream",
            "wake_seq",
            "packet_seq",
            "capture_qpc_100ns",
            "wake_qpc_100ns",
            "device_position_frames",
            "frame_count",
            "raw_flags",
            "discontinuity",
            "silent",
            "timestamp_error",
            "capture_epoch",
            "target_pid",
        ])?;
        Ok(Self { writer })
    }

    pub fn write(&mut self, record: &CapturedFrameRecord) -> anyhow::Result<()> {
        self.writer.write_record(&[
            format!("{:?}", record.stream),
            record.wake_seq.to_string(),
            record.packet_seq.to_string(),
            record.capture_qpc_100ns.to_string(),
            record.wake_qpc_100ns.to_string(),
            record.device_position_frames.to_string(),
            record.frame_count.to_string(),
            record.raw_flags.to_string(),
            record.discontinuity.to_string(),
            record.silent.to_string(),
            record.timestamp_error.to_string(),
            record.capture_epoch.to_string(),
            record
                .target_pid
                .map(|p| p.to_string())
                .unwrap_or_default(),
        ])?;
        Ok(())
    }

    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
