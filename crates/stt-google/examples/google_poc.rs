//! Live connectivity spike for the Google adapter. Connects to the real Speech-to-
//! Text v2 `StreamingRecognize` API, feeds it a synthesized sine wave (no real
//! speech is available in this environment), and prints every `SttEvent` as it
//! arrives.
//!
//! Since the audio is a pure tone rather than speech, Google is not expected to
//! return a meaningful transcript — this spike is only about confirming the gRPC
//! handshake, request/response framing, and auth round-trip against the live API.
//!
//! Uses Application Default Credentials (`gcloud auth application-default login`,
//! or `GOOGLE_APPLICATION_CREDENTIALS` pointing at a service-account key).
//!
//! Run with:
//!
//! ```sh
//! GOOGLE_STT_PROJECT_ID=my-project cargo run --example google_poc -p stt-google
//! # optionally: GOOGLE_STT_LOCATION=asia-northeast1
//! ```

use std::time::Duration;

use stt_api::{AudioChunk, SttEvent, SttProvider, SttSessionConfig};
use stt_google::{GoogleProvider, GoogleSttCredentials};

const SAMPLE_RATE_HZ: u32 = 16_000;
const TONE_HZ: f32 = 440.0;
const DURATION_SECS: f32 = 3.0;
const CHUNK_MS: u32 = 20;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project_id = match std::env::var("GOOGLE_STT_PROJECT_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!(
                "GOOGLE_STT_PROJECT_ID is not set; export a real GCP project id to run this spike \
                 (Application Default Credentials must also be configured, e.g. via \
                 `gcloud auth application-default login` or GOOGLE_APPLICATION_CREDENTIALS)"
            );
            std::process::exit(1);
        }
    };
    let location = std::env::var("GOOGLE_STT_LOCATION").unwrap_or_else(|_| "global".to_string());

    let provider = GoogleProvider::new(GoogleSttCredentials::new(project_id, location));
    let config = SttSessionConfig::new(SAMPLE_RATE_HZ)
        .with_language("ja")
        .with_interim_results(true)
        .with_diarization(true)
        .with_vad_events(true);

    let (mut session, mut events) = provider.start_session(config).await?;

    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                SttEvent::PartialTranscript {
                    text,
                    audio_start_ms,
                    audio_end_ms,
                    ..
                } => {
                    println!("[partial] {audio_start_ms:?}-{audio_end_ms:?} {text:?}");
                }
                SttEvent::FinalTranscript {
                    text,
                    words,
                    audio_start_ms,
                    audio_end_ms,
                    ..
                } => {
                    println!("[final]   {audio_start_ms:?}-{audio_end_ms:?} {text:?}");
                    for word in words.into_iter().flatten() {
                        println!(
                            "          word={:?} speaker={:?} {:?}-{:?}",
                            word.text, word.speaker, word.start_ms, word.end_ms
                        );
                    }
                }
                SttEvent::SpeechStarted => println!("[event]   SpeechStarted"),
                SttEvent::SpeechEnded => println!("[event]   SpeechEnded"),
                SttEvent::Error(err) => {
                    println!("[error]   {err} (retryable={})", err.is_retryable());
                }
            }
        }
    });

    let samples = generate_sine_wave(SAMPLE_RATE_HZ, DURATION_SECS, TONE_HZ);
    let chunk_samples = (SAMPLE_RATE_HZ * CHUNK_MS / 1000) as usize;
    let mut start_sample: u64 = 0;
    for chunk in samples.chunks(chunk_samples) {
        session
            .send_audio(AudioChunk {
                pcm: chunk,
                start_sample,
            })
            .await?;
        start_sample += chunk.len() as u64;
        tokio::time::sleep(Duration::from_millis(CHUNK_MS as u64)).await;
    }

    session.finalize().await?;
    println!("finalize: ok");

    printer.await?;
    Ok(())
}

fn generate_sine_wave(sample_rate_hz: u32, duration_secs: f32, freq_hz: f32) -> Vec<f32> {
    let n = (sample_rate_hz as f32 * duration_secs) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate_hz as f32;
            (2.0 * std::f32::consts::PI * freq_hz * t).sin() * 0.5
        })
        .collect()
}
