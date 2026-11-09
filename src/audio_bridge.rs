//! Writes the Guest's microphone audio straight into a CoreAudio output device (BlackHole)
//! instead of relaying it through the Moonlight protocol.
//!
//! The vendored `moonlight-common` crate only forwards mic frames
//! (`MoonlightStream::send_microphone_opus_data`) when the connected host negotiates as a
//! "Foundation Sunshine" server (see `api::stream::webrtc::mod::WebRTCSessionAnswer`'s
//! `microphone` field comment) — upstream Sunshine never does, so that path is permanently
//! dead. This module bypasses it: `api::stream::webrtc::microphone::MicChannel` decodes the
//! Guest's Opus mic frames and pushes the resulting PCM here, which plays it straight into a
//! CoreAudio device the Host configured as their DAW's input (BlackHole), with no dependency
//! on Sunshine at all.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, OnceLock};

    use coreaudio::audio_unit::audio_format::LinearPcmFlags;
    use coreaudio::audio_unit::macos_helpers::{audio_unit_from_device_id, get_device_id_from_name};
    use coreaudio::audio_unit::render_callback::{self, data};
    use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope, StreamFormat};
    use tracing::{info, warn};

    /// Must match a BlackHole install (2ch is the common default; a multichannel BlackHole
    /// variant would need its own device name and channel count here).
    const DEVICE_NAME: &str = "BlackHole 2ch";
    const SAMPLE_RATE: f64 = 48_000.0;
    const CHANNELS: u32 = 2;
    /// Bounds the output jitter buffer so a device nothing is actually draining can't grow
    /// this without bound. ~2 seconds of stereo audio.
    const MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE as usize * CHANNELS as usize * 2;

    pub struct GuestMicBridge {
        queue: Arc<Mutex<VecDeque<f32>>>,
        // Keeping the AudioUnit alive keeps its render thread running; never read after open().
        _audio_unit: AudioUnit,
    }

    // AudioUnit wraps a raw `AudioComponentInstance` pointer, which isn't Send/Sync by
    // default. We never call back into it after `open()` (its render callback runs on
    // CoreAudio's own thread, driven by the device, not by us) — it's only kept here to
    // stay alive for the process's lifetime, so sharing the handle across threads is safe.
    unsafe impl Send for GuestMicBridge {}
    unsafe impl Sync for GuestMicBridge {}

    impl GuestMicBridge {
        fn open() -> anyhow::Result<Self> {
            let device_id = get_device_id_from_name(DEVICE_NAME, false).ok_or_else(|| {
                anyhow::anyhow!(
                    "no CoreAudio output device named \"{DEVICE_NAME}\" (is BlackHole installed?)"
                )
            })?;

            let mut audio_unit = audio_unit_from_device_id(device_id, false)?;

            let stream_format = StreamFormat {
                sample_rate: SAMPLE_RATE,
                sample_format: SampleFormat::F32,
                flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
                channels: CHANNELS,
            };
            audio_unit.set_stream_format(stream_format, Scope::Input, Element::Output)?;

            let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
            let render_queue = queue.clone();

            type Args = render_callback::Args<data::Interleaved<f32>>;
            audio_unit.set_render_callback(move |args: Args| {
                let Args {
                    num_frames, data, ..
                } = args;

                let mut queue = render_queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());

                for frame in 0..num_frames {
                    for channel in 0..CHANNELS as usize {
                        // Underrun (nothing queued, or Guest isn't talking) plays silence
                        // rather than stalling or repeating stale samples.
                        let sample = queue.pop_front().unwrap_or(0.0);
                        data.buffer[frame * CHANNELS as usize + channel] = sample;
                    }
                }

                Ok(())
            })?;

            audio_unit.start()?;
            info!(device = DEVICE_NAME, "guest mic bridge writing into CoreAudio device");

            Ok(Self {
                queue,
                _audio_unit: audio_unit,
            })
        }

        /// Duplicates mono samples to both channels and appends them to the output queue,
        /// dropping the oldest queued audio first if we're backed up past MAX_QUEUED_SAMPLES.
        fn push_mono(&self, samples: &[f32]) {
            let mut queue = self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

            for &sample in samples {
                queue.push_back(sample);
                queue.push_back(sample);
            }

            while queue.len() > MAX_QUEUED_SAMPLES {
                queue.pop_front();
            }
        }
    }

    static BRIDGE: OnceLock<Option<GuestMicBridge>> = OnceLock::new();

    /// Lazily opens the bridge on first use. Returns `None` (logged once) when BlackHole
    /// isn't installed or CoreAudio setup otherwise fails — the stream still works, it just
    /// won't carry the Guest's mic.
    fn global() -> Option<&'static GuestMicBridge> {
        BRIDGE
            .get_or_init(|| match GuestMicBridge::open() {
                Ok(bridge) => Some(bridge),
                Err(err) => {
                    warn!(error = %err, "guest microphone won't be forwarded this session");
                    None
                }
            })
            .as_ref()
    }

    /// Pushes decoded mono PCM samples (48kHz) from a Guest's mic onto the shared output
    /// queue. A no-op (besides the lazy-open attempt) when the bridge couldn't be opened.
    pub fn push_mono_samples(samples: &[f32]) {
        if let Some(bridge) = global() {
            bridge.push_mono(samples);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod other {
    /// No-op on platforms without this bridge implemented yet.
    pub fn push_mono_samples(_samples: &[f32]) {}
}

#[cfg(target_os = "macos")]
pub use macos::push_mono_samples;
#[cfg(not(target_os = "macos"))]
pub use other::push_mono_samples;
