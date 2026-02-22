use std::path::Path;

use rubato::{
    Async as AsyncResampler, FixedAsync, Resampler as RubatoResampler,
    SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use audioadapter_buffers::direct::InterleavedSlice;

/// Reads a WAV file and returns f32 mono samples resampled to 16kHz.
///
/// Supports 16-bit integer and 32-bit float formats. Stereo is down-mixed to mono.
/// Audio at any sample rate is resampled to 16kHz for ASR consumption.
pub fn read_wav_16k_mono(path: impl AsRef<Path>) -> anyhow::Result<(Vec<f32>, u32)> {
    let reader = hound::WavReader::open(path.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to open WAV '{}': {}", path.as_ref().display(), e))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let sample_rate = spec.sample_rate;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max_val)
                .collect()
        }
        hound::SampleFormat::Float => {
            reader
                .into_samples::<f32>()
                .map(|s| s.unwrap_or(0.0))
                .collect()
        }
    };

    // Down-mix to mono if stereo or multi-channel
    let mono = if channels > 1 {
        samples
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        samples
    };

    // Resample to 16kHz if needed
    if sample_rate != 16000 {
        let resampled = resample_to_16k(&mono, sample_rate)?;
        Ok((resampled, 16000))
    } else {
        Ok((mono, 16000))
    }
}

/// Reads a WAV file that must already be 16kHz mono. Errors if sample rate != 16000.
pub fn read_wav_16k_mono_strict(path: impl AsRef<Path>) -> anyhow::Result<Vec<f32>> {
    let reader = hound::WavReader::open(path.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to open WAV '{}': {}", path.as_ref().display(), e))?;
    let spec = reader.spec();
    if spec.sample_rate != 16000 {
        anyhow::bail!(
            "Expected 16kHz WAV but got {}Hz in '{}'",
            spec.sample_rate,
            path.as_ref().display()
        );
    }
    let channels = spec.channels as usize;

    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max_val)
                .collect()
        }
        hound::SampleFormat::Float => {
            reader
                .into_samples::<f32>()
                .map(|s| s.unwrap_or(0.0))
                .collect()
        }
    };

    let mono = if channels > 1 {
        samples
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    } else {
        samples
    };

    Ok(mono)
}

/// Resamples mono audio from `src_rate` Hz to 16kHz using sinc interpolation.
fn resample_to_16k(audio: &[f32], src_rate: u32) -> anyhow::Result<Vec<f32>> {
    let ratio = 16000.0 / src_rate as f64;
    let chunk_size = 1024;

    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };

    let mut resampler = AsyncResampler::<f32>::new_sinc(
        ratio,
        2.0,
        &params,
        chunk_size,
        1, // mono
        FixedAsync::Input,
    )
    .map_err(|e| anyhow::anyhow!("Failed to create resampler: {}", e))?;

    let mut output = Vec::with_capacity((audio.len() as f64 * ratio) as usize + 1024);

    for chunk in audio.chunks(chunk_size) {
        let input = if chunk.len() < chunk_size {
            let mut padded = chunk.to_vec();
            padded.resize(chunk_size, 0.0);
            padded
        } else {
            chunk.to_vec()
        };

        let frames = input.len();
        let input_adapter = InterleavedSlice::new(&input, 1, frames)
            .map_err(|e| anyhow::anyhow!("Input adapter error: {}", e))?;

        let result = resampler
            .process(&input_adapter, 0, None)
            .map_err(|e| anyhow::anyhow!("Resample error: {}", e))?;

        output.extend(result.take_data());
    }

    // Trim to expected length (remove zero-padding artifacts)
    let expected_len = (audio.len() as f64 * ratio) as usize;
    output.truncate(expected_len);

    Ok(output)
}
