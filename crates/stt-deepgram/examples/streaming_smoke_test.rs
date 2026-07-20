//! Ad hoc smoke test for the streaming `DeepgramProvider`/`DeepgramSession` against
//! the real Deepgram API, using real speech (unlike `deepgram_poc.rs`'s sine wave) and
//! exercising `keep_alive()` mid-stream — added to verify the connect/finalize
//! timeouts introduced for tasks #81/#85 don't misfire against a live server, and that
//! `keep_alive()`'s `{"type":"KeepAlive"}` control frame is accepted mid-session.
//!
//! Usage: `DEEPGRAM_API_KEY=... cargo run -p stt-deepgram --example streaming_smoke_test -- <path-to-16kHz-mono-s16le-pcm-file>`

use std::env;
use std::fs;
use std::time::{Duration, Instant};

use stt_api::{AudioChunk, KeepAliveEffect, SttEvent, SttProvider, SttSessionConfig};
use stt_deepgram::DeepgramProvider;

const SAMPLE_RATE_HZ: u32 = 16_000;
const CHUNK_MS: u32 = 20;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = env::var("DEEPGRAM_API_KEY").expect("set DEEPGRAM_API_KEY");
    let pcm_path = env::args().nth(1).expect("usage: streaming_smoke_test <pcm-file>");

    let raw = fs::read(&pcm_path)?;
    let samples: Vec<f32> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32).collect();
    println!("loaded {} samples ({:.2}s)", samples.len(), samples.len() as f32 / SAMPLE_RATE_HZ as f32);

    let provider = DeepgramProvider::new(api_key);
    let config = SttSessionConfig::new(SAMPLE_RATE_HZ)
        .with_language("ja")
        .with_interim_results(true)
        .with_diarization(true)
        .with_vad_events(true);

    let connect_start = Instant::now();
    let (mut session, mut events) = provider.start_session(config).await?;
    println!("connected in {:?}", connect_start.elapsed());

    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                SttEvent::PartialTranscript { text, .. } if !text.is_empty() => println!("[partial] {text:?}"),
                SttEvent::PartialTranscript { .. } => {}
                SttEvent::FinalTranscript { text, .. } if !text.is_empty() => println!("[final]   {text:?}"),
                SttEvent::FinalTranscript { .. } => {}
                SttEvent::SpeechStarted => println!("[event]   SpeechStarted"),
                SttEvent::SpeechEnded => println!("[event]   SpeechEnded"),
                SttEvent::Error(err) => println!("[error]   {err} (retryable={})", err.is_retryable()),
            }
        }
    });

    let chunk_samples = (SAMPLE_RATE_HZ * CHUNK_MS / 1000) as usize;
    let mut start_sample: u64 = 0;
    for chunk in samples.chunks(chunk_samples) {
        session.send_audio(AudioChunk { pcm: chunk, start_sample }).await?;
        start_sample += chunk.len() as u64;
        tokio::time::sleep(Duration::from_millis(CHUNK_MS as u64)).await;
    }

    println!("pausing 2s, then calling keep_alive() mid-session...");
    tokio::time::sleep(Duration::from_secs(2)).await;
    match session.keep_alive().await {
        Ok(KeepAliveEffect::ControlMessage) => println!("keep_alive: sent KeepAlive control frame, ok"),
        Ok(other) => println!("keep_alive: unexpected effect {other:?}"),
        Err(err) => println!("keep_alive failed: {err}"),
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let finalize_start = Instant::now();
    session.finalize().await?;
    println!("finalize: ok in {:?}", finalize_start.elapsed());

    printer.await?;
    Ok(())
}
