use super::{
    is_microphone_access_denied, is_no_input_device_error, run_consumer, AudioRecorder,
    CaptureProcessor, CaptureTransportState, ChunkDisposition, Cmd, VadConfig, VadPolicy,
};

// ---- remote capture source (companion devices) -----------------------------

/// Drive a full remote session: open_remote, start, push audio, stop
/// (acknowledging the pause like the server does on a healthy release),
/// and collect the finalized samples. Returns (source, samples).
fn run_remote_session(input: &[f32], policy: VadPolicy) -> (super::RemoteAudioSource, Vec<f32>) {
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");
    assert!(source.is_open());

    let ready = recorder.start(policy).expect("start");
    source.push_chunk(input);
    ready
        .recv_timeout(Duration::from_secs(1))
        .expect("remote capture ready");

    let stop_handle = thread::spawn(move || recorder.stop().expect("stop returns samples"));
    wait_for_pause_request(&source, Duration::from_secs(1));
    source.ack_pause();
    let samples = stop_handle.join().expect("join stop");
    (source, samples)
}

#[test]
fn remote_source_round_trips_fed_samples() {
    let input: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.01) - 8.0).collect();
    let (source, samples) = run_remote_session(&input, VadPolicy::Disabled);

    // The input arrives byte-for-byte, followed by the same zero-padding to
    // a whole frame the LOCAL path's finish_recording applies (the frame
    // resampler pads its pending partial frame; 480 samples with no VAD
    // backend attached).
    assert_eq!(&samples[..input.len()], &input[..]);
    let frame = 480usize;
    let padded_len = input.len() + (frame - input.len() % frame) % frame;
    assert_eq!(samples.len(), padded_len, "trailing frame padding only");
    assert!(samples[input.len()..].iter().all(|&s| s == 0.0));
    // The stop handshake was acknowledged by the push path, so no overrun
    // was recorded.
    assert_eq!(source.overrun_samples(), 0);
}

#[test]
fn remote_source_runs_the_vad_passthrough_path() {
    // A passthrough detector with a non-default frame size proves the remote
    // recorder shares the CaptureProcessor (frame re-chunking through the
    // same-rate resampler) rather than bypassing it.
    let frame_samples = 256;
    let mut recorder = AudioRecorder::new().expect("recorder").with_vad(
        Box::new(FixedFrameVad(frame_samples)),
        0,
        0,
    );
    let source = recorder.open_remote().expect("open remote source");

    let ready = recorder.start(VadPolicy::Offline).expect("start");
    let input: Vec<f32> = vec![0.5; 1024];
    source.push_chunk(&input);
    ready
        .recv_timeout(Duration::from_secs(1))
        .expect("remote capture ready");

    let stop_handle = thread::spawn(move || recorder.stop().expect("stop"));
    wait_for_pause_request(&source, Duration::from_secs(1));
    source.ack_pause();
    let samples = stop_handle.join().expect("join stop");
    assert_eq!(samples.len(), 1024);
    assert!(samples.iter().all(|&s| s == 0.5));
}

#[test]
fn remote_stop_with_acked_dead_source_completes_fast_and_returns_audio() {
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");
    let ready = recorder.start(VadPolicy::Disabled).expect("start");
    source.push_chunk(&[1.0, 2.0, 3.0]);
    ready.recv_timeout(Duration::from_secs(1)).expect("ready");

    // The phone died: no boundary block will arrive. Stop begins, and once
    // the pause request is live the server's drop handler acknowledges it,
    // which must keep Stop well under the 2 s PAUSE_ACK_TIMEOUT while still
    // finalizing what was captured.
    let stop_handle = thread::spawn(move || recorder.stop().expect("stop"));
    wait_for_pause_request(&source, Duration::from_secs(1));
    source.ack_pause();
    let began = Instant::now();
    let samples = stop_handle.join().expect("join stop");
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "acknowledged stop must not wait the 2s pause timeout, took {:?}",
        began.elapsed()
    );
    assert_eq!(&samples[..3], &[1.0, 2.0, 3.0]);
}

#[test]
fn remote_stop_with_silent_dead_source_still_finalizes_within_the_pause_timeout() {
    // Worst case: the phone vanishes and nobody acknowledges. The consumer's
    // pause_timed_out path must still return the buffered samples before it
    // exits (the graceful-finalize requirement).
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");
    let ready = recorder.start(VadPolicy::Disabled).expect("start");
    source.push_chunk(&[4.0, 5.0]);
    ready.recv_timeout(Duration::from_secs(1)).expect("ready");

    let began = Instant::now();
    let samples = recorder.stop().expect("stop returns buffered samples");
    assert!(
        began.elapsed() >= Duration::from_millis(1500),
        "unacknowledged stop waits out the pause timeout (took {:?})",
        began.elapsed()
    );
    assert_eq!(&samples[..2], &[4.0, 5.0]);
}

#[test]
fn remote_source_counts_overruns_when_the_ring_fills() {
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");

    // Two seconds of ring capacity; push five seconds without draining so
    // the tail is dropped-newest and counted.
    let big: Vec<f32> = vec![0.1; constants::WHISPER_SAMPLE_RATE as usize * 5];
    source.push_chunk(&big);
    assert!(source.overrun_samples() >= 3 * constants::WHISPER_SAMPLE_RATE as u64);

    let _ = recorder.close();
}

#[test]
fn remote_source_drop_acknowledges_an_in_flight_pause() {
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");
    let ready = recorder.start(VadPolicy::Disabled).expect("start");
    source.push_chunk(&[7.0]);
    ready.recv_timeout(Duration::from_secs(1)).expect("ready");

    // Stop in a worker, then drop every source handle (the socket died) as
    // soon as the pause request is live: the drop acknowledges the pause so
    // Stop returns promptly instead of timing out.
    let stop_handle = thread::spawn(move || recorder.stop().expect("stop"));
    wait_for_pause_request(&source, Duration::from_secs(1));
    let began = Instant::now();
    drop(source);
    let samples = stop_handle.join().expect("join stop");
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "drop must acknowledge the pause promptly, took {:?}",
        began.elapsed()
    );
    assert_eq!(&samples[..1], &[7.0]);
}

/// Wait until the consumer's Stop has raised the pause request, so an ack
/// that follows lands inside the handshake window (an early ack is wiped
/// when the consumer starts the pause).
fn wait_for_pause_request(source: &super::RemoteAudioSource, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !source.pause_requested() {
        assert!(Instant::now() < deadline, "pause was not requested");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn remote_source_close_discards_late_audio() {
    let mut recorder = AudioRecorder::new().expect("recorder");
    let source = recorder.open_remote().expect("open remote source");
    source.close();
    assert!(!source.is_open());
    // Pushing after close must not panic or register overruns.
    source.push_chunk(&[1.0, 2.0]);
    assert_eq!(source.overrun_samples(), 0);
    let _ = recorder.close();
}
use crate::audio_toolkit::constants;
use crate::audio_toolkit::vad::{VadFrame, VoiceActivityDetector};
use rtrb::RingBuffer;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn unopened_recorder_does_not_need_reopen() {
    let recorder = AudioRecorder::new().expect("recorder");
    assert!(!recorder.needs_reopen());
}

#[test]
fn stream_error_requires_reopen() {
    let recorder = AudioRecorder::new().expect("recorder");
    recorder.stream_error.store(true, Ordering::Relaxed);
    assert!(recorder.needs_reopen());
}

/// Pass-through detector with a configurable frame size, standing in for a
/// backend such as Earshot whose frames are not 30 ms.
struct FixedFrameVad(usize);

impl VoiceActivityDetector for FixedFrameVad {
    fn push_frame<'a>(&'a mut self, frame: &'a [f32]) -> anyhow::Result<VadFrame<'a>> {
        Ok(VadFrame::Speech(frame))
    }

    fn frame_samples(&self) -> usize {
        self.0
    }
}

#[test]
fn resampler_frame_size_follows_the_vad_backend() {
    let frame_samples = 256;
    let vad = VadConfig {
        detector: Arc::new(Mutex::new(Box::new(FixedFrameVad(frame_samples)))),
        frame_samples,
        offline_hangover_frames: 0,
        streaming_hangover_frames: 0,
    };
    let frame_lengths = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&frame_lengths);
    let mut processor = CaptureProcessor::new(
        16_000,
        Some(vad),
        None,
        Some(Arc::new(move |frame: &[f32]| {
            observed.lock().unwrap().push(frame.len())
        })),
        Instant::now(),
    );

    let (ready_tx, _ready_rx) = mpsc::channel();
    processor.begin_recording(VadPolicy::Offline, ready_tx);
    processor.process_raw_chunk(&[0.0; 1024], ChunkDisposition::Capture);
    let samples = processor.finish_recording();

    assert_eq!(samples.len(), 1024);
    assert_eq!(*frame_lengths.lock().unwrap(), vec![frame_samples; 4]);
}

#[test]
fn idle_chunks_are_discarded_without_reaching_the_recording() {
    let mut processor = CaptureProcessor::new(16_000, None, None, None, Instant::now());
    processor.process_raw_chunk(&[1.0; 480], ChunkDisposition::Discard);
    assert!(processor.finish_recording().is_empty());
}

#[test]
fn shutdown_is_processed_without_audio_samples() {
    let (_producer, consumer) = RingBuffer::<f32>::new(48_000);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        run_consumer(
            CaptureProcessor::new(48_000, None, None, None, Instant::now()),
            consumer,
            cmd_rx,
            Arc::new(CaptureTransportState::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let _ = done_tx.send(());
    });

    cmd_tx.send(Cmd::Shutdown).expect("send shutdown");
    assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    worker.join().expect("join consumer");
}

#[test]
fn callback_writes_mono_samples() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    let transport = CaptureTransportState::default();

    AudioRecorder::write_input_to_ring(&[0.25f32, -0.5, 1.0], 1, None, &mut producer, &transport);

    let mut output = [0.0; 3];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [0.25, -0.5, 1.0]);
}

#[test]
fn callback_downmixes_or_selects_multichannel_input() {
    let transport = CaptureTransportState::default();
    let (mut average_tx, mut average_rx) = RingBuffer::<f32>::new(4);
    AudioRecorder::write_input_to_ring(
        &[1.0f32, 3.0, -1.0, 1.0],
        2,
        None,
        &mut average_tx,
        &transport,
    );
    let mut averaged = [0.0; 2];
    average_rx
        .pop_entire_slice(&mut averaged)
        .expect("averaged samples");
    assert_eq!(averaged, [2.0, 0.0]);

    let (mut selected_tx, mut selected_rx) = RingBuffer::<f32>::new(4);
    AudioRecorder::write_input_to_ring(
        &[1.0f32, 3.0, -1.0, 1.0],
        2,
        Some(1),
        &mut selected_tx,
        &transport,
    );
    let mut selected = [0.0; 2];
    selected_rx
        .pop_entire_slice(&mut selected)
        .expect("selected samples");
    assert_eq!(selected, [3.0, 1.0]);
}

#[test]
fn callback_forwards_boundary_block_then_stays_silent_until_resumed() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    let transport = CaptureTransportState::default();

    // The block in hand when a pause is first observed was captured before
    // the stop, so it is forwarded and only then acknowledged.
    transport.pause_requested.store(true, Ordering::Release);
    AudioRecorder::write_input_to_ring(&[1.0f32, 2.0], 1, None, &mut producer, &transport);
    assert!(transport.pause_acknowledged.load(Ordering::Acquire));
    assert_eq!(consumer.slots(), 2);

    // Later blocks while paused are dropped and are not counted as overruns.
    AudioRecorder::write_input_to_ring(&[3.0f32], 1, None, &mut producer, &transport);
    assert_eq!(consumer.slots(), 2);
    assert_eq!(transport.overrun_samples.load(Ordering::Relaxed), 0);

    // Clearing the pause, as the consumer does before stop() returns, resumes capture.
    transport.pause_acknowledged.store(false, Ordering::Relaxed);
    transport.pause_requested.store(false, Ordering::Release);
    AudioRecorder::write_input_to_ring(&[4.0f32], 1, None, &mut producer, &transport);
    let mut output = [0.0; 3];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [1.0, 2.0, 4.0]);
    assert!(!transport.pause_acknowledged.load(Ordering::Acquire));
}

#[test]
fn callback_partially_fills_ring_and_counts_dropped_audio() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(2);
    let transport = CaptureTransportState::default();

    AudioRecorder::write_input_to_ring(&[1.0f32, 2.0, 3.0], 1, None, &mut producer, &transport);

    let mut captured = [0.0; 2];
    consumer
        .pop_entire_slice(&mut captured)
        .expect("partial callback audio");
    assert_eq!(captured, [1.0, 2.0]);
    assert_eq!(transport.overrun_samples.load(Ordering::Relaxed), 1);
}

#[test]
fn bounded_drain_leaves_remaining_samples_for_the_next_command_cycle() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    producer
        .push_entire_slice(&[1.0, 2.0, 3.0, 4.0, 5.0])
        .expect("samples");
    let mut drained = Vec::new();

    let count =
        super::drain_available_samples(&mut consumer, 3, |part| drained.extend_from_slice(part));

    assert_eq!(count, 3);
    assert_eq!(drained, [1.0, 2.0, 3.0]);
    assert_eq!(consumer.slots(), 2);
}

#[test]
fn ring_wraparound_preserves_both_read_slices_in_order() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(5);
    let transport = CaptureTransportState::default();
    producer
        .push_entire_slice(&[1.0, 2.0, 3.0, 4.0])
        .expect("initial samples");
    let mut discarded = [0.0; 3];
    consumer
        .pop_entire_slice(&mut discarded)
        .expect("advance ring head");

    AudioRecorder::write_input_to_ring(
        &[5.0f32, 6.0, 7.0, 8.0],
        1,
        None,
        &mut producer,
        &transport,
    );

    let chunk = consumer.read_chunk(5).expect("wrapped samples");
    let (first, second) = chunk.as_slices();
    assert!(!first.is_empty());
    assert!(!second.is_empty());
    let ordered = first
        .iter()
        .chain(second.iter())
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(ordered, [4.0, 5.0, 6.0, 7.0, 8.0]);
}

#[test]
fn repeated_start_stop_cycles_resume_capture_without_leaking_samples() {
    let (mut producer, consumer) = RingBuffer::<f32>::new(16_000);
    let transport = Arc::new(CaptureTransportState::default());
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let streamed = Arc::new(Mutex::new(Vec::new()));
    let streamed_cb = Arc::clone(&streamed);
    let consumer_transport = Arc::clone(&transport);
    let worker = thread::spawn(move || {
        let processor = CaptureProcessor::new(
            16_000,
            None,
            None,
            Some(Arc::new(move |frame: &[f32]| {
                streamed_cb.lock().unwrap().extend_from_slice(frame)
            })),
            Instant::now(),
        );
        run_consumer(
            processor,
            consumer,
            cmd_rx,
            consumer_transport,
            Arc::new(AtomicBool::new(false)),
        );
    });

    let wait_for_pause_request = || {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !transport.pause_requested.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "pause was not requested");
            thread::sleep(Duration::from_millis(1));
        }
    };

    let first_input = [0.25f32, -0.5, 1.0];
    let (ready_tx, ready_rx) = mpsc::channel();
    cmd_tx
        .send(Cmd::Start(VadPolicy::Disabled, Instant::now(), ready_tx))
        .expect("first start");
    AudioRecorder::write_input_to_ring(&first_input, 1, None, &mut producer, &transport);
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first capture ready");

    let (reply_tx, reply_rx) = mpsc::channel();
    cmd_tx.send(Cmd::Stop(reply_tx)).expect("first stop");
    wait_for_pause_request();
    // The first callback after Stop carries audio captured before the stop,
    // so it belongs to the recording.
    AudioRecorder::write_input_to_ring(&[99.0f32], 1, None, &mut producer, &transport);

    let first_samples = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("first stop reply");
    let first_expected = [0.25f32, -0.5, 1.0, 99.0];
    assert_eq!(&first_samples[..first_expected.len()], &first_expected);
    assert!(first_samples[first_expected.len()..]
        .iter()
        .all(|&sample| sample == 0.0));
    assert!(!transport.pause_requested.load(Ordering::Acquire));

    let first_streamed_len = {
        let streamed = streamed.lock().unwrap();
        assert_eq!(&streamed[..first_expected.len()], &first_expected);
        streamed.len()
    };

    // Start again immediately after stop() would have returned. The producer
    // must already be re-enabled, and no first-cycle samples may leak through.
    let second_input = [0.75f32, -0.25, 0.5];
    let (ready_tx, ready_rx) = mpsc::channel();
    cmd_tx
        .send(Cmd::Start(VadPolicy::Disabled, Instant::now(), ready_tx))
        .expect("second start");
    AudioRecorder::write_input_to_ring(&second_input, 1, None, &mut producer, &transport);
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second capture ready");

    let (reply_tx, reply_rx) = mpsc::channel();
    cmd_tx.send(Cmd::Stop(reply_tx)).expect("second stop");
    wait_for_pause_request();
    AudioRecorder::write_input_to_ring(&[199.0f32], 1, None, &mut producer, &transport);

    let second_samples = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("second stop reply");
    let second_expected = [0.75f32, -0.25, 0.5, 199.0];
    assert_eq!(&second_samples[..second_expected.len()], &second_expected);
    assert!(second_samples[second_expected.len()..]
        .iter()
        .all(|&sample| sample == 0.0));
    assert!(!first_samples
        .iter()
        .any(|sample| second_expected.contains(sample)));
    assert!(!second_samples
        .iter()
        .any(|sample| first_expected.contains(sample)));
    assert!(!transport.pause_requested.load(Ordering::Acquire));

    {
        let streamed = streamed.lock().unwrap();
        assert_eq!(streamed.len(), first_streamed_len + second_samples.len());
        assert_eq!(
            &streamed[first_streamed_len..first_streamed_len + second_expected.len()],
            &second_expected
        );
    }

    cmd_tx.send(Cmd::Shutdown).expect("shutdown");
    worker.join().expect("consumer worker");
}

#[test]
fn missing_callback_at_stop_marks_stream_for_rebuild_and_returns_samples() {
    let (_producer, consumer) = RingBuffer::<f32>::new(16_000);
    let transport = Arc::new(CaptureTransportState::default());
    let stream_error = Arc::new(AtomicBool::new(false));
    let observed_error = Arc::clone(&stream_error);
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let worker_transport = Arc::clone(&transport);
    let worker = thread::spawn(move || {
        run_consumer(
            CaptureProcessor::new(16_000, None, None, None, Instant::now()),
            consumer,
            cmd_rx,
            worker_transport,
            stream_error,
        );
    });

    let (ready_tx, _ready_rx) = mpsc::channel();
    cmd_tx
        .send(Cmd::Start(VadPolicy::Disabled, Instant::now(), ready_tx))
        .expect("start");
    let (reply_tx, reply_rx) = mpsc::channel();
    cmd_tx.send(Cmd::Stop(reply_tx)).expect("stop");

    let samples = reply_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("pause timeout still returns captured samples");
    assert!(samples.is_empty());
    worker.join().expect("consumer exits after pause timeout");
    assert!(observed_error.load(Ordering::Acquire));
}

#[test]
fn detects_access_is_denied() {
    assert!(is_microphone_access_denied("Access is denied"));
}

#[test]
fn detects_permission_denied() {
    assert!(is_microphone_access_denied("permission denied"));
}

#[test]
fn detects_windows_error_code() {
    assert!(is_microphone_access_denied("WASAPI error: 0x80070005"));
}

#[test]
fn does_not_match_unrelated_errors() {
    assert!(!is_microphone_access_denied("device not found"));
}

#[test]
fn detects_no_input_device() {
    assert!(is_no_input_device_error("No input device found"));
}

#[test]
fn detects_coreaudio_config_error() {
    assert!(is_no_input_device_error(
        "Failed to fetch preferred config: A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred"
    ));
}

#[test]
fn does_not_match_other_errors_for_no_device() {
    assert!(!is_no_input_device_error("permission denied"));
    assert!(!is_no_input_device_error("device not found"));
}
