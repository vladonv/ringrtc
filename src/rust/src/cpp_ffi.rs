//
// Copyright 2026 signal2sip contributors
// SPDX-License-Identifier: AGPL-3.0-only
//

//! Milestone E (see /home/vlad/.claude/plans/jiggly-mapping-plum.md and
//! signal2sip's project memory project_signal2sip_milestone_e_research):
//! a thin `extern "C"` wrapper around `CallManager<NativePlatform>`.
//!
//! Key finding from this milestone's research: `NativePlatform`
//! (native.rs) is not Node-specific - it already takes a
//! `PeerConnectionFactory` (so Milestone D's
//! `new_with_raw_pcm_adm()` slots in with zero changes to native.rs) and
//! three small trait objects (`SignalingSender`, `CallStateHandler`,
//! `GroupUpdateHandler`) supplied by the embedder. This file's job is
//! just to implement those three traits by calling out to C function
//! pointers, and expose call-lifecycle operations as `extern "C"`
//! functions - not to write a new `Platform` impl.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex};

use crate::common::{CallId, CallMediaType, DeviceId, Result};
use crate::core::call_manager::CallManager;
use crate::core::group_call;
use crate::core::signaling;
use crate::lite::http;
use crate::lite::sfu::{DemuxId, UserId};
use crate::native::{
    CallState, CallStateHandler, GroupUpdate, GroupUpdateHandler, NativeCallContext,
    NativePlatform, SignalingSender,
};
use crate::webrtc::peer_connection::AudioLevel;
use crate::webrtc::peer_connection_factory::{AudioConfig, PeerConnectionFactory};
use crate::webrtc::peer_connection_observer::NetworkRoute;
use crate::webrtc::media::{VideoFrame, VideoSink};
use crate::webrtc::raw_pcm_audio_device_module::RawPcmAudioDeviceModule;

/// C-supplied callbacks, mirroring exactly what SignalingSender/
/// CallStateHandler need to report. One `Callbacks` per CallManager
/// instance (`context` is an opaque pointer the C caller gets back on
/// every invocation - e.g. a `this` pointer for whatever gateway object
/// owns this call manager).
#[repr(C)]
pub struct Callbacks {
    pub context: *mut c_void,
    /// (context, remote_peer_id, call_id, call_media_type, opaque, opaque_len)
    pub send_offer:
        extern "C" fn(*mut c_void, *const c_char, u64, i32, *const u8, usize),
    /// (context, remote_peer_id, call_id, opaque, opaque_len)
    pub send_answer: extern "C" fn(*mut c_void, *const c_char, u64, *const u8, usize),
    /// (context, remote_peer_id, call_id, has_receiver_device_id, receiver_device_id, opaque, opaque_len)
    pub send_ice:
        extern "C" fn(*mut c_void, *const c_char, u64, bool, u32, *const u8, usize),
    /// (context, remote_peer_id, call_id, hangup_type, hangup_device_id)
    pub send_hangup: extern "C" fn(*mut c_void, *const c_char, u64, i32, u32),
    /// (context, remote_peer_id, call_id, call_state) - call_state values
    /// match this file's CALL_STATE_* constants below.
    pub call_state: extern "C" fn(*mut c_void, *const c_char, u64, i32),
}

// The context pointer's thread-safety is the C caller's responsibility
// (matches how every other Borrowed<c_void> context pointer in this
// codebase is handled, e.g. RffiAudioConfig's adm_borrowed).
unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

/// Minimal stderr logger - env_logger is only a dev-dependency of this
/// crate (not available to library consumers like this C ABI), so this
/// avoids adding a new real dependency just for diagnostics.
struct StderrLogger;
impl log::Log for StderrLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!("[{}] {}: {}", record.level(), record.target(), record.args());
    }
    fn flush(&self) {}
}
static STDERR_LOGGER: StderrLogger = StderrLogger;

/// Installs a stderr `log` crate backend and routes WebRTC's own internal
/// C++ logging through it too - without this, every error!/info!/warn!
/// call in this crate (including this file's own diagnostics) is a silent
/// no-op, since the `log` facade needs a real backend installed. Safe to
/// call multiple times. Call once at startup, before creating any call
/// manager.
#[unsafe(no_mangle)]
pub extern "C" fn signal2sip_init_logging() {
    let _ = log::set_logger(&STDERR_LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
    crate::webrtc::logging::set_logger(log::LevelFilter::Debug);
}

pub const CALL_STATE_INCOMING_AUDIO: i32 = 0;
pub const CALL_STATE_OUTGOING_AUDIO: i32 = 1;
pub const CALL_STATE_RINGING: i32 = 2;
pub const CALL_STATE_CONNECTED: i32 = 3;
pub const CALL_STATE_CONNECTING: i32 = 4;
pub const CALL_STATE_ENDED: i32 = 5;
pub const CALL_STATE_REJECTED: i32 = 6;
pub const CALL_STATE_CONCLUDED: i32 = 7;

fn call_state_to_i32(state: &CallState) -> i32 {
    match state {
        CallState::Incoming(_) => CALL_STATE_INCOMING_AUDIO,
        CallState::Outgoing(_) => CALL_STATE_OUTGOING_AUDIO,
        CallState::Ringing => CALL_STATE_RINGING,
        CallState::Connected => CALL_STATE_CONNECTED,
        CallState::Connecting => CALL_STATE_CONNECTING,
        CallState::Ended(..) => CALL_STATE_ENDED,
        CallState::Rejected(_) => CALL_STATE_REJECTED,
        CallState::Concluded => CALL_STATE_CONCLUDED,
    }
}

fn peer_id_cstring(peer_id: &str) -> CString {
    CString::new(peer_id).unwrap_or_else(|_| CString::new("").unwrap())
}

struct CCallbackDelegate {
    callbacks: Callbacks,
}

impl SignalingSender for CCallbackDelegate {
    fn send_signaling(
        &self,
        recipient_id: &str,
        call_id: CallId,
        receiver_device_id: Option<DeviceId>,
        message: signaling::Message,
    ) -> Result<()> {
        let recipient_cstr = peer_id_cstring(recipient_id);
        match message {
            signaling::Message::Offer(offer) => {
                (self.callbacks.send_offer)(
                    self.callbacks.context,
                    recipient_cstr.as_ptr(),
                    call_id.as_u64(),
                    offer.call_media_type as i32,
                    offer.opaque.as_ptr(),
                    offer.opaque.len(),
                );
            }
            signaling::Message::Answer(answer) => {
                (self.callbacks.send_answer)(
                    self.callbacks.context,
                    recipient_cstr.as_ptr(),
                    call_id.as_u64(),
                    answer.opaque.as_ptr(),
                    answer.opaque.len(),
                );
            }
            signaling::Message::Ice(ice) => {
                // One callback invocation per candidate, keeping the C ABI
                // simple (a candidate list would need its own allocation
                // dance across the boundary).
                for candidate in ice.candidates {
                    (self.callbacks.send_ice)(
                        self.callbacks.context,
                        recipient_cstr.as_ptr(),
                        call_id.as_u64(),
                        receiver_device_id.is_some(),
                        receiver_device_id.unwrap_or(0),
                        candidate.opaque.as_ptr(),
                        candidate.opaque.len(),
                    );
                }
            }
            signaling::Message::Hangup(hangup) => {
                let (hangup_type, device_id) = hangup.to_type_and_device_id();
                (self.callbacks.send_hangup)(
                    self.callbacks.context,
                    recipient_cstr.as_ptr(),
                    call_id.as_u64(),
                    hangup_type as i32,
                    device_id.unwrap_or(0),
                );
            }
            signaling::Message::Busy => {
                // Not used in this project's 1:1-only scope.
            }
        }
        Ok(())
    }

    // Group-call signaling: unused, this project is 1:1-only.
    fn send_call_message(
        &self,
        _recipient_id: UserId,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
    ) -> Result<()> {
        Ok(())
    }

    fn send_call_message_to_group(
        &self,
        _group_id: group_call::GroupId,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
        _recipients_override: HashSet<UserId>,
    ) -> Result<()> {
        Ok(())
    }

    fn send_call_message_to_adhoc_group(
        &self,
        _message: Vec<u8>,
        _urgency: group_call::SignalingMessageUrgency,
        _expiration: u64,
        _recipients_to_endorsements: HashMap<UserId, Vec<u8>>,
    ) -> Result<()> {
        Ok(())
    }
}

impl CallStateHandler for CCallbackDelegate {
    fn handle_call_state(
        &self,
        remote_peer_id: &str,
        call_id: CallId,
        state: CallState,
    ) -> Result<()> {
        let peer_cstr = peer_id_cstring(remote_peer_id);
        (self.callbacks.call_state)(
            self.callbacks.context,
            peer_cstr.as_ptr(),
            call_id.as_u64(),
            call_state_to_i32(&state),
        );
        Ok(())
    }

    // Not exposed via the C ABI yet - this project doesn't need remote
    // audio/video mute state or network-route/bandwidth diagnostics for a
    // 1:1 audio-only gateway.
    fn handle_remote_audio_state(&self, _remote_peer_id: &str, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn handle_remote_video_state(&self, _remote_peer_id: &str, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn handle_remote_sharing_screen(&self, _remote_peer_id: &str, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn handle_network_route(
        &self,
        _remote_peer_id: &str,
        _network_route: NetworkRoute,
    ) -> Result<()> {
        Ok(())
    }
    fn handle_audio_levels(
        &self,
        _remote_peer_id: &str,
        _captured_level: AudioLevel,
        _received_level: AudioLevel,
    ) -> Result<()> {
        Ok(())
    }
    fn handle_low_bandwidth_for_video(
        &self,
        _remote_peer_id: &str,
        _recovered: bool,
    ) -> Result<()> {
        Ok(())
    }
}

/// NativePlatform::new() takes SignalingSender and CallStateHandler as two
/// separate Box<dyn Trait + Send> - this thin newtype lets both boxes
/// forward to the same underlying CCallbackDelegate (sharing one set of
/// C callbacks) via Arc.
struct CCallbackDelegateRef(Arc<CCallbackDelegate>);

impl SignalingSender for CCallbackDelegateRef {
    fn send_signaling(
        &self,
        recipient_id: &str,
        call_id: CallId,
        receiver_device_id: Option<DeviceId>,
        message: signaling::Message,
    ) -> Result<()> {
        self.0
            .send_signaling(recipient_id, call_id, receiver_device_id, message)
    }
    fn send_call_message(
        &self,
        recipient_id: UserId,
        message: Vec<u8>,
        urgency: group_call::SignalingMessageUrgency,
    ) -> Result<()> {
        self.0.send_call_message(recipient_id, message, urgency)
    }
    fn send_call_message_to_group(
        &self,
        group_id: group_call::GroupId,
        message: Vec<u8>,
        urgency: group_call::SignalingMessageUrgency,
        recipients_override: HashSet<UserId>,
    ) -> Result<()> {
        self.0
            .send_call_message_to_group(group_id, message, urgency, recipients_override)
    }
    fn send_call_message_to_adhoc_group(
        &self,
        message: Vec<u8>,
        urgency: group_call::SignalingMessageUrgency,
        expiration: u64,
        recipients_to_endorsements: HashMap<UserId, Vec<u8>>,
    ) -> Result<()> {
        self.0.send_call_message_to_adhoc_group(
            message,
            urgency,
            expiration,
            recipients_to_endorsements,
        )
    }
}

impl CallStateHandler for CCallbackDelegateRef {
    fn handle_call_state(&self, remote_peer_id: &str, call_id: CallId, state: CallState) -> Result<()> {
        self.0.handle_call_state(remote_peer_id, call_id, state)
    }
    fn handle_remote_audio_state(&self, remote_peer_id: &str, enabled: bool) -> Result<()> {
        self.0.handle_remote_audio_state(remote_peer_id, enabled)
    }
    fn handle_remote_video_state(&self, remote_peer_id: &str, enabled: bool) -> Result<()> {
        self.0.handle_remote_video_state(remote_peer_id, enabled)
    }
    fn handle_remote_sharing_screen(&self, remote_peer_id: &str, enabled: bool) -> Result<()> {
        self.0.handle_remote_sharing_screen(remote_peer_id, enabled)
    }
    fn handle_network_route(&self, remote_peer_id: &str, network_route: NetworkRoute) -> Result<()> {
        self.0.handle_network_route(remote_peer_id, network_route)
    }
    fn handle_audio_levels(
        &self,
        remote_peer_id: &str,
        captured_level: AudioLevel,
        received_level: AudioLevel,
    ) -> Result<()> {
        self.0
            .handle_audio_levels(remote_peer_id, captured_level, received_level)
    }
    fn handle_low_bandwidth_for_video(&self, remote_peer_id: &str, recovered: bool) -> Result<()> {
        self.0.handle_low_bandwidth_for_video(remote_peer_id, recovered)
    }
}

/// Group calls are out of scope for this project (1:1 audio only) - every
/// GroupUpdate is silently ignored.
struct NoopGroupUpdateHandler;
impl GroupUpdateHandler for NoopGroupUpdateHandler {
    fn handle_group_update(&self, _update: GroupUpdate) -> Result<()> {
        Ok(())
    }
}

/// RingRTC's own HTTP client (used for group-call/SFU signaling) is never
/// invoked in this project's 1:1-only scope, so this delegate should never
/// actually be called - it exists only to satisfy CallManager::new's
/// required argument.
struct NoopHttpDelegate;
impl http::Delegate for NoopHttpDelegate {
    fn send_request(&self, _request_id: u32, _request: http::Request) {}
}

/// Opaque handle returned to C: the call manager plus a handle to the
/// per-manager raw-PCM audio device (Milestone D) for push/pull, since
/// AppCallContext (NativeCallContext) needs a real PeerConnectionFactory-
/// backed audio track at `proceed()` time.
pub struct CallManagerHandle {
    call_manager: CallManager<NativePlatform>,
    peer_connection_factory: PeerConnectionFactory,
    raw_pcm_adm: Arc<Mutex<RawPcmAudioDeviceModule>>,
}

fn cstr_to_string(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

/// Creates a new call manager backed by a cubeb-free, raw-PCM audio
/// device (Milestone D) instead of a real/virtual sound device. Returns
/// null on failure. `callbacks` is copied by value; its `context` pointer
/// is passed back on every invocation for the lifetime of the returned
/// handle.
#[unsafe(no_mangle)]
pub extern "C" fn signal2sip_call_manager_create(callbacks: Callbacks) -> *mut CallManagerHandle {
    let audio_config = AudioConfig::default();
    let (peer_connection_factory, raw_pcm_adm) =
        match PeerConnectionFactory::new_with_raw_pcm_adm(&audio_config, false, "") {
            Ok(result) => result,
            Err(e) => {
                error!("signal2sip_call_manager_create: failed to create PeerConnectionFactory: {}", e);
                return std::ptr::null_mut();
            }
        };

    let delegate = Arc::new(CCallbackDelegate { callbacks });
    let signaling_sender: Box<dyn SignalingSender + Send> = Box::new(CCallbackDelegateRef(delegate.clone()));
    let state_handler: Box<dyn CallStateHandler + Send> = Box::new(CCallbackDelegateRef(delegate));
    let group_handler: Box<dyn crate::native::GroupUpdateHandler + Send> =
        Box::new(NoopGroupUpdateHandler);

    let http_client = http::DelegatingClient::new(NoopHttpDelegate);

    let platform = NativePlatform::new(
        peer_connection_factory.clone(),
        signaling_sender,
        false,
        state_handler,
        group_handler,
    );

    let call_manager = match CallManager::new(platform, http_client) {
        Ok(cm) => cm,
        Err(e) => {
            error!("signal2sip_call_manager_create: failed to create CallManager: {}", e);
            return std::ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(CallManagerHandle {
        call_manager,
        peer_connection_factory,
        raw_pcm_adm,
    }))
}

/// Safety: `handle` must be a pointer returned by
/// signal2sip_call_manager_create and not yet destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_manager_destroy(handle: *mut CallManagerHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle) });
}

/// Pushes PCM samples (mono, 48kHz, i16) to be encoded and sent to the
/// remote peer once a call is connected.
///
/// Safety: `handle` must be a valid, non-null pointer from
/// signal2sip_call_manager_create; `samples`/`num_samples` must describe a
/// valid, readable buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_push_recorded_samples(
    handle: *mut CallManagerHandle,
    samples: *const i16,
    num_samples: usize,
) {
    if handle.is_null() || samples.is_null() {
        return;
    }
    let handle = unsafe { &*handle };
    let slice = unsafe { std::slice::from_raw_parts(samples, num_samples) };
    if let Ok(adm) = handle.raw_pcm_adm.lock() {
        adm.push_recorded_samples(slice);
    }
}

/// Pulls up to `max_samples` of decoded PCM (mono, 48kHz, i16) received
/// from the remote peer into `out`, returning how many samples were
/// actually written (may be fewer than `max_samples`, including zero).
///
/// Safety: `handle` must be a valid, non-null pointer from
/// signal2sip_call_manager_create; `out`/`max_samples` must describe a
/// valid, writable buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_pull_playout_samples(
    handle: *mut CallManagerHandle,
    out: *mut i16,
    max_samples: usize,
) -> usize {
    if handle.is_null() || out.is_null() {
        return 0;
    }
    let handle = unsafe { &*handle };
    let Ok(adm) = handle.raw_pcm_adm.lock() else {
        return 0;
    };
    let samples = adm.pull_playout_samples(max_samples);
    let n = samples.len();
    unsafe {
        std::ptr::copy_nonoverlapping(samples.as_ptr(), out, n);
    }
    n
}

fn make_call_context(handle: &CallManagerHandle) -> Result<NativeCallContext> {
    let outgoing_audio_track = handle.peer_connection_factory.create_outgoing_audio_track()?;
    let outgoing_video_source = handle.peer_connection_factory.create_outgoing_video_source()?;
    let outgoing_video_track = handle
        .peer_connection_factory
        .create_outgoing_video_track(&outgoing_video_source)?;
    Ok(NativeCallContext::new(
        false, // hide_ip
        vec![], // ice_servers - none needed for a same-host/synthetic test
        outgoing_audio_track,
        outgoing_video_track,
        Box::new(NoopVideoSink),
    ))
}

#[derive(Clone)]
struct NoopVideoSink;
impl VideoSink for NoopVideoSink {
    fn on_video_frame(&self, _demux_id: DemuxId, _frame: VideoFrame) {}
    fn box_clone(&self) -> Box<dyn VideoSink> {
        Box::new(NoopVideoSink)
    }
}

/// Places an outgoing 1:1 audio call. `remote_peer_id`/`local_device_id`
/// identify the callee/caller for signaling purposes only (this project
/// maps one Signal serviceId to one remote_peer_id). Returns the new
/// call's id, or 0 on failure (0 is never a real CallId - see
/// CallId::random()'s u64 range).
///
/// Safety: `handle`/`remote_peer_id` must be valid, non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_start_outgoing(
    handle: *mut CallManagerHandle,
    remote_peer_id: *const c_char,
    local_device_id: u32,
) -> u64 {
    if handle.is_null() {
        return 0;
    }
    let handle = unsafe { &mut *handle };
    let peer_id = cstr_to_string(remote_peer_id);
    let call_id = CallId::random();
    if let Err(e) = handle.call_manager.create_outgoing_call(
        peer_id,
        call_id,
        CallMediaType::Audio,
        local_device_id,
    ) {
        error!("signal2sip_call_start_outgoing failed: {}", e);
        return 0;
    }
    call_id.as_u64()
}

/// Feeds a received offer (opaque bytes from a real Signal CallMessage's
/// offer.opaque, or - in the synthetic test - directly from another
/// in-process CallManager's send_offer callback) into the call manager,
/// which will itself invoke the accept/proceed flow via callbacks.
///
/// Safety: `handle`/`remote_peer_id`/`opaque` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_received_offer(
    handle: *mut CallManagerHandle,
    remote_peer_id: *const c_char,
    call_id: u64,
    sender_device_id: u32,
    receiver_device_id: u32,
    opaque: *const u8,
    opaque_len: usize,
) -> bool {
    if handle.is_null() || opaque.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    let peer_id = cstr_to_string(remote_peer_id);
    let opaque_bytes = unsafe { std::slice::from_raw_parts(opaque, opaque_len) }.to_vec();
    let offer = match signaling::Offer::new(CallMediaType::Audio, opaque_bytes) {
        Ok(o) => o,
        Err(e) => {
            error!("signal2sip_call_received_offer: bad offer: {}", e);
            return false;
        }
    };
    let received = signaling::ReceivedOffer {
        offer,
        age: std::time::Duration::from_secs(0),
        sender_device_id,
        receiver_device_id,
        sender_identity_key: vec![0u8; 32],
        receiver_identity_key: vec![0u8; 32],
    };
    match handle
        .call_manager
        .received_offer(peer_id, CallId::new(call_id), received)
    {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_received_offer failed: {}", e);
            false
        }
    }
}

/// Accepts the (assumed-active) incoming call `call_id`. IMPORTANT: must
/// only be called once the call has reached CALL_STATE_RINGING (connected
/// but not yet accepted) - accept_call() is only valid once the
/// underlying connection is actually established. Calling it right after
/// signal2sip_call_proceed() without waiting is a real bug this project
/// hit: the call FSM logs "Unexpected event AcceptCall, while in state
/// ConnectingBeforeAccepted" and silently drops it, leaving the call
/// stuck forever - see CallState::Ringing's own doc comment in native.rs
/// ("currently can be stuck here if you accept incoming before Ringing").
///
/// Safety: `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_accept(handle: *mut CallManagerHandle, call_id: u64) -> bool {
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    match handle.call_manager.accept_call(CallId::new(call_id)) {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_accept: accept_call failed: {}", e);
            false
        }
    }
}

/// Proceeds with an outgoing call once the caller decides to actually
/// connect (mirrors the same NativeCallContext construction as accept, but
/// without accept_call - proceed() alone is the outgoing-call equivalent).
///
/// Safety: `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_proceed(handle: *mut CallManagerHandle, call_id: u64) -> bool {
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    let call_context = match make_call_context(handle) {
        Ok(ctx) => ctx,
        Err(e) => {
            error!("signal2sip_call_proceed: failed to build call context: {}", e);
            return false;
        }
    };
    match handle.call_manager.proceed(
        CallId::new(call_id),
        call_context,
        crate::common::CallConfig::default(),
        None,
    ) {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_proceed failed: {}", e);
            false
        }
    }
}

/// Safety: `handle`/`remote_peer_id`/`opaque` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_received_answer(
    handle: *mut CallManagerHandle,
    remote_peer_id: *const c_char,
    call_id: u64,
    sender_device_id: u32,
    opaque: *const u8,
    opaque_len: usize,
) -> bool {
    if handle.is_null() || opaque.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    let peer_id = cstr_to_string(remote_peer_id);
    let opaque_bytes = unsafe { std::slice::from_raw_parts(opaque, opaque_len) }.to_vec();
    let answer = match signaling::Answer::new(opaque_bytes) {
        Ok(a) => a,
        Err(e) => {
            error!("signal2sip_call_received_answer: bad answer: {}", e);
            return false;
        }
    };
    let received = signaling::ReceivedAnswer {
        answer,
        sender_device_id,
        sender_identity_key: vec![0u8; 32],
        receiver_identity_key: vec![0u8; 32],
    };
    match handle
        .call_manager
        .received_answer(peer_id, CallId::new(call_id), received)
    {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_received_answer failed: {}", e);
            false
        }
    }
}

/// Safety: `handle`/`remote_peer_id`/`opaque` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_received_ice(
    handle: *mut CallManagerHandle,
    remote_peer_id: *const c_char,
    call_id: u64,
    sender_device_id: u32,
    opaque: *const u8,
    opaque_len: usize,
) -> bool {
    if handle.is_null() || opaque.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    let peer_id = cstr_to_string(remote_peer_id);
    let opaque_bytes = unsafe { std::slice::from_raw_parts(opaque, opaque_len) }.to_vec();
    let received = signaling::ReceivedIce {
        ice: signaling::Ice {
            candidates: vec![signaling::IceCandidate {
                opaque: opaque_bytes,
            }],
        },
        sender_device_id,
    };
    match handle
        .call_manager
        .received_ice(peer_id, CallId::new(call_id), received)
    {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_received_ice failed: {}", e);
            false
        }
    }
}

/// Must be called once the application has finished actually delivering
/// the most recent signaling message handed to it via a `send_*` callback
/// (offer/answer/ice/hangup) - RingRTC's own signaling queue serializes
/// messages per call and will not invoke the next `send_*` callback until
/// this (or `signal2sip_call_message_send_failure`) is called. Missing
/// this call is a real bug this project hit: without it, only the very
/// first signaling message per call (the offer or answer) is ever
/// delivered - every ICE candidate queues up behind it forever, so the
/// call gathers candidates but neither side ever learns the other's,
/// and the call sits in ConnectingBeforeAccepted until RingRTC's own
/// ~60s CallTimeout ends it. See `assume_messages_sent()` in native.rs -
/// this project's CallManager is built with `should_assume_messages_sent
/// = false`, so this call is mandatory, not optional.
///
/// Safety: `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_message_sent(handle: *mut CallManagerHandle, call_id: u64) -> bool {
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    match handle.call_manager.message_sent(CallId::new(call_id)) {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_message_sent failed: {}", e);
            false
        }
    }
}

/// Counterpart to `signal2sip_call_message_sent` for when delivery of the
/// most recent signaling message actually failed (e.g. a real network/HTTP
/// error sending a Signal CallMessage) - lets RingRTC continue draining its
/// signaling queue (dropping/skipping that message) instead of stalling
/// forever the way a missing `message_sent()` call would.
///
/// Safety: `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_message_send_failure(handle: *mut CallManagerHandle, call_id: u64) -> bool {
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    match handle.call_manager.message_send_failure(CallId::new(call_id)) {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_message_send_failure failed: {}", e);
            false
        }
    }
}

/// Local hangup of the currently-active call (RingRTC only tracks one
/// active call at a time per CallManager - matches this project's
/// one-process-per-account, one-call-at-a-time scope).
///
/// Safety: `handle` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal2sip_call_hangup(handle: *mut CallManagerHandle) -> bool {
    if handle.is_null() {
        return false;
    }
    let handle = unsafe { &mut *handle };
    match handle.call_manager.hangup() {
        Ok(()) => true,
        Err(e) => {
            error!("signal2sip_call_hangup failed: {}", e);
            false
        }
    }
}
