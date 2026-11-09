use std::{future::pending, sync::Arc};

use audiopus::{Channels, SampleRate, coder::Decoder as OpusDecoder};
use moonlight_common::webrtc::sdp::Session;
use tokio::{
    spawn,
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
};
use tracing::{Instrument, debug, debug_span, info, warn};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};

use crate::app::AppError;

/// Browsers send a plain `getUserMedia({audio: true})` mic track as mono Opus — matches
/// `web/stream/audio/microphone_pipe.ts`, which requests no channel/rate constraints.
const MIC_SAMPLE_RATE: SampleRate = SampleRate::Hz48000;
const MIC_CHANNELS: Channels = Channels::Mono;
/// Max Opus frame size at 48kHz is 120ms (5760 samples); size the decode buffer generously.
const MAX_DECODED_SAMPLES: usize = 5760;

pub enum MicChannelEvent {
    OnFrame { samples: Vec<f32> },
    Closed,
}

pub struct MicChannel {
    receiver: Option<UnboundedReceiver<MicChannelEvent>>,
}

impl MicChannel {
    pub async fn new(offer_sdp: &Session) -> Option<Self> {
        let mut has_mic = false;

        'outside: for media in &offer_sdp.medias {
            for attribute in &media.attributes {
                let attribute = &attribute.attribute;
                if attribute == "sendonly" || attribute == "sendrecv" {
                    has_mic = true;
                    break 'outside;
                }
            }
        }

        if !has_mic {
            return None;
        }

        Some(Self { receiver: None })
    }

    pub fn set_track(&mut self, track: Arc<dyn TrackRemote>) {
        let (sender, receiver) = unbounded_channel();

        spawn(
            async move {
                let mut decoder = match OpusDecoder::new(MIC_SAMPLE_RATE, MIC_CHANNELS) {
                    Ok(decoder) => decoder,
                    Err(err) => {
                        warn!(error = %err, "failed to create opus decoder for guest mic, dropping mic track");
                        return;
                    }
                };
                let mut decode_buffer = [0f32; MAX_DECODED_SAMPLES];
                let mut logged_first_packet = false;

                while let Some(event) = track.poll().await {
                    match event {
                        TrackRemoteEvent::OnOpen(info) => {
                            info!(info = ?info, "guest mic track opened");
                        }
                        TrackRemoteEvent::OnRtpPacket(packet) => {
                            if !logged_first_packet {
                                logged_first_packet = true;
                                info!("received first guest mic rtp packet");
                            }

                            let decoded_samples = match decoder.decode_float(
                                Some(packet.payload.as_ref()),
                                decode_buffer.as_mut_slice(),
                                false,
                            ) {
                                Ok(count) => count,
                                Err(err) => {
                                    warn!(error = %err, "failed to decode guest mic opus frame, skipping it");
                                    continue;
                                }
                            };

                            if sender
                                .send(MicChannelEvent::OnFrame {
                                    samples: decode_buffer[..decoded_samples].to_vec(),
                                })
                                .is_err()
                            {
                                debug!(
                                    "mic receiver was closed, closing remote track polling task"
                                );
                                break;
                            }
                        }
                        TrackRemoteEvent::OnEnded => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
            .instrument(debug_span!("microphone track remote")),
        );

        self.receiver = Some(receiver);
    }

    pub async fn drive(&mut self) -> Result<MicChannelEvent, AppError> {
        if let Some(receiver) = self.receiver.as_mut() {
            let maybe_event = receiver.recv().await;

            match maybe_event {
                Some(event) => Ok(event),
                None => {
                    self.receiver = None;
                    Ok(MicChannelEvent::Closed)
                }
            }
        } else {
            pending().await
        }
    }
}
