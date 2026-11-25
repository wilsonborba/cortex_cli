//! Local microphone capture and offline whisper.cpp transcription.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
use zeroize::Zeroize;

const RATE: u32 = 16_000;
const SMALL_NAME: &str = "ggml-small-q5_1.bin";
const BASE_NAME: &str = "ggml-base-q5_1.bin";
const SMALL_SHA256: &str = "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb";
const BASE_SHA256: &str = "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898";
const MODEL_REVISION: &str = "87cd18b47b941d2f65d09981dad23bb7d0481c77";
const MAX_MODEL_BYTES: u64 = 512 * 1024 * 1024;
const MODEL_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MODEL_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn clear_audio_samples(samples: &mut [f32]) {
    samples.zeroize();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceFailure {
    Microphone,
    Model,
    Transcription,
    NoSpeech,
    Cancelled,
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
            Self::Cancelled => "Local speech transcription cancelled.",
        })
    }
}

trait CaptureProducer {
    fn stop(&mut self) -> Result<(), VoiceFailure>;
}

struct CpalProducer(Option<cpal::Stream>);

impl CaptureProducer for CpalProducer {
    fn stop(&mut self) -> Result<(), VoiceFailure> {
        let Some(stream) = self.0.take() else {
            return Ok(());
        };
        let paused = stream.pause();
        drop(stream);
        paused.map_err(|_| VoiceFailure::Microphone)
    }
}

pub struct Recorder {
    producer: Box<dyn CaptureProducer>,
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
        if stream.play().is_err() {
            drop(stream);
            if let Ok(mut samples) = samples.lock() {
                clear_audio_samples(&mut samples);
            }
            return Err(VoiceFailure::Microphone);
        }
        Ok(Self {
            producer: Box::new(CpalProducer(Some(stream))),
            samples,
            failed,
            source_rate,
            channels,
        })
    }

    pub fn stop(mut self) -> Result<Vec<f32>, VoiceFailure> {
        self.producer.stop()?;
        if self.failed.load(Ordering::Relaxed) {
            return Err(VoiceFailure::Microphone);
        }
        let interleaved = self
            .samples
            .lock()
            .map_err(|_| VoiceFailure::Microphone)?
            .clone();
        let mut interleaved = interleaved;
        let mut mono = downmix(&interleaved, self.channels);
        clear_audio_samples(&mut interleaved);
        let mut samples = resample(&mono, self.source_rate, RATE);
        clear_audio_samples(&mut mono);
        if samples.len() < (RATE / 5) as usize {
            clear_audio_samples(&mut samples);
            return Err(VoiceFailure::NoSpeech);
        }
        Ok(samples)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Stop and destroy the callback producer before clearing its shared buffer.
        let _ = self.producer.stop();
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_audio_samples(&mut samples);
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

fn cancellation_requested(cancel: &AtomicBool) -> Result<(), VoiceFailure> {
    if cancel.load(Ordering::Relaxed) {
        Err(VoiceFailure::Cancelled)
    } else {
        Ok(())
    }
}

struct OwnedPartFile {
    path: PathBuf,
    committed: bool,
}

impl Drop for OwnedPartFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn copy_model_bytes<R: Read, W: Write>(
    response: &mut R,
    file: &mut W,
    cancel: &AtomicBool,
    max_bytes: u64,
    timeout: Duration,
) -> Result<String, VoiceFailure> {
    let started = Instant::now();
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        cancellation_requested(cancel)?;
        if started.elapsed() >= timeout {
            return Err(VoiceFailure::Model);
        }
        let read = response.read(&mut buffer).map_err(|_| {
            if cancel.load(Ordering::Relaxed) {
                VoiceFailure::Cancelled
            } else {
                VoiceFailure::Model
            }
        })?;
        if started.elapsed() >= timeout {
            return Err(VoiceFailure::Model);
        }
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|length| *length <= max_bytes)
            .ok_or(VoiceFailure::Model)?;
        file.write_all(&buffer[..read])
            .map_err(|_| VoiceFailure::Model)?;
        digest.update(&buffer[..read]);
    }
    cancellation_requested(cancel)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_model_with_cancel(
    path: &Path,
    expected: &str,
    cancel: &AtomicBool,
) -> Result<bool, VoiceFailure> {
    let Ok(mut file) = File::open(path) else {
        return Ok(false);
    };
    if file
        .metadata()
        .map(|metadata| metadata.len() > MAX_MODEL_BYTES)
        .unwrap_or(true)
    {
        return Ok(false);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        cancellation_requested(cancel)?;
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => digest.update(&buffer[..read]),
            Err(_) => return Ok(false),
        }
    }
    Ok(format!("{:x}", digest.finalize()) == expected)
}

#[cfg(test)]
fn verify_model(path: &Path, expected: &str) -> bool {
    verify_model_with_cancel(path, expected, &AtomicBool::new(false)).unwrap_or(false)
}

fn download_model_file(
    url: &str,
    path: &Path,
    expected_digest: &str,
    cancel: &AtomicBool,
) -> Result<(), VoiceFailure> {
    cancellation_requested(cancel)?;
    let parent = path.parent().ok_or(VoiceFailure::Model)?;
    fs::create_dir_all(parent).map_err(|_| VoiceFailure::Model)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| VoiceFailure::Model)?
        .as_nanos();
    let temporary = parent.join(format!(
        "{}.{}.{}.part",
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(VoiceFailure::Model)?,
        std::process::id(),
        nonce
    ));
    (|| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(MODEL_CONNECT_TIMEOUT)
            .timeout(MODEL_IO_TIMEOUT)
            .build()
            .map_err(|_| VoiceFailure::Model)?;
        let mut response = client
            .get(url)
            .send()
            .map_err(|_| {
                if cancel.load(Ordering::Relaxed) {
                    VoiceFailure::Cancelled
                } else {
                    VoiceFailure::Model
                }
            })?
            .error_for_status()
            .map_err(|_| {
                if cancel.load(Ordering::Relaxed) {
                    VoiceFailure::Cancelled
                } else {
                    VoiceFailure::Model
                }
            })?;
        cancellation_requested(cancel)?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_MODEL_BYTES)
        {
            return Err(VoiceFailure::Model);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| VoiceFailure::Model)?;
        let mut owned_part = OwnedPartFile {
            path: temporary.clone(),
            committed: false,
        };
        let digest = copy_model_bytes(
            &mut response,
            &mut file,
            cancel,
            MAX_MODEL_BYTES,
            MODEL_DOWNLOAD_TIMEOUT,
        )?;
        if digest != expected_digest {
            return Err(VoiceFailure::Model);
        }
        file.flush().map_err(|_| VoiceFailure::Model)?;
        drop(file);
        fs::rename(&temporary, path).map_err(|_| VoiceFailure::Model)?;
        owned_part.committed = true;
        Ok(())
    })()
}

fn ensure_model(use_small: bool, cancel: &AtomicBool) -> Result<PathBuf, VoiceFailure> {
    cancellation_requested(cancel)?;
    let (name, digest) = if use_small {
        (SMALL_NAME, SMALL_SHA256)
    } else {
        (BASE_NAME, BASE_SHA256)
    };
    let path = model_file(name)?;
    if verify_model_with_cancel(&path, digest, cancel)? {
        cancellation_requested(cancel)?;
        return Ok(path);
    }
    let url =
        format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODEL_REVISION}/{name}");
    download_model_file(&url, &path, digest, cancel)?;
    Ok(path)
}

pub fn transcribe(samples: &[f32], cancel: &Arc<AtomicBool>) -> Result<String, VoiceFailure> {
    let model = ensure_model(true, cancel)?;
    cancellation_requested(cancel)?;
    transcribe_with_model(samples, &model, cancel)
}

fn transcribe_with_model(
    samples: &[f32],
    model: &Path,
    cancel: &Arc<AtomicBool>,
) -> Result<String, VoiceFailure> {
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
    let cancel_for_abort = Arc::clone(cancel);
    params.set_abort_callback_safe(move || cancel_for_abort.load(Ordering::Relaxed));
    if state.full(params, samples).is_err() {
        return if cancel.load(Ordering::Relaxed) {
            Err(VoiceFailure::Cancelled)
        } else {
            Err(VoiceFailure::Transcription)
        };
    }
    cancellation_requested(cancel)?;
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
    use std::{
        net::{TcpListener, TcpStream},
        sync::mpsc as std_mpsc,
        thread::JoinHandle,
    };

    struct ControlledProducer {
        stop_requested: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        worker: Arc<Mutex<Option<JoinHandle<()>>>>,
        fail_stop: bool,
    }

    type ControlledRecorderFixture = (
        Recorder,
        Arc<Mutex<Vec<f32>>>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        Arc<Mutex<Option<JoinHandle<()>>>>,
    );

    impl CaptureProducer for ControlledProducer {
        fn stop(&mut self) -> Result<(), VoiceFailure> {
            self.stop_requested.store(true, Ordering::SeqCst);
            if let Some(worker) = self.worker.lock().unwrap().take() {
                worker.join().unwrap();
            }
            self.stopped.store(true, Ordering::SeqCst);
            if self.fail_stop {
                Err(VoiceFailure::Microphone)
            } else {
                Ok(())
            }
        }
    }

    fn controlled_recorder(fail_stop: bool) -> ControlledRecorderFixture {
        let samples = Arc::new(Mutex::new(vec![0.5]));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let callback_stopped = Arc::new(AtomicBool::new(false));
        let worker = Arc::new(Mutex::new(None));
        let callback_samples = Arc::clone(&samples);
        let callback_stop = Arc::clone(&stop_requested);
        let callback_done = Arc::clone(&callback_stopped);
        let callback = std::thread::spawn(move || {
            while !callback_stop.load(Ordering::SeqCst) {
                callback_samples.lock().unwrap().push(0.25);
                std::thread::sleep(Duration::from_millis(1));
            }
            callback_done.store(true, Ordering::SeqCst);
        });
        *worker.lock().unwrap() = Some(callback);
        let recorder = Recorder {
            producer: Box::new(ControlledProducer {
                stop_requested: Arc::clone(&stop_requested),
                stopped: Arc::clone(&callback_stopped),
                worker: Arc::clone(&worker),
                fail_stop,
            }),
            samples: Arc::clone(&samples),
            failed: Arc::new(AtomicBool::new(false)),
            source_rate: RATE,
            channels: 1,
        };
        (recorder, samples, stop_requested, callback_stopped, worker)
    }

    fn stop_test_callback(stop_requested: &AtomicBool, worker: &Mutex<Option<JoinHandle<()>>>) {
        stop_requested.store(true, Ordering::SeqCst);
        if let Some(worker) = worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
    }

    fn local_response_server(body: Vec<u8>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serve_request(&mut stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });
        (format!("http://{address}/model"), worker)
    }

    fn slow_local_response_server(
        body: Vec<u8>,
    ) -> (
        String,
        std_mpsc::Receiver<()>,
        std_mpsc::Sender<()>,
        JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (sent_tx, sent_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serve_request(&mut stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            let split = 64 * 1024;
            stream.write_all(&body[..split]).unwrap();
            stream.flush().unwrap();
            let _ = sent_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            let _ = stream.write_all(&body[split..]);
            let _ = stream.flush();
        });
        (
            format!("http://{address}/model"),
            sent_rx,
            release_tx,
            worker,
        )
    }

    fn serve_request(stream: &mut TcpStream) {
        let mut request = [0u8; 2048];
        let _ = stream.read(&mut request).unwrap();
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "cortex-cli-voice-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn recorder_drop_stops_the_callback_before_wiping_shared_samples() {
        let (recorder, samples, stop_requested, callback_stopped, worker) =
            controlled_recorder(false);
        drop(recorder);
        let stopped_before_cleanup = callback_stopped.load(Ordering::SeqCst);
        let wiped_before_cleanup = samples.lock().unwrap().iter().all(|sample| *sample == 0.0);
        stop_test_callback(&stop_requested, &worker);
        assert!(
            stopped_before_cleanup,
            "producer must be joined before wiping"
        );
        assert!(
            wiped_before_cleanup,
            "capture buffer must be zeroized on drop"
        );
    }

    #[test]
    fn recorder_stop_error_still_joins_callback_and_wipes_buffer() {
        let (recorder, samples, _, callback_stopped, _) = controlled_recorder(true);
        assert_eq!(recorder.stop().unwrap_err(), VoiceFailure::Microphone);
        assert!(callback_stopped.load(Ordering::SeqCst));
        assert!(samples.lock().unwrap().iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn local_http_fixture_downloads_verified_model_and_commits_only_final_file() {
        let body = b"synthetic whisper model".to_vec();
        let digest = format!("{:x}", Sha256::digest(&body));
        let (url, server) = local_response_server(body.clone());
        let dir = unique_test_dir("download-pass");
        let path = dir.join("model.bin");
        download_model_file(&url, &path, &digest, &AtomicBool::new(false)).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_http_fixture_rejects_wrong_digest_and_cleans_its_exact_partial() {
        let (url, server) = local_response_server(b"synthetic model".to_vec());
        let dir = unique_test_dir("download-bad-digest");
        let path = dir.join("model.bin");
        assert_eq!(
            download_model_file(&url, &path, &"0".repeat(64), &AtomicBool::new(false)),
            Err(VoiceFailure::Model)
        );
        server.join().unwrap();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn local_http_fixture_cancellation_removes_only_its_partial_model() {
        let body = vec![0x5a; 128 * 1024];
        let digest = format!("{:x}", Sha256::digest(&body));
        let (url, sent, release, server) = slow_local_response_server(body);
        let dir = unique_test_dir("download-cancel");
        let path = dir.join("model.bin");
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_path = path.clone();
        let download = std::thread::spawn(move || {
            download_model_file(&url, &worker_path, &digest, &worker_cancel)
        });
        sent.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut partial_created = false;
        while Instant::now() < deadline {
            partial_created = fs::read_dir(&dir).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")
            });
            if partial_created {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cancel.store(true, Ordering::Relaxed);
        let _ = release.send(());
        assert_eq!(download.join().unwrap(), Err(VoiceFailure::Cancelled));
        server.join().unwrap();
        assert!(
            partial_created,
            "the HTTP fixture must exercise partial cleanup"
        );
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn model_copy_enforces_byte_and_elapsed_time_limits() {
        let mut output = Vec::new();
        assert_eq!(
            copy_model_bytes(
                &mut io::Cursor::new(b"12345"),
                &mut output,
                &AtomicBool::new(false),
                4,
                Duration::from_secs(1),
            ),
            Err(VoiceFailure::Model)
        );
        assert!(output.is_empty());

        struct SlowReader;
        impl Read for SlowReader {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(10));
                target[0] = 1;
                Ok(1)
            }
        }
        assert_eq!(
            copy_model_bytes(
                &mut SlowReader,
                &mut Vec::new(),
                &AtomicBool::new(false),
                4,
                Duration::from_millis(1),
            ),
            Err(VoiceFailure::Model)
        );
    }

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
