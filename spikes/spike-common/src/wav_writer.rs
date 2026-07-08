// spike-windows-01-02-detail-design.md §3.5
//
// 全ストリーム共通で interleaved float32 (IEEE float WAV) に統一する
// (§4.6のStreamSink設計を参照。ネイティブビット幅を保存する経路は用意しない)。

pub struct PcmWavWriter {
    inner: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl PcmWavWriter {
    pub fn create_from_format(
        path: &std::path::Path,
        channels: u16,
        sample_rate: u32,
    ) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let inner = hound::WavWriter::create(path, spec)?;
        Ok(Self { inner })
    }

    pub fn write_samples(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        for &s in samples {
            self.inner.write_sample(s)?;
        }
        Ok(())
    }

    pub fn finalize(self) -> anyhow::Result<()> {
        self.inner.finalize()?;
        Ok(())
    }
}
