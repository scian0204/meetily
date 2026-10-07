//! Browser audio source (meetily-server only).
//!
//! Browser PCM arrives over WebSocket and replaces cpal capture:
//! WS handler -> push_frame() -> one bounded queue -> one consumer thread
//! -> each source's sink (AudioCapture::process_audio_data, registered by stream.rs).
//! Everything after the sink is unchanged.
//!
//! A frame carries the microphone and tab-audio samples of the same 20 ms, and a single
//! thread feeds both sinks back to back. Separate per-source threads would let one source
//! run ahead during bursts, and the mixer pads the lagging source with silence.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};

use super::devices::{AudioDevice, DeviceType as DeviceKind};
use super::recording_state::DeviceType;

/// Browser sends mono at 48 kHz. Any other rate would be resampled by AudioCapture.
pub const WEB_SAMPLE_RATE: u32 = 48_000;
/// Frames buffered between the socket and the consumer (~30 s of 20 ms frames).
const QUEUE_FRAMES: usize = 1500;

/// One browser frame: samples per source, all the same duration.
pub type Frame = Vec<(DeviceType, Vec<f32>)>;
type Sink = Box<dyn Fn(&[f32]) + Send>;

static ENABLED: AtomicBool = AtomicBool::new(false);
static QUEUE: OnceLock<SyncSender<Frame>> = OnceLock::new();
/// [microphone, system]
static SINKS: Mutex<[Option<Sink>; 2]> = Mutex::new([None, None]);
static DROPPED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Switch audio capture to browser frames. Called once by meetily-server main.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
    QUEUE.get_or_init(|| {
        let (tx, rx) = sync_channel::<Frame>(QUEUE_FRAMES);
        std::thread::Builder::new()
            .name("web-audio".into())
            .spawn(move || {
                for frame in rx {
                    deliver(frame);
                }
            })
            .expect("failed to spawn the web audio thread");
        tx
    });
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

fn index(source: &DeviceType) -> usize {
    match source {
        DeviceType::Microphone => 0,
        DeviceType::System => 1,
    }
}

fn deliver(frame: Frame) {
    let sinks = SINKS.lock().unwrap();
    for (source, samples) in frame {
        // No sink: no recording consumes this source, the frame is dropped.
        if let Some(sink) = &sinks[index(&source)] {
            sink(&samples);
        }
    }
}

/// Queue one decoded frame. Never blocks: dropped (with a warning) when the queue is full.
pub fn push_frame(frame: Frame) {
    let Some(queue) = QUEUE.get() else { return };
    if let Err(TrySendError::Full(_)) = queue.try_send(frame) {
        let dropped = DROPPED_FRAMES.fetch_add(1, Ordering::Relaxed);
        if dropped % 250 == 0 {
            log::warn!("web audio: queue full, dropped {} frame(s) so far", dropped + 1);
        }
    }
}

/// Route a source's samples to `sink` (the web stream's AudioCapture); replaces any previous sink.
pub fn attach(source: &DeviceType, sink: Sink) {
    SINKS.lock().unwrap()[index(source)] = Some(sink);
}

/// Stop routing a source; its frames are dropped from now on.
pub fn detach(source: &DeviceType) {
    SINKS.lock().unwrap()[index(source)] = None;
}

/// Web-mode device resolution: trust the browser's label, never enumerate cpal.
pub fn device(requested: Option<&str>, kind: DeviceKind) -> AudioDevice {
    let fallback = match kind {
        DeviceKind::Input => "Browser Microphone",
        DeviceKind::Output => "Browser Tab Audio",
    };
    requested
        .and_then(|name| AudioDevice::from_name(name).ok())
        .unwrap_or_else(|| AudioDevice::new(fallback.to_string(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn frames_reach_attached_sinks_in_order() {
        enable();
        let (tx, rx) = channel();
        let mic_tx = tx.clone();
        attach(&DeviceType::Microphone, Box::new(move |s| mic_tx.send(("mic", s.to_vec())).unwrap()));
        attach(&DeviceType::System, Box::new(move |s| tx.send(("sys", s.to_vec())).unwrap()));

        push_frame(vec![(DeviceType::Microphone, vec![0.1]), (DeviceType::System, vec![0.2])]);
        push_frame(vec![(DeviceType::Microphone, vec![0.3]), (DeviceType::System, vec![0.4])]);
        let got: Vec<_> = (0..4).map(|_| rx.recv_timeout(Duration::from_secs(2)).unwrap()).collect();
        assert_eq!(got, vec![("mic", vec![0.1]), ("sys", vec![0.2]), ("mic", vec![0.3]), ("sys", vec![0.4])]);

        detach(&DeviceType::System);
        push_frame(vec![(DeviceType::Microphone, vec![0.5]), (DeviceType::System, vec![0.6])]);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), ("mic", vec![0.5]));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err()); // system frame dropped
        detach(&DeviceType::Microphone);
    }

    #[test]
    fn device_falls_back_to_browser_names() {
        assert_eq!(device(None, DeviceKind::Input).name, "Browser Microphone");
        assert_eq!(device(Some("no suffix"), DeviceKind::Output).name, "Browser Tab Audio");
        let d = device(Some("USB Mic (input)"), DeviceKind::Input);
        assert_eq!((d.name.as_str(), d.device_type), ("USB Mic", DeviceKind::Input));
    }
}
