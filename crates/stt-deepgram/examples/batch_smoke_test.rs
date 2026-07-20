//! Ad hoc smoke test for `DeepgramBatchProvider::transcribe_batch` against the real
//! Deepgram API, added to verify `batch.rs`'s doc comment caveat ("an actual API key
//! has not been used to verify this against a live response") — this crate's own
//! tests only ever exercise the local axum mock server.
//!
//! Usage: `DEEPGRAM_API_KEY=... cargo run -p stt-deepgram --example batch_smoke_test -- <path-to-16kHz-mono-s16le-pcm-file>`

use std::env;
use std::fs;

use stt_deepgram::DeepgramBatchProvider;
use stt_api::{BatchAudioInput, BatchSttProvider, SttSessionConfig};

#[tokio::main]
async fn main() {
    let api_key = env::var("DEEPGRAM_API_KEY").expect("set DEEPGRAM_API_KEY");
    let pcm_path = env::args().nth(1).expect("usage: batch_smoke_test <pcm-file>");

    let raw = fs::read(&pcm_path).expect("read pcm file");
    let pcm: Vec<f32> = raw
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32)
        .collect();
    println!("loaded {} samples ({:.2}s at 16kHz)", pcm.len(), pcm.len() as f32 / 16_000.0);

    let provider = DeepgramBatchProvider::new(api_key);
    let audio = BatchAudioInput { pcm: &pcm, sample_rate_hz: 16_000, channels: 1 };
    let config = SttSessionConfig::new(16_000).with_language("ja").with_diarization(true);

    match provider.transcribe_batch(audio, config).await {
        Ok(transcript) => {
            println!("transcript: {:?}", transcript.text);
            if let Some(words) = transcript.words {
                println!("word count: {}", words.len());
                for w in words.iter().take(5) {
                    println!("  {:?} [{:?}..{:?}] speaker={:?} conf={:?}", w.text, w.start_ms, w.end_ms, w.speaker, w.confidence);
                }
            } else {
                println!("no per-word timing returned");
            }
        }
        Err(err) => {
            eprintln!("transcribe_batch failed: {err}");
            std::process::exit(1);
        }
    }
}
