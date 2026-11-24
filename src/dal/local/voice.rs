//! Local microphone capture and offline whisper.cpp transcription.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const RATE: u32 = 16_000;
const SMALL_NAME: &str = "ggml-small-q5_1.bin";
const BASE_NAME: &str = "ggml-base-q5_1.bin";
const SMALL_SHA256: &str = "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb";
const BASE_SHA256: &str = "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898";
const MODEL_REVISION: &str = "87cd18b47b941d2f65d09981dad23bb7d0481c77";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceFailure {
    Microphone,
    Model,
    Transcription,
    NoSpeech,
}

impl std::fmt::Display for VoiceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Microphone => {
                "No microphone is available. Check microphone permission and the default input device."
            }
            Self::Model => "The local speech model could not be downloaded or verified.",
            Self::Transcription => "Local speech transcription failed.",
            Self::NoSpeech => "No speech was detected.",
        })
    }
}

pub struct Recorder {
    stream: cpal::Stream,
    samples: Arc<Mutex<Vec<f32>>>,
    failed: Arc<AtomicBool>,
    source_rate: u32,
    channels: usize,
}

impl Recorder {
    pub fn start() -> Result<Self, VoiceFailure> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(VoiceFailure::Microphone)?;
        let supported = device
            .default_input_config()
            .map_err(|_| VoiceFailure::Microphone)?;
        let source_rate = supported.sample_rate().0;
        let channels = usize::from(supported.channels()).max(1);
        let config: cpal::StreamConfig = supported.clone().into();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let failed = Arc::new(AtomicBool::new(false));
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => {
                build_stream::<f32>(&device, &config, &samples, &failed, |v| v)
            }
            cpal::SampleFormat::F64 => {
                build_stream::<f64>(&device, &config, &samples, &failed, |v| v as f32)
            }
            cpal::SampleFormat::I8 => {
                build_stream::<i8>(&device, &config, &samples, &failed, |v| v as f32 / 128.0)
            }
            cpal::SampleFormat::U8 => {
                build_stream::<u8>(&device, &config, &samples, &failed, |v| {
                    (v as f32 - 128.0) / 128.0
                })
            }
            cpal::SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, &samples, &failed, |v| v as f32 / 32_768.0)
            }
            cpal::SampleFormat::I24 => {
                build_stream::<cpal::I24>(&device, &config, &samples, &failed, |value| {
                    cpal::Sample::to_sample::<f32>(value)
                })
            }
            cpal::SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, &samples, &failed, |v| {
                    (v as f32 - 32_768.0) / 32_768.0
                })
            }
            cpal::SampleFormat::I32 => {
                build_stream::<i32>(&device, &config, &samples, &failed, |v| {
                    v as f32 / 2_147_483_648.0
                })
            }
            cpal::SampleFormat::U32 => {
                build_stream::<u32>(&device, &config, &samples, &failed, |v| {
                    (v as f64 - 2_147_483_648.0) as f32 / 2_147_483_648.0
                })
            }
            cpal::SampleFormat::I64 => {
                build_stream::<i64>(&device, &config, &samples, &failed, |v| {
                    (v as f64 / 9_223_372_036_854_775_808.0) as f32
                })
            }
            cpal::SampleFormat::U64 => {
                build_stream::<u64>(&device, &config, &samples, &failed, |v| {
                    ((v as f64 - 9_223_372_036_854_775_808.0) / 9_223_372_036_854_775_808.0) as f32
                })
            }
            _ => return Err(VoiceFailure::Microphone),
        }
        .map_err(|_| VoiceFailure::Microphone)?;
        stream.play().map_err(|_| VoiceFailure::Microphone)?;
        Ok(Self {
            stream,
            samples,
            failed,
            source_rate,
            channels,
        })
    }

    pub fn stop(self) -> Result<Vec<f32>, VoiceFailure> {
        self.stream.pause().map_err(|_| VoiceFailure::Microphone)?;
        if self.failed.load(Ordering::Relaxed) {
            return Err(VoiceFailure::Microphone);
        }
        let interleaved = self
            .samples
            .lock()
            .map_err(|_| VoiceFailure::Microphone)?
            .clone();
        let mono = downmix(&interleaved, self.channels);
        let samples = resample(&mono, self.source_rate, RATE);
        if samples.len() < (RATE / 5) as usize {
            return Err(VoiceFailure::NoSpeech);
        }
        Ok(samples)
    }
}

fn build_stream<T: cpal::SizedSample + Copy + 'static>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: &Arc<Mutex<Vec<f32>>>,
    failed: &Arc<AtomicBool>,
    convert: fn(T) -> f32,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let samples = Arc::clone(samples);
    let failed_on_error = Arc::clone(failed);
    device.build_input_stream(
        config,
        move |input: &[T], _| {
            if let Ok(mut output) = samples.lock() {
                output.extend(input.iter().map(|sample| {
                    let value = convert(*sample);
                    if value.is_finite() { value } else { 0.0 }
                }));
            }
        },
        move |_| failed_on_error.store(true, Ordering::Relaxed),
        None,
    )
}

fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    interleaved
        .chunks_exact(channels.max(1))
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

fn resample(input: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if input.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return input.to_vec();
    }
    let output_len =
        ((input.len() as u64 * u64::from(target_rate)) / u64::from(source_rate)) as usize;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * f64::from(source_rate) / f64::from(target_rate);
            let left = position.floor() as usize;
            let right = (left + 1).min(input.len() - 1);
            let fraction = (position - left as f64) as f32;
            input[left] + (input[right] - input[left]) * fraction
        })
        .collect()
}

fn cache_dir() -> Result<PathBuf, VoiceFailure> {
    #[cfg(target_os = "macos")]
    let path = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Caches/Cortex/voice"));
    #[cfg(target_os = "linux")]
    let path = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|home| home.join("cortex/voice"));
    path.ok_or(VoiceFailure::Model)
}

fn model_file(name: &str) -> Result<PathBuf, VoiceFailure> {
    Ok(cache_dir()?.join(name))
}

fn verify_model(path: &Path, expected: &str) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    format!("{:x}", digest.finalize()) == expected
}

fn ensure_model(use_small: bool) -> Result<PathBuf, VoiceFailure> {
    let (name, digest) = if use_small {
        (SMALL_NAME, SMALL_SHA256)
    } else {
        (BASE_NAME, BASE_SHA256)
    };
    let path = model_file(name)?;
    if verify_model(&path, digest) {
        return Ok(path);
    }
    let parent = path.parent().ok_or(VoiceFailure::Model)?;
    fs::create_dir_all(parent).map_err(|_| VoiceFailure::Model)?;
    let _ = fs::remove_file(&path);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| VoiceFailure::Model)?
        .as_nanos();
    let temporary = parent.join(format!("{name}.{}.{}.part", std::process::id(), nonce));
    let url =
        format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODEL_REVISION}/{name}");
    let result = (|| {
        let mut response = reqwest::blocking::Client::new()
            .get(url)
            .send()
            .map_err(|_| VoiceFailure::Model)?
            .error_for_status()
            .map_err(|_| VoiceFailure::Model)?;
        let mut file = File::create(&temporary).map_err(|_| VoiceFailure::Model)?;
        io::copy(&mut response, &mut file).map_err(|_| VoiceFailure::Model)?;
        file.flush().map_err(|_| VoiceFailure::Model)?;
        if !verify_model(&temporary, digest) {
            return Err(VoiceFailure::Model);
        }
        fs::rename(&temporary, &path).map_err(|_| VoiceFailure::Model)?;
        Ok(path.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn transcribe(samples: &[f32]) -> Result<String, VoiceFailure> {
    let model = ensure_model(true)?;
    transcribe_with_model(samples, &model)
}

fn transcribe_with_model(samples: &[f32], model: &Path) -> Result<String, VoiceFailure> {
    let path = model.to_str().ok_or(VoiceFailure::Model)?;
    let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .map_err(|_| VoiceFailure::Transcription)?;
    let mut state = context
        .create_state()
        .map_err(|_| VoiceFailure::Transcription)?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4) as i32,
    );
    params.set_language(Some("auto"));
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    state
        .full(params, samples)
        .map_err(|_| VoiceFailure::Transcription)?;
    let mut text = String::new();
    for segment in state.as_iter() {
        if let Ok(part) = segment.to_str_lossy() {
            text.push_str(part.trim());
            text.push(' ');
        }
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        Err(VoiceFailure::NoSpeech)
    } else {
        Ok(text)
    }
}

pub fn remove_models(output: &mut impl Write) -> io::Result<()> {
    let dir = cache_dir().map_err(|_| io::Error::other("voice model cache is unavailable"))?;
    remove_model_files(&dir)?;
    writeln!(output, "Voice models removed.")
}

fn remove_model_files(dir: &Path) -> io::Result<()> {
    for name in [SMALL_NAME, BASE_NAME] {
        match fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(io::Error::other("could not remove the voice model")),
        }
    }
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry =
                    entry.map_err(|_| io::Error::other("could not inspect voice model cache"))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if (name.starts_with(SMALL_NAME) || name.starts_with(BASE_NAME))
                    && name.ends_with(".part")
                {
                    fs::remove_file(entry.path())
                        .map_err(|_| io::Error::other("could not remove the voice model"))?;
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(io::Error::other("could not inspect voice model cache")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_stereo_audio_to_mono() {
        assert_eq!(downmix(&[1.0, -1.0, 0.5, 0.5], 2), vec![0.0, 0.5]);
    }

    #[test]
    fn resamples_mono_to_whisper_rate() {
        let output = resample(&[0.0, 1.0, 0.0, -1.0], 4, 8);
        assert_eq!(output.len(), 8);
        assert!((output[1] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn model_sha256_constants_are_full_length() {
        assert_eq!(SMALL_SHA256.len(), 64);
        assert_eq!(BASE_SHA256.len(), 64);
    }

    #[test]
    fn streamed_hash_check_accepts_only_the_expected_sha256() {
        let path =
            std::env::temp_dir().join(format!("cortex-cli-model-test-{}", std::process::id()));
        fs::write(&path, b"voice model fixture").unwrap();
        assert!(verify_model(
            &path,
            "183a8f7be21382fc685b55b7a138c9caa8f404d94526a3ee1b92f4459ed60696"
        ));
        assert!(!verify_model(&path, SMALL_SHA256));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn remove_command_deletes_both_models_from_the_requested_cache() {
        let path =
            std::env::temp_dir().join(format!("cortex-cli-voice-test-{}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        for name in [SMALL_NAME, BASE_NAME] {
            fs::write(path.join(name), b"test model").unwrap();
        }
        fs::write(path.join(format!("{SMALL_NAME}.test.part")), b"partial").unwrap();
        remove_model_files(&path).unwrap();
        assert!(!path.join(SMALL_NAME).exists());
        assert!(!path.join(BASE_NAME).exists());
        assert!(!path.join(format!("{SMALL_NAME}.test.part")).exists());
        fs::remove_dir(path).unwrap();
    }
}
