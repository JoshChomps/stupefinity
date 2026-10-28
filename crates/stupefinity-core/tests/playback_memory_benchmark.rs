use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stupefinity_core::eq::EqualizerEngine;
use stupefinity_core::player::AudioPlayer;
use stupefinity_core::streaming::{ProgressiveStreamReader, StreamCoordinator};
use stupefinity_protocol::EqProfile;
use tokio::sync::mpsc;

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
}

extern "system" {
    fn K32GetProcessMemoryInfo(
        process: *mut std::ffi::c_void,
        counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
}

fn get_process_memory() -> (f64, f64) {
    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    unsafe {
        let handle = GetCurrentProcess();
        let res = K32GetProcessMemoryInfo(handle, &mut counters, counters.cb);
        if res != 0 {
            let private_mb = counters.pagefile_usage as f64 / (1024.0 * 1024.0);
            let working_mb = counters.working_set_size as f64 / (1024.0 * 1024.0);
            (private_mb, working_mb)
        } else {
            (0.0, 0.0)
        }
    }
}

fn create_test_wav(duration_secs: u32, sample_rate: u32) -> Vec<u8> {
    let num_channels: u16 = 2;
    let bits_per_sample: u16 = 16;
    let total_samples = duration_secs * sample_rate * num_channels as u32;
    let data_len = total_samples * (bits_per_sample as u32 / 8);
    let mut wav = Vec::with_capacity((44 + data_len) as usize);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&num_channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * num_channels as u32 * (bits_per_sample as u32 / 8);
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    let block_align = num_channels * (bits_per_sample / 8);
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());

    for i in 0..(duration_secs * sample_rate) {
        let t = i as f32 / sample_rate as f32;
        let sample = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.15;
        let pcm = (sample * 32767.0) as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    wav
}

/// Measures the memory of this test process while it streams and plays audio through the
/// real player, EQ and progressive reader. It is a lower bound for the audio pipeline, not a
/// measurement of the app (which also runs the UI, IPC and network clients): use
/// `scripts/benchmark_memory.ps1` for that. Needs an audio output device, so it is ignored by
/// default; run it with `cargo test --test playback_memory_benchmark -- --ignored --nocapture`.
#[test]
#[ignore = "needs an audio output device"]
fn test_active_playback_memory_budget() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut eq_engine = EqualizerEngine::new(44100.0);
    eq_engine.set_profile(EqProfile::default());
    eq_engine.set_enabled(true);
    let eq = Arc::new(Mutex::new(eq_engine));

    let player = AudioPlayer::new(tx, eq);
    player.set_volume(0.5);

    // Create a 5-second PCM WAV audio file
    let audio_bytes = create_test_wav(5, 44100);
    let total_audio_len = audio_bytes.len() as u64;

    let temp_path =
        std::env::temp_dir().join(format!("active_stream_bench_{}.tmp", std::process::id()));
    let mut writer_file = File::create(&temp_path).expect("Create temp benchmark file");
    let reader_file = File::open(&temp_path).expect("Open temp benchmark file");

    let coord = Arc::new(StreamCoordinator::new());
    coord.set_total_len(total_audio_len);

    let stream_reader = ProgressiveStreamReader::new(reader_file, coord.clone(), temp_path.clone());

    // Spawn progressive streaming writer simulating network chunk delivery
    let coord_clone = coord.clone();
    let write_handle = std::thread::spawn(move || {
        let chunk_size = 16 * 1024;
        for chunk in audio_bytes.chunks(chunk_size) {
            writer_file.write_all(chunk).unwrap();
            writer_file.flush().unwrap();
            coord_clone.add_downloaded(chunk.len() as u64);
            std::thread::sleep(Duration::from_millis(20));
        }
        coord_clone.mark_finished();
    });

    // Wait until initial buffer (64 KB) is reached
    while coord.downloaded_bytes() < 64 * 1024 {
        std::thread::sleep(Duration::from_millis(5));
    }

    // Begin active playback through WASAPI + Symphonia + Equalizer
    player.play_reader(Box::new(stream_reader));

    // Sample memory metrics for 2 seconds while actively streaming and playing audio
    let mut peak_private_mb: f64 = 0.0;
    let mut peak_working_mb: f64 = 0.0;

    for _ in 0..20 {
        let (private_mb, working_mb) = get_process_memory();
        if private_mb > peak_private_mb {
            peak_private_mb = private_mb;
        }
        if working_mb > peak_working_mb {
            peak_working_mb = working_mb;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    write_handle.join().unwrap();
    player.stop();

    println!("\n=======================================================");
    println!("  Active Audio Streaming & Playback Memory Benchmark");
    println!("=======================================================");
    println!(
        "  Peak Private Bytes (Committed): {:.2} MB",
        peak_private_mb
    );
    println!(
        "  Peak Working Set   (Physical):  {:.2} MB",
        peak_working_mb
    );
    println!("  Budget Threshold:              <= 10.0 MB Private Bytes");
    println!("=======================================================\n");

    assert!(
        peak_private_mb <= 10.0,
        "Active streaming Private Bytes exceeded budget! Measured: {:.2} MB (> 10.0 MB)",
        peak_private_mb
    );
}
