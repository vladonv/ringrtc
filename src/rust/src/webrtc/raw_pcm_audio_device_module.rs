//
// Copyright 2026 signal2sip contributors
// SPDX-License-Identifier: AGPL-3.0-only
//

//! A cubeb-free AudioDeviceModule for signal2sip: instead of driving a real
//! sound device, PCM samples are pushed in (as if from a microphone) and
//! pulled out (as if to a speaker) by native code - e.g. a PJSIP bridge -
//! via `push_recorded_samples()`/`pull_playout_samples()`. This is the
//! Milestone D patch from /home/vlad/.claude/plans/jiggly-mapping-plum.md:
//! prove raw PCM can be injected into/pulled out of RingRTC without any
//! OS audio device in the path, mirroring tg2sip-webrtc's
//! tgcalls-to-PJSIP ring buffer bridge (which was possible there because
//! tgcalls exposes a raw PCM callback API that this cubeb-based
//! AudioDeviceModule does not).
//!
//! This file is purely additive - it does not modify audio_device_module.rs
//! at all. It drives WebRTC's own `Rust_recordedDataIsAvailable`/
//! `Rust_needMorePlayData` FFI hooks (declared `pub` in
//! webrtc::ffi::audio_device_module, called by that file's cubeb-backed
//! `Worker` too) directly, on its own 10ms timer thread instead of a real
//! sound device's hardware-driven callback thread.

use std::collections::VecDeque;
use std::ffi::{c_uchar, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::webrtc;
use crate::webrtc::audio_device_module::AudioLayer;
use crate::webrtc::ffi::audio_device_module::{Rust_needMorePlayData, Rust_recordedDataIsAvailable};

const SAMPLE_FREQUENCY: u32 = 48_000;
const NUM_CHANNELS: u32 = 1;
// WebRTC always expects to provide/consume 10ms of samples at a time,
// matching audio_device_module.rs's WEBRTC_WINDOW.
const WEBRTC_WINDOW: usize = (SAMPLE_FREQUENCY / 100) as usize;
// Cap queued-but-unconsumed audio at 2s so a native side that stops
// pulling/pushing doesn't grow these queues unbounded.
const MAX_QUEUED_SAMPLES: usize = SAMPLE_FREQUENCY as usize * 2;

fn write_bool(mut ptr: webrtc::ptr::Borrowed<bool>, v: bool) -> i32 {
    match unsafe { ptr.as_mut() } {
        Some(p) => {
            *p = v;
            0
        }
        None => -1,
    }
}

fn write_u16(mut ptr: webrtc::ptr::Borrowed<u16>, v: u16) -> i32 {
    match unsafe { ptr.as_mut() } {
        Some(p) => {
            *p = v;
            0
        }
        None => -1,
    }
}

#[derive(Debug)]
pub struct RawPcmAudioDeviceModule {
    // "Microphone" side: samples pushed in here by native code are what
    // gets encoded and sent to the remote peer.
    mic_queue: Arc<Mutex<VecDeque<i16>>>,
    // "Speaker" side: decoded audio received from the remote peer lands
    // here for native code to pull out.
    speaker_queue: Arc<Mutex<VecDeque<i16>>>,
    playing: bool,
    recording: bool,
    playout_initialized: bool,
    recording_initialized: bool,
    playout_thread: Option<JoinHandle<()>>,
    recording_thread: Option<JoinHandle<()>>,
    stop_playout_flag: Arc<AtomicBool>,
    stop_recording_flag: Arc<AtomicBool>,
}

impl RawPcmAudioDeviceModule {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            mic_queue: Arc::new(Mutex::new(VecDeque::new())),
            speaker_queue: Arc::new(Mutex::new(VecDeque::new())),
            playing: false,
            recording: false,
            playout_initialized: false,
            recording_initialized: false,
            playout_thread: None,
            recording_thread: None,
            stop_playout_flag: Arc::new(AtomicBool::new(false)),
            stop_recording_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Feed PCM samples (mono, 48kHz, i16) to be encoded and sent to the
    /// remote peer - the "microphone" input.
    pub fn push_recorded_samples(&self, samples: &[i16]) {
        let mut q = self.mic_queue.lock().unwrap();
        q.extend(samples.iter().copied());
        while q.len() > MAX_QUEUED_SAMPLES {
            q.pop_front();
        }
    }

    /// Pull up to `max_samples` of decoded PCM received from the remote
    /// peer (mono, 48kHz, i16) - the "speaker" output. Returns fewer than
    /// requested (possibly zero) if not enough is buffered yet.
    pub fn pull_playout_samples(&self, max_samples: usize) -> Vec<i16> {
        let mut q = self.speaker_queue.lock().unwrap();
        let n = max_samples.min(q.len());
        q.drain(0..n).collect()
    }

    pub fn active_audio_layer(&self, _audio_layer: webrtc::ptr::Borrowed<AudioLayer>) -> i32 {
        -1
    }

    pub fn init(&mut self) -> i32 {
        0
    }

    pub fn terminate(&mut self) -> i32 {
        self.stop_playout_flag.store(true, Ordering::SeqCst);
        self.stop_recording_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.playout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.recording_thread.take() {
            let _ = t.join();
        }
        0
    }

    pub fn initialized(&self) -> bool {
        true
    }

    pub fn playout_devices(&mut self) -> i16 {
        1
    }

    pub fn recording_devices(&mut self) -> i16 {
        1
    }

    pub fn playout_device_name(
        &mut self,
        _index: u16,
        _name_out: webrtc::ptr::Borrowed<c_uchar>,
        _guid_out: webrtc::ptr::Borrowed<c_uchar>,
    ) -> i32 {
        -1
    }

    pub fn recording_device_name(
        &mut self,
        _index: u16,
        _name_out: webrtc::ptr::Borrowed<c_uchar>,
        _guid_out: webrtc::ptr::Borrowed<c_uchar>,
    ) -> i32 {
        -1
    }

    pub fn playout_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, true)
    }

    pub fn init_playout(&mut self) -> i32 {
        self.playout_initialized = true;
        0
    }

    pub fn playout_is_initialized(&self) -> bool {
        self.playout_initialized
    }

    pub fn recording_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, true)
    }

    pub fn init_recording(&mut self) -> i32 {
        self.recording_initialized = true;
        0
    }

    pub fn recording_is_initialized(&self) -> bool {
        self.recording_initialized
    }

    pub fn start_playout(&mut self) -> i32 {
        if self.playing {
            return 0;
        }
        self.playing = true;
        self.stop_playout_flag.store(false, Ordering::SeqCst);
        let speaker_queue = self.speaker_queue.clone();
        let stop_flag = self.stop_playout_flag.clone();
        self.playout_thread = Some(thread::spawn(move || {
            // Self-correcting fixed-deadline schedule, matching
            // RingRtcSipBridge::FeederLoop's sleep_until pattern
            // (native/voip/ringrtc_sip_bridge.cpp) - a naive
            // thread::sleep(10ms) after each iteration's work would drift
            // whenever the FFI call or mutex lock takes longer than usual
            // (e.g. under real CPU contention from Signal networking/ICE
            // in the same process), and that drift compounds over the
            // life of a call instead of self-correcting.
            let mut next_deadline = Instant::now();
            while !stop_flag.load(Ordering::SeqCst) {
                let mut data = vec![0i16; WEBRTC_WINDOW];
                let mut samples_out: usize = 0;
                let mut elapsed_time_ms: i64 = 0;
                let mut ntp_time_ms: i64 = 0;
                // Safety: `data` has WEBRTC_WINDOW * sizeof(i16) bytes
                // allocated and this call must not write beyond that;
                // matches Worker::need_more_play_data's own usage of this
                // same FFI function in audio_device_module.rs.
                let ret = unsafe {
                    Rust_needMorePlayData(
                        WEBRTC_WINDOW,
                        std::mem::size_of::<i16>(),
                        NUM_CHANNELS.try_into().unwrap(),
                        SAMPLE_FREQUENCY,
                        data.as_mut_ptr() as *mut c_void,
                        &mut samples_out,
                        &mut elapsed_time_ms,
                        &mut ntp_time_ms,
                    )
                };
                if ret == 0 && samples_out > 0 {
                    data.truncate(samples_out);
                    let mut q = speaker_queue.lock().unwrap();
                    q.extend(data);
                    while q.len() > MAX_QUEUED_SAMPLES {
                        q.pop_front();
                    }
                }
                next_deadline += Duration::from_millis(10);
                if let Some(d) = next_deadline.checked_duration_since(Instant::now()) {
                    thread::sleep(d);
                }
            }
        }));
        0
    }

    pub fn stop_playout(&mut self) -> i32 {
        self.stop_playout_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.playout_thread.take() {
            let _ = t.join();
        }
        self.playing = false;
        0
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn start_recording(&mut self) -> i32 {
        if self.recording {
            return 0;
        }
        self.recording = true;
        self.stop_recording_flag.store(false, Ordering::SeqCst);
        let mic_queue = self.mic_queue.clone();
        let stop_flag = self.stop_recording_flag.clone();
        self.recording_thread = Some(thread::spawn(move || {
            // Self-correcting schedule - see the matching comment in
            // start_playout() above.
            let mut next_deadline = Instant::now();
            while !stop_flag.load(Ordering::SeqCst) {
                let samples: Vec<i16> = {
                    let mut q = mic_queue.lock().unwrap();
                    let n = WEBRTC_WINDOW.min(q.len());
                    let mut v: Vec<i16> = q.drain(0..n).collect();
                    // Pad with silence on underrun, matching the ring
                    // buffer convention tg2sip's own voip::RingBuffer uses
                    // (see tg2sip-webrtc/tg2sip/voip/ring_buffer.h).
                    v.resize(WEBRTC_WINDOW, 0);
                    v
                };
                let mut new_mic_level: u32 = 0;
                // Safety: `samples` has WEBRTC_WINDOW * sizeof(i16) bytes
                // allocated and this call must not read beyond that;
                // matches Worker::recorded_data_is_available's own usage.
                let _ret = unsafe {
                    Rust_recordedDataIsAvailable(
                        samples.as_ptr() as *const c_void,
                        samples.len(),
                        std::mem::size_of::<i16>(),
                        NUM_CHANNELS.try_into().unwrap(),
                        SAMPLE_FREQUENCY,
                        0,
                        0,
                        0,
                        false,
                        &mut new_mic_level,
                        -1,
                    )
                };
                next_deadline += Duration::from_millis(10);
                if let Some(d) = next_deadline.checked_duration_since(Instant::now()) {
                    thread::sleep(d);
                }
            }
        }));
        0
    }

    pub fn stop_recording(&mut self) -> i32 {
        self.stop_recording_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.recording_thread.take() {
            let _ = t.join();
        }
        self.recording = false;
        0
    }

    pub fn recording(&self) -> bool {
        self.recording
    }

    pub fn init_speaker(&mut self) -> i32 {
        0
    }
    pub fn speaker_is_initialized(&self) -> bool {
        true
    }
    pub fn init_microphone(&mut self) -> i32 {
        0
    }
    pub fn microphone_is_initialized(&self) -> bool {
        true
    }

    pub fn speaker_volume_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_speaker_volume(&mut self, _volume: u32) -> i32 {
        -1
    }
    pub fn speaker_volume(&self, _volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }
    pub fn max_speaker_volume(&self, _max_volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }
    pub fn min_speaker_volume(&self, _min_volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }

    pub fn microphone_volume_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_microphone_volume(&mut self, _volume: u32) -> i32 {
        -1
    }
    pub fn microphone_volume(&self, _volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }
    pub fn max_microphone_volume(&self, _max_volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }
    pub fn min_microphone_volume(&self, _min_volume_out: webrtc::ptr::Borrowed<u32>) -> i32 {
        -1
    }

    pub fn speaker_mute_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_speaker_mute(&mut self, _enable: bool) -> i32 {
        -1
    }
    pub fn speaker_mute(&self, _enabled_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        -1
    }

    pub fn microphone_mute_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_microphone_mute(&mut self, _enable: bool) -> i32 {
        -1
    }
    pub fn microphone_mute(&self, _enabled_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        -1
    }

    pub fn stereo_playout_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_stereo_playout(&mut self, _enable: bool) -> i32 {
        -1
    }
    pub fn stereo_playout(&self, enabled_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(enabled_out, false)
    }
    pub fn stereo_recording_is_available(&self, available_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(available_out, false)
    }
    pub fn set_stereo_recording(&mut self, _enable: bool) -> i32 {
        -1
    }
    pub fn stereo_recording(&self, enabled_out: webrtc::ptr::Borrowed<bool>) -> i32 {
        write_bool(enabled_out, false)
    }

    pub fn playout_delay(&self, delay_ms_out: webrtc::ptr::Borrowed<u16>) -> i32 {
        write_u16(delay_ms_out, 0)
    }
}

impl Drop for RawPcmAudioDeviceModule {
    fn drop(&mut self) {
        self.stop_playout_flag.store(true, Ordering::SeqCst);
        self.stop_recording_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.playout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.recording_thread.take() {
            let _ = t.join();
        }
    }
}

// --- C callback table, mirroring webrtc::ffi::audio_device_module's
// all_adm_functions!-generated AudioDeviceCallbacks exactly (same method
// list, same order - the C++ side reads this by fixed repr(C) offsets and
// doesn't care which Rust type backs it) but dispatching to
// RawPcmAudioDeviceModule instead of the cubeb-backed AudioDeviceModule.
// Duplicated (not shared via macro) deliberately: audio_device_module.rs
// is left completely untouched by this patch.

macro_rules! raw_pcm_all_adm_functions {
    ($macro:ident) => {
        $macro!(
            active_audio_layer(audio_layer: webrtc::ptr::Borrowed<AudioLayer>) -> i32;
            init() -> i32;
            terminate() -> i32;
            initialized() -> bool;
            playout_devices() -> i16;
            recording_devices() -> i16;
            playout_device_name(index: u16, name: webrtc::ptr::Borrowed<c_uchar>, guid: webrtc::ptr::Borrowed<c_uchar>) -> i32;
            recording_device_name(index: u16, name: webrtc::ptr::Borrowed<c_uchar>, guid: webrtc::ptr::Borrowed<c_uchar>) -> i32;
            playout_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            init_playout() -> i32;
            playout_is_initialized() -> bool;
            recording_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            init_recording() -> i32;
            recording_is_initialized() -> bool;
            start_playout() -> i32;
            stop_playout() -> i32;
            playing() -> bool;
            start_recording() -> i32;
            stop_recording() -> i32;
            recording() -> bool;
            init_speaker() -> i32;
            speaker_is_initialized() -> bool;
            init_microphone() -> i32;
            microphone_is_initialized() -> bool;
            speaker_volume_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_speaker_volume(volume: u32) -> i32;
            speaker_volume(volume: webrtc::ptr::Borrowed<u32>) -> i32;
            max_speaker_volume(max_volume: webrtc::ptr::Borrowed<u32>) -> i32;
            min_speaker_volume(min_volume: webrtc::ptr::Borrowed<u32>) -> i32;
            microphone_volume_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_microphone_volume(volume: u32) -> i32;
            microphone_volume(volume: webrtc::ptr::Borrowed<u32>) -> i32;
            max_microphone_volume(max_volume: webrtc::ptr::Borrowed<u32>) -> i32;
            min_microphone_volume(min_volume: webrtc::ptr::Borrowed<u32>) -> i32;
            speaker_mute_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_speaker_mute(enable: bool) -> i32;
            speaker_mute(enabled: webrtc::ptr::Borrowed<bool>) -> i32;
            microphone_mute_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_microphone_mute(enable: bool) -> i32;
            microphone_mute(enabled: webrtc::ptr::Borrowed<bool>) -> i32;
            stereo_playout_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_stereo_playout(enable: bool) -> i32;
            stereo_playout(enabled: webrtc::ptr::Borrowed<bool>) -> i32;
            stereo_recording_is_available(available: webrtc::ptr::Borrowed<bool>) -> i32;
            set_stereo_recording(enable: bool) -> i32;
            stereo_recording(enabled: webrtc::ptr::Borrowed<bool>) -> i32;
            playout_delay(delay_ms: webrtc::ptr::Borrowed<u16>) -> i32;
        );
    }
}

enum InternalFailure {
    NullPtr,
    MutexPoison,
}
impl From<InternalFailure> for i32 {
    fn from(_failure: InternalFailure) -> i32 {
        -1
    }
}
impl From<InternalFailure> for i16 {
    fn from(_failure: InternalFailure) -> i16 {
        -1
    }
}
impl From<InternalFailure> for bool {
    fn from(_failure: InternalFailure) -> bool {
        false
    }
}

macro_rules! raw_pcm_adm_wrapper {
    () => {};
    ($f:ident($($param:ident: $arg_ty:ty),*) -> $ret:ty ; $($t:tt)*) => {
        extern "C" fn $f(mut ptr: webrtc::ptr::Borrowed<Mutex<RawPcmAudioDeviceModule>>, $($param: $arg_ty),*) -> $ret {
            if let Some(mutex) = unsafe { ptr.as_mut() } {
                match mutex.lock() {
                    #[allow(unused_mut)]
                    Ok(mut adm) => adm.$f($($param),*),
                    Err(e) => {
                        error!("Mutex was poisoned? {}", e);
                        InternalFailure::MutexPoison.into()
                    }
                }
            } else {
                error!("{} wrapper with null pointer", stringify!($f));
                InternalFailure::NullPtr.into()
            }
        }
        raw_pcm_adm_wrapper!($($t)*);
    }
}

raw_pcm_all_adm_functions!(raw_pcm_adm_wrapper);

macro_rules! raw_pcm_adm_struct_definition {
    (struct RawPcmAudioDeviceCallbacks { $($inner:tt)* } => ) => {
        #[repr(C)]
        #[allow(non_snake_case)]
        pub struct RawPcmAudioDeviceCallbacks {
            $($inner)*
        }
    };
    (struct RawPcmAudioDeviceCallbacks { $($inner:tt)* } => $f:ident($($param:ident: $arg_ty:ty),*) -> $ret:ty ; $($t:tt)*) => {
        raw_pcm_adm_struct_definition!(struct RawPcmAudioDeviceCallbacks {
            $($inner)*
            pub $f: extern "C" fn(
              adm_borrowed: webrtc::ptr::Borrowed<Mutex<RawPcmAudioDeviceModule>>, $($param: $arg_ty),*) -> $ret,
        } => $($t)*);
    };
    ($f:ident($($param:ident: $arg_ty:ty),*) -> $ret:ty ; $($t:tt)*) => {
        raw_pcm_adm_struct_definition!(struct RawPcmAudioDeviceCallbacks {
          pub $f: extern "C" fn(
              adm_borrowed: webrtc::ptr::Borrowed<Mutex<RawPcmAudioDeviceModule>>, $($param: $arg_ty),*) -> $ret,
        } => $($t)*);
    }
}

raw_pcm_all_adm_functions!(raw_pcm_adm_struct_definition);

macro_rules! raw_pcm_adm_struct_instantiation {
    (RawPcmAudioDeviceCallbacks { $($inner:tt)* } => ) => {
        const RAW_PCM_AUDIO_DEVICE_CBS: RawPcmAudioDeviceCallbacks = RawPcmAudioDeviceCallbacks {
            $($inner)*
        };
    };
    (RawPcmAudioDeviceCallbacks { $($inner:tt)* } => $f:ident($($_args:tt)*) -> $_ret:ty ; $($t:tt)*) => {
        raw_pcm_adm_struct_instantiation!(
            RawPcmAudioDeviceCallbacks {
                $($inner)*
                $f: crate::webrtc::raw_pcm_audio_device_module::$f,
            } => $($t)*
        );
    };
    ($f:ident($($_args:tt)*) -> $_ret:ty ; $($t:tt)*) => {
        raw_pcm_adm_struct_instantiation!(
            RawPcmAudioDeviceCallbacks {
                $f: crate::webrtc::raw_pcm_audio_device_module::$f,
            } => $($t)*
        );
    }
}

raw_pcm_all_adm_functions!(raw_pcm_adm_struct_instantiation);
pub const RAW_PCM_AUDIO_DEVICE_CBS_PTR: *const RawPcmAudioDeviceCallbacks = &RAW_PCM_AUDIO_DEVICE_CBS;

/// Safety: must be called with the same pointer passed to the C++ layer
/// when constructing the PeerConnectionFactory - mirrors
/// webrtc::ffi::audio_device_module::decrement_adm_ref_count exactly, but
/// for RawPcmAudioDeviceModule's own concrete type (that function hardcodes
/// `Mutex<AudioDeviceModule>`, so it cannot safely be reused here).
pub unsafe extern "C" fn decrement_raw_pcm_adm_ref_count(adm_borrowed: webrtc::ptr::Borrowed<c_void>) {
    if adm_borrowed.is_null() {
        return;
    }
    let adm_borrowed = adm_borrowed.as_ptr() as *const Mutex<RawPcmAudioDeviceModule>;
    let _adm = unsafe { Arc::from_raw(adm_borrowed) };
}
