use anyhow::{Context, Result};
use cpal::{
    traits::*, Device, FromSample, InputCallbackInfo, OutputCallbackInfo, Sample, SampleFormat,
    Stream, StreamConfig,
};
use crossbeam::sync::{Parker, Unparker};
use midir::{
    MidiInput, MidiInputConnection, MidiInputPort, MidiOutput, MidiOutputConnection, MidiOutputPort,
};
use parking_lot::Mutex;
use rtrb::RingBuffer;
use std::borrow::Borrow;
use std::num::NonZeroU32;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::ScopedJoinHandle;
use std::time::{Duration, Instant};

use super::super::config::WrapperConfig;
use super::super::{publish_audio_devices_in_use, AudioDevicesInUse};
use super::device_watch::{DeviceWatch, Kind, Wanted};
use super::{sleep_unless, Backend, RunOutcome};
use crate::midi::MidiResult;
use crate::prelude::{
    AudioIOLayout, AuxiliaryBuffers, Buffer, MidiConfig, NoteEvent, Plugin, PluginNoteEvent,
    Transport,
};
use crate::wrapper::util::buffer_management::{BufferManager, ChannelPointers};

const MIDI_EVENT_QUEUE_CAPACITY: usize = 2048;

/// How often the device NAMES are listed while a requested device is not open, or while waiting
/// for the hardware to change. A plain listing: no configuration queries, so it costs nothing
/// audible.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a probe waits for the first callback of the stream it started on a wanted device.
const PROBE_FIRST_CALLBACK: Duration = Duration::from_secs(1);

/// Uses CPAL for audio and midir for MIDI.
pub struct CpalMidir {
    config: WrapperConfig,
    audio_io_layout: AudioIOLayout,
    /// Needed to re-select devices in `reinit()` after the audio stream died.
    host_id: cpal::HostId,
    /// The host the devices were opened through, kept for `reinit()`: an ASIO host remembers the
    /// driver it loaded, so its enumeration hands that driver back and refuses every other one,
    /// where a fresh host would load (and so unload) drivers under the running one (A7).
    host: cpal::Host,
    /// A single-device duplex host, see `single_device_duplex`.
    duplex: bool,
    /// The period the wrapper was initialized with; a duplex restart never exceeds it.
    launch_period: u32,
    /// Duplex input samples dropped on a full ring, and output samples silenced on an empty one.
    overflows: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,

    input: Option<CpalDevice>,
    output: CpalDevice,

    midi_input: Mutex<Option<MidirInputDevice>>,
    midi_output: Mutex<Option<MidirOutputDevice>>,

    /// The requested devices that are not open, and when to try them again.
    watch: Mutex<DeviceWatch>,
    /// What the stream is open on, as published to the host application.
    in_use: Mutex<AudioDevicesInUse>,
    /// Requested devices the stream kept dying on: the next restart stands something else in
    /// for them and the watch leaves them alone for a while.
    quarantined: Vec<Wanted>,
}

/// All data needed for a CPAL input or output stream.
struct CpalDevice {
    pub device: Device,
    pub config: StreamConfig,
    pub sample_format: SampleFormat,
}

/// Whether the devices are opened for a start or for a restart after a stream failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Start {
    Launch,
    Restart,
}

impl Start {
    /// On a start, the log line for a device that is not connected lists what is, as the error
    /// for an unknown name used to; a restart already knows.
    fn available(self, host: &cpal::Host) -> String {
        match self {
            Start::Launch => {
                let mut listing = String::from(". Available devices are:");
                for name in device_names(host) {
                    listing.push('\n');
                    listing.push_str(&name);
                }
                listing
            }
            Start::Restart => String::new(),
        }
    }
}

/// What a requested device resolves to right now.
enum Resolved {
    /// The host does not list it.
    Absent,
    /// The host lists it, but it cannot run the stream.
    Refused(anyhow::Error),
    Open(CpalDevice),
}

/// The requested devices a start or restart could not open, by what happens to them next.
#[derive(Default)]
struct OpenedDevicesSoFar {
    absent: Vec<Wanted>,
    refused: Vec<Wanted>,
    quarantined: Vec<Wanted>,
}

impl OpenedDevicesSoFar {
    fn refused(&mut self, wanted: Wanted, quarantined: &[Wanted]) {
        if quarantined.contains(&wanted) {
            self.quarantined.push(wanted);
        } else {
            self.refused.push(wanted);
        }
    }
}

/// The devices a start or restart opened, and the requested ones it could not.
struct OpenedDevices {
    output: CpalDevice,
    input: Option<CpalDevice>,
    in_use: AudioDevicesInUse,
    absent: Vec<Wanted>,
    refused: Vec<Wanted>,
    quarantined: Vec<Wanted>,
}

/// All data needed to create a Midir input stream.
struct MidirInputDevice {
    pub backend: MidiInput,
    pub port: MidiInputPort,
}

/// An active `MidirInputDevice`. Transformed back and from this during the `.run()` function.
struct ActiveMidirInputDevice {
    pub connection: MidiInputConnection<()>,
    pub port: MidiInputPort,
}

/// All data needed to create a Midir output stream.
struct MidirOutputDevice {
    pub backend: MidiOutput,
    pub port: MidiOutputPort,
}

/// An active `MidirOutputDevice`. Transformed back and from this during the `.run()` function.
struct ActiveMidirOutputDevice {
    pub connection: MidiOutputConnection,
    pub port: MidiOutputPort,
}

/// Send+Sync wrapper for `Vec<*mut f32>` so we can preallocate channel pointer vectors for use with
/// the `BufferManager` API.
struct ChannelPointerVec(Vec<*mut f32>);

unsafe impl Send for ChannelPointerVec {}
unsafe impl Sync for ChannelPointerVec {}

impl ChannelPointerVec {
    // If you directly access the `.0` field then it will try to move it out of the struct which
    // undoes the Send+Sync impl.
    pub fn get(&mut self) -> &mut Vec<*mut f32> {
        &mut self.0
    }
}

/// A task for the MIDI output thread.
enum MidiOutputTask<P: Plugin> {
    /// Send an event as MIDI data.
    Send(PluginNoteEvent<P>),
    /// Terminate the thread, stopping it from blocking and allowing it to be joined.
    Terminate,
}

impl<P: Plugin> Backend<P> for CpalMidir {
    fn run(
        &mut self,
        should_stop: Arc<AtomicBool>,
        mut cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
    ) -> RunOutcome {
        // Distinguishes why this function returns: `stream_error` is set by the stream error
        // callbacks (e.g. the audio device disappeared), `callback_stopped` is set by the wrapped
        // process callback below when the caller's `cb` requests a stop. If neither is set the
        // wakeup came from `should_stop`.
        let stream_error = Arc::new(AtomicBool::new(false));
        let callback_stopped = Arc::new(AtomicBool::new(false));
        // A duplex driver never reports a dead stream, so the output callback stamps this and the
        // wait below watches it (A6).
        let liveness = Liveness::new();
        // So this is a lot of fun. There are up to four separate streams here, all using their own
        // callbacks. The audio output stream acts as the primary stream, and everything else either
        // sends data to it or (in the case of the MIDI output stream) receives data from it using
        // channels.
        //
        // Audio input is read from the input device (if configured), and is send at a period at a
        // time to the output stream interleaved, one frame of the plugin's main input channels at a
        // time. Because of that the audio output stream is delayed for one period using a parker to
        // you don't immediately get xruns. CPAL audio devices may also not accept floating point
        // samples, so all of the actual audio handling and buffer management handles in the
        // `build_*_data_callback()` functions defined below.
        //
        // MIDI input is parsed in the Midir callback and the events are sent over a callback to the
        // output audio thread where the process callback happens. If that process callback outputs
        // events then those are sent over another ringbuffer to a thread that handles MIDI output.
        // Both MIDI input and MIDI output are disabled by default.
        //
        // The thread scope is needed to accomodate the midir MIDI output API. Outputting MIDI is
        // realtime unsafe, and to be able to output MIDI with midir you need to transform between
        // `MidiOutputPort` and `MidiOutputPortConnection` types by taking values out of an
        // `Option`.
        std::thread::scope(|s| {
            // This thread needs to be blocked until audio processing ends as CPAL processes the
            // streams on other threads. Created up front so the input stream's error callback can
            // also wake it (a dead capture stream mid-session should end the run, not leave the
            // output spinning on an input ring buffer that will never fill up again).
            let parker = Parker::new();
            let unparker = parker.unparker().clone();

            let mut _input_stream: Option<Stream> = None;
            let mut input_rb_consumer: Option<rtrb::Consumer<f32>> = None;
            if let Some(input) = &self.input {
                // Data is sent to the output data callback using a wait-free ring buffer, one
                // interleaved frame of the plugin's main input channel count at a time
                let ring_channels = self
                    .audio_io_layout
                    .main_input_channels
                    .map(NonZeroU32::get)
                    .unwrap_or(0) as usize;
                let period = self.config.period_size as usize;
                // A duplex ring holds whole frames, so a full one drops whole frames, and two
                // periods of them: the driver's period is fixed and both callbacks run once per
                // period, input first (A5).
                let capacity = if self.duplex {
                    ring_channels * period * 2
                } else {
                    (self.output.config.channels as usize).max(ring_channels) * period
                };
                let (rb_producer, rb_consumer) = RingBuffer::new(capacity);
                input_rb_consumer = Some(rb_consumer);

                let input_parker = Parker::new();
                let input_unparker = input_parker.unparker().clone();
                let error_cb = {
                    let input_unparker = input_unparker.clone();
                    let main_unparker = unparker.clone();
                    let stream_error = stream_error.clone();
                    move |err| {
                        nih_error!("Error during capture: {err:#}");
                        stream_error.store(true, Ordering::Release);
                        input_unparker.clone().unpark();
                        main_unparker.clone().unpark();
                    }
                };

                macro_rules! build_input_streams {
                    ($sample_format:expr, $(($format:path, $primitive_type:ty)),*) => {
                        match $sample_format {
                            $($format => input.device.build_input_stream(
                                &input.config,
                                self.build_input_data_callback::<$primitive_type>(input_unparker, rb_producer),
                                error_cb,
                                None,
                            ),)*
                            format => {
                                nih_error!("Unsupported sample format {format}");
                                Err(cpal::BuildStreamError::StreamConfigNotSupported)
                            }
                        }
                    }
                }
                // A capture stream that cannot be built or started must not take the output down
                // with it: the run goes on without audio input and the output callback hands the
                // plugin silence. A capture error mid-session still ends the run via `error_cb`.
                let built = build_input_streams!(
                    input.sample_format,
                    (SampleFormat::I8, i8),
                    (SampleFormat::I16, i16),
                    (SampleFormat::I32, i32),
                    (SampleFormat::I64, i64),
                    (SampleFormat::U8, u8),
                    (SampleFormat::U16, u16),
                    (SampleFormat::U32, u32),
                    (SampleFormat::U64, u64),
                    (SampleFormat::F32, f32),
                    (SampleFormat::F64, f64)
                );
                // cpal's ASIO output build holds the device's stream lock while it stops the
                // running driver and recreates its buffers, and a playing capture callback takes
                // that lock on the driver thread every period. The capture is therefore played
                // only once the output exists, right before the output itself.
                let stream = if self.duplex {
                    started_capture(
                        built,
                        |_: &Stream| Ok::<(), cpal::PlayStreamError>(()),
                        || false,
                    )
                } else {
                    started_capture(
                        built,
                        |stream: &Stream| stream.play(),
                        // Playback is delayed one period if we're capturing audio so it has
                        // something to process, and the timeout keeps a wedged capture device from
                        // blocking this thread forever. Until the output stream exists only the
                        // capture `error_cb` can raise `stream_error`, so a raised flag here is a
                        // refused start — and so is a capture that started without a word but
                        // delivered nothing, which would otherwise leave the output callback
                        // waiting on an empty ring.
                        || {
                            input_parker.park_timeout(Duration::from_secs(2));
                            let silent = || {
                                input_rb_consumer
                                    .as_ref()
                                    .map_or(true, |rb| rb.slots() == 0)
                            };
                            if silent() {
                                input_parker.park_timeout(Duration::from_millis(200));
                            }
                            stream_error.load(Ordering::Acquire) || silent()
                        },
                    )
                };
                if stream.is_none() {
                    // The capture stream is gone and can no longer raise the flag, so a start
                    // failure it reported must not end the run
                    stream_error.store(false, Ordering::Release);
                    input_rb_consumer = None;
                }
                _input_stream = stream;
            }

            // The requested input opened its configuration but not a stream: refused, watched.
            if self.input.is_some() && _input_stream.is_none() {
                let refused = self.in_use.lock().input.take();
                if let Some(name) = refused {
                    self.in_use.lock().refused_input = Some(name.clone());
                    self.watch.lock().refuse(
                        Wanted {
                            name,
                            kind: Kind::Input,
                        },
                        Instant::now(),
                    );
                    publish_audio_devices_in_use(self.in_use.lock().clone());
                }
            }

            // While another device stands in for a requested one, watch for it: a cheap name
            // listing every couple of seconds on its own thread, so the stream is never held up,
            // and a probe of the device once it is listed. The flag ends this run as
            // `DeviceReturned` and the wrapper reinitializes onto the device; a probe that fails
            // costs the stand-in stream nothing.
            let device_returned = Arc::new(AtomicBool::new(false));
            let watch_stop = Arc::new(AtomicBool::new(false));
            if !self.duplex && !self.watch.lock().is_idle() {
                let backend: &Self = self;
                let device_returned = device_returned.clone();
                let watch_stop = watch_stop.clone();
                let unparker = unparker.clone();
                s.spawn(move || {
                    while !sleep_unless(&watch_stop, DEVICE_POLL_INTERVAL) {
                        let Ok(host) = cpal::host_from_id(backend.host_id) else {
                            continue;
                        };
                        let due = backend
                            .watch
                            .lock()
                            .due(&device_names(&host), Instant::now());
                        let returned = due.iter().any(|wanted| {
                            let opens = backend.opens(&host, wanted);
                            if !opens {
                                backend.watch.lock().probe_failed(wanted, Instant::now());
                            }
                            opens
                        });
                        if returned {
                            device_returned.store(true, Ordering::Release);
                            unparker.unpark();
                            break;
                        }
                    }
                });
            }

            // The output callback can read input events from this ringbuffer
            let mut midi_input_rb_consumer: Option<rtrb::Consumer<PluginNoteEvent<P>>> = None;
            let midi_input_connection: Option<ActiveMidirInputDevice> =
                self.midi_input.lock().take().and_then(|midi_input| {
                    // Data is sent to the output data callback using a wait-free ring buffer
                    let (rb_producer, rb_consumer) = RingBuffer::new(MIDI_EVENT_QUEUE_CAPACITY);
                    midi_input_rb_consumer = Some(rb_consumer);

                    let result = midi_input.backend.connect(
                        &midi_input.port,
                        "MIDI input",
                        self.build_midi_input_thread::<P>(rb_producer),
                        (),
                    );

                    match result {
                        Ok(connection) => Some(ActiveMidirInputDevice {
                            connection,
                            port: midi_input.port,
                        }),
                        Err(err) => {
                            // We won't retry once this fails
                            nih_error!("Could not create the MIDI input connection: {err:#}");
                            midi_input_rb_consumer = None;

                            None
                        }
                    }
                });

            // The output callback can also emit MIDI events. To handle these we'll need to spawn
            // our own thread. This can be simplified a lot by using the `MidiOutputConnection`
            // directly inside the audio output callback, but looking at the implementation sending
            // MIDI events is not realtime safe in most midir backends.
            // NOTE: This uses crossbeam channels instead of rtrb specifically for the optional
            //        blocking API. This lets the MIDI sending thread sleep when there's no work to
            //        do.
            let mut midi_output_rb_producer: Option<crossbeam::channel::Sender<MidiOutputTask<P>>> =
                None;
            let midi_output_connection: Option<ScopedJoinHandle<ActiveMidirOutputDevice>> =
                self.midi_output.lock().take().and_then(|midi_output| {
                    // This uses crossbeam channels for the reason mentioned above, but to keep
                    // things cohesive we'll use the same naming scheme as we use for rtrb
                    let (sender, receiver) = crossbeam::channel::bounded(MIDI_EVENT_QUEUE_CAPACITY);
                    midi_output_rb_producer = Some(sender);

                    let result = midi_output
                        .backend
                        .connect(&midi_output.port, "MIDI output");

                    match result {
                        Ok(mut connection) => Some(s.spawn(move || {
                            while let Ok(task) = receiver.recv() {
                                match task {
                                    MidiOutputTask::Send(event) => match event.as_midi() {
                                        Some(MidiResult::Basic(midi_data)) => {
                                            if let Err(err) = connection.send(&midi_data) {
                                                nih_error!("Could not send MIDI event: {err}");
                                            }
                                        }
                                        Some(MidiResult::SysEx(padded_sysex_buffer, length)) => {
                                            // The SysEx buffer may contain padding
                                            let padded_sysex_buffer = padded_sysex_buffer.borrow();
                                            nih_debug_assert!(length <= padded_sysex_buffer.len());

                                            if let Err(err) =
                                                connection.send(&padded_sysex_buffer[..length])
                                            {
                                                nih_error!("Could not send MIDI event: {err}");
                                            }
                                        }
                                        None => (),
                                    },
                                    MidiOutputTask::Terminate => break,
                                }
                            }

                            // We'll return the same value from the join handle as what ends up
                            // being stored in `midi_input_connection` to keep this symmetrical with
                            // the input handling
                            ActiveMidirOutputDevice {
                                connection,
                                port: midi_output.port,
                            }
                        })),
                        Err(err) => {
                            nih_error!("Could not create the MIDI output connection: {err:#}");
                            midi_output_rb_producer = None;

                            None
                        }
                    }
                });

            let error_cb = {
                let unparker = unparker.clone();
                let stream_error = stream_error.clone();
                move |err| {
                    nih_error!("Error during playback: {err:#}");
                    stream_error.store(true, Ordering::Release);
                    unparker.clone().unpark();
                }
            };

            // Wrapping the caller's process callback lets us tell "the plugin asked to stop" apart
            // from "the stream died" without touching the data callback itself.
            let cb = {
                let callback_stopped = callback_stopped.clone();
                move |buffer: &mut Buffer,
                      aux: &mut AuxiliaryBuffers,
                      transport: Transport,
                      input_events: &[PluginNoteEvent<P>],
                      output_events: &mut Vec<PluginNoteEvent<P>>| {
                    let keep_running = cb(buffer, aux, transport, input_events, output_events);
                    if !keep_running {
                        callback_stopped.store(true, Ordering::Release);
                    }
                    keep_running
                }
            };

            macro_rules! build_output_streams {
                ($sample_format:expr, $(($format:path, $primitive_type:ty)),*) => {
                    match $sample_format {
                        $($format => self.output.device.build_output_stream(
                            &self.output.config,
                            self.build_output_data_callback::<P, $primitive_type>(
                                unparker,
                                input_rb_consumer,
                                midi_input_rb_consumer,
                                // This is a MPMC crossbeam channel instead of an rtrb ringbuffer, and we
                                // also need it to terminate the thread
                                midi_output_rb_producer.clone(),
                                cb,
                                liveness.clone(),
                            ),
                            error_cb,
                            None,
                        ),)*
                        format => {
                            nih_error!("Unsupported sample format {format}");
                            Err(cpal::BuildStreamError::StreamConfigNotSupported)
                        }
                    }
                }
            }
            // MIDI connections were already taken out of `self` above, so these failures must NOT
            // return early: fall through to the MIDI restore code below instead (an early return
            // would also deadlock the scoped MIDI output thread, which blocks on its channel until
            // it receives a `Terminate` task).
            let mut setup_failed = false;
            let output_stream = match build_output_streams!(
                self.output.sample_format,
                (SampleFormat::I8, i8),
                (SampleFormat::I16, i16),
                (SampleFormat::I32, i32),
                (SampleFormat::I64, i64),
                (SampleFormat::U8, u8),
                (SampleFormat::U16, u16),
                (SampleFormat::U32, u32),
                (SampleFormat::U64, u64),
                (SampleFormat::F32, f32),
                (SampleFormat::F64, f64)
            ) {
                Ok(stream) => {
                    if self.duplex {
                        if let Some(Err(err)) = _input_stream.as_ref().map(Stream::play) {
                            nih_error!("Could not start the capture stream: {err:#}");
                        }
                    }
                    // TODO: Wait a period before doing this when also reading the input
                    if let Err(err) = stream.play() {
                        nih_error!("Error trying to start the output stream: {err:#}");
                        setup_failed = true;
                    }
                    Some(stream)
                }
                Err(err) => {
                    nih_error!("Error creating the output stream: {err:#}");
                    setup_failed = true;
                    None
                }
            };

            // Wait for the audio thread to exit. The timeout also lets this thread notice a stop
            // request when the device is already dead and no further callbacks or stream errors
            // will arrive (previously that combination made the application hang on exit).
            if !setup_failed {
                // The first callback gets the whole timeout
                liveness.stamp();
                loop {
                    parker.park_timeout(Duration::from_millis(100));
                    if stream_error.load(Ordering::Acquire)
                        || callback_stopped.load(Ordering::Acquire)
                        || should_stop.load(Ordering::SeqCst)
                        || device_returned.load(Ordering::Acquire)
                    {
                        break;
                    }
                    if self.duplex && liveness.expired() {
                        nih_error!(
                            "No callback from the ASIO driver for {DUPLEX_LIVENESS_TIMEOUT_MS} \
                             ms, restarting the stream"
                        );
                        stream_error.store(true, Ordering::Release);
                        break;
                    }
                }
            }
            drop(output_stream);
            watch_stop.store(true, Ordering::Release);
            if self.duplex {
                let overflows = self.overflows.swap(0, Ordering::Relaxed);
                let underruns = self.underruns.swap(0, Ordering::Relaxed);
                if overflows + underruns > 0 {
                    nih_log!(
                        "Duplex ring: {overflows} input samples dropped, {underruns} output \
                         samples without input"
                    );
                }
            }

            // The Midir API requires us to take things out of Options and transform between these
            // structs
            *self.midi_input.lock() =
                midi_input_connection.map(|midi_input_connection| MidirInputDevice {
                    backend: midi_input_connection.connection.close().0,
                    port: midi_input_connection.port,
                });
            *self.midi_output.lock() =
                midi_output_connection.map(move |midi_output_connection_handle| {
                    // The thread needs to be terminated first
                    midi_output_rb_producer
                        .expect("Inconsistent internal MIDI output state")
                        .send(MidiOutputTask::Terminate)
                        .expect("Could not terminate the MIDI output thread");

                    let midi_output_connection = midi_output_connection_handle
                        .join()
                        .expect("MIDI output thread panicked");

                    MidirOutputDevice {
                        backend: midi_output_connection.connection.close(),
                        port: midi_output_connection.port,
                    }
                });

            // A stop requested by the process callback wins over a simultaneous stream error: the
            // plugin already decided to shut down, so the caller must not try to recover. Either
            // kind of stop also wins over a device that came back at the same moment.
            let callback_stopped = callback_stopped.load(Ordering::Acquire);
            if setup_failed || (stream_error.load(Ordering::Acquire) && !callback_stopped) {
                RunOutcome::StreamFailed
            } else if !callback_stopped
                && !should_stop.load(Ordering::SeqCst)
                && device_returned.load(Ordering::Acquire)
            {
                RunOutcome::DeviceReturned
            } else {
                RunOutcome::Stopped
            }
        })
    }

    fn reinit(&mut self) -> Result<()> {
        if self.duplex {
            // A7: the input shares the output's driver; drop it first so no second handle
            // outlives the restart. The output `CpalDevice` stays alive and keeps its driver
            // loaded: the kept host's enumeration then returns that driver and skips the others,
            // so a restart always lands on the same driver. Another driver needs a relaunch (A10).
            self.input = None;
        }

        // Unlike `new()` there is deliberately no native-sample-rate override here:
        // `self.config.sample_rate` is the rate the plugin and the wrapper were initialized with
        // and cannot change mid-session. For a default-device selection this picks up whatever
        // the *current* system default is.
        let listed = {
            let in_use = self.in_use.lock();
            (in_use.outputs.clone(), in_use.inputs.clone())
        };
        let device_use = self.device_use();
        let opened = Self::open_devices(
            &self.host,
            &mut self.config,
            &self.audio_io_layout,
            Start::Restart,
            &self.quarantined,
            device_use,
            listed,
        )?;
        self.quarantined.clear();
        self.output = opened.output;
        self.input = opened.input;
        let now = Instant::now();
        let mut watch = DeviceWatch::new(opened.absent, opened.refused, now);
        for wanted in &opened.quarantined {
            watch.quarantine(wanted, now);
        }
        *self.watch.lock() = watch;
        *self.in_use.lock() = opened.in_use;
        Ok(())
    }

    fn wait_for_device_change(&self, should_stop: &AtomicBool, max: Duration) {
        if self.duplex {
            // A7: listing loads every ASIO driver; wait instead and let `reinit()` try again.
            sleep_unless(should_stop, max);
            return;
        }
        let Ok(host) = cpal::host_from_id(self.host_id) else {
            sleep_unless(should_stop, max);
            return;
        };
        let before = device_names(&host);
        let deadline = Instant::now() + max;
        while Instant::now() < deadline {
            if sleep_unless(should_stop, DEVICE_POLL_INTERVAL) || device_names(&host) != before {
                return;
            }
        }
    }

    fn callback_thread_is_driver_owned(&self) -> bool {
        self.duplex
    }

    fn quarantine_requested(&mut self) -> bool {
        if self.duplex {
            // A restart can only land on the driver that is loaded, so there is nothing to stand
            // in for it: wait for the hardware instead.
            return false;
        }
        let in_use = self.in_use.lock().clone();
        let quarantined: Vec<Wanted> = in_use
            .output
            .map(|name| Wanted {
                name,
                kind: Kind::Output,
            })
            .into_iter()
            .chain(in_use.input.map(|name| Wanted {
                name,
                kind: Kind::Input,
            }))
            .collect();
        if quarantined.is_empty() {
            return false;
        }
        self.quarantined = quarantined;
        true
    }
}

fn main_channels(channels: Option<NonZeroU32>) -> usize {
    channels.map(NonZeroU32::get).unwrap_or_default() as usize
}

/// Every device the host lists, by name. A plain listing — no configuration queries — so it is
/// cheap enough to poll.
fn device_names(host: &cpal::Host) -> Vec<String> {
    host.devices()
        .map(|devices| devices.filter_map(|device| device.name().ok()).collect())
        .unwrap_or_default()
}

/// Device names as a picker wants them: no blanks, no duplicates, host order kept.
pub(crate) fn dedupe_names(names: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !name.trim().is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// The output and input lists to publish (spec A3). A duplex (ASIO) host is listed once, at
/// launch, before any driver is loaded; the input list IS the output list, since a driver is both
/// and a second listing would load every driver again. A restart carries `listed` (the launch
/// lists) over: the running driver stays loaded through it, and while one is loaded ASIO lists
/// only that driver. WASAPI and CoreAudio are never listed here: the default path never listed
/// devices, and a full listing costs seconds on CoreAudio.
fn device_lists(
    start: Start,
    duplex: bool,
    listed: (Vec<String>, Vec<String>),
    enumerate: impl FnOnce() -> Vec<String>,
) -> (Vec<String>, Vec<String>) {
    match start {
        Start::Restart => listed,
        Start::Launch if duplex => {
            let names = enumerate();
            (names.clone(), names)
        }
        Start::Launch => (Vec::new(), Vec::new()),
    }
}

fn names_of(devices: Option<impl Iterator<Item = Device>>) -> Vec<String> {
    dedupe_names(
        devices
            .into_iter()
            .flatten()
            .filter_map(|device| device.name().ok())
            .collect(),
    )
}

/// Whether this host runs input and output on one device and one driver thread (ASIO). Decides
/// the ring policy and the watch policy (spec A2, A5–A7).
fn single_device_duplex(host_id: cpal::HostId) -> bool {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        host_id == cpal::HostId::Asio
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        let _ = host_id;
        false
    }
}

/// How a start or restart treats the output device's own settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceUse {
    /// WASAPI and CoreAudio: the period as requested, and a restart refuses a device that runs at
    /// another rate rather than switch it under other applications.
    Shared,
    /// A single-device duplex driver (ASIO), which this application alone uses: the period
    /// follows the driver's buffer-size range, capped on a restart at the wrapper's
    /// `max_buffer_size`, and a restart sets the driver back to the session's rate (A4, A6).
    Duplex { period_cap: Option<u32> },
}

impl DeviceUse {
    fn is_duplex(self) -> bool {
        matches!(self, DeviceUse::Duplex { .. })
    }

    /// The period the stream asks the device for.
    fn period(self, requested: u32, range: cpal::SupportedBufferSize) -> u32 {
        match self {
            DeviceUse::Shared => requested,
            DeviceUse::Duplex { period_cap } => {
                let clamped = clamp_period(period_cap.unwrap_or(requested), range);
                period_cap.map_or(clamped, |cap| clamped.min(cap))
            }
        }
    }
}

/// The period a duplex driver is asked for: the request clamped into the driver's range. cpal
/// hands `BufferSize::Fixed` to ASIO checking only the maximum (A4).
pub(crate) fn clamp_period(requested: u32, range: cpal::SupportedBufferSize) -> u32 {
    match range {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => requested.clamp(min, max),
        _ => requested,
    }
}

/// How many input channels to open on a duplex driver: the fewest that cover the plugin, else the
/// most the driver has; `None` when the plugin wants none or the driver has none.
pub(crate) fn duplex_input_channels(wanted: u16, available: &[u16]) -> Option<u16> {
    if wanted == 0 {
        return None;
    }
    let covering = available.iter().copied().filter(|&c| c >= wanted).min();
    covering.or_else(|| available.iter().copied().max())
}

/// No error callback ever fires on ASIO (cpal ignores it), so silence is the signal (A6).
const DUPLEX_LIVENESS_TIMEOUT_MS: u64 = 2000;

pub(crate) fn liveness_expired(now_ms: u64, last_ms: u64) -> bool {
    now_ms.saturating_sub(last_ms) > DUPLEX_LIVENESS_TIMEOUT_MS
}

/// When the output callback last ran, in milliseconds since the run started.
#[derive(Clone)]
struct Liveness {
    run_started: Instant,
    last_callback: Arc<AtomicU64>,
}

impl Liveness {
    fn new() -> Self {
        Self {
            run_started: Instant::now(),
            last_callback: Arc::new(AtomicU64::new(0)),
        }
    }

    fn now_ms(&self) -> u64 {
        self.run_started.elapsed().as_millis() as u64
    }

    fn stamp(&self) {
        self.last_callback.store(self.now_ms(), Ordering::Relaxed);
    }

    fn expired(&self) -> bool {
        liveness_expired(self.now_ms(), self.last_callback.load(Ordering::Relaxed))
    }
}

/// Both duplex callbacks share the driver thread, so a full ring means the output has not drained
/// yet: drop and count, never spin (A5).
pub(crate) fn duplex_push(producer: &mut rtrb::Producer<f32>, sample: f32, overflows: &AtomicU64) {
    if producer.push(sample).is_err() {
        overflows.fetch_add(1, Ordering::Relaxed);
    }
}

/// An empty duplex ring on the output side is silence for this sample, counted (A5).
pub(crate) fn duplex_pop(consumer: &mut rtrb::Consumer<f32>, underruns: &AtomicU64) -> f32 {
    consumer.pop().unwrap_or_else(|_| {
        underruns.fetch_add(1, Ordering::Relaxed);
        0.0
    })
}

/// Discards all but the newest `keep` samples. The capture can run alone for a period between its
/// start and the output's, so the output's first period would otherwise play that backlog and
/// carry it as latency for the rest of the run.
pub(crate) fn duplex_discard_backlog(consumer: &mut rtrb::Consumer<f32>, keep: usize) {
    let stale = consumer.slots().saturating_sub(keep);
    if let Ok(chunk) = consumer.read_chunk(stale) {
        chunk.commit_all();
    }
}

impl CpalMidir {
    /// Initialize the backend with the specified host. Returns an error if this failed for whatever
    /// reason.
    pub fn new<P: Plugin>(config: WrapperConfig, cpal_host_id: cpal::HostId) -> Result<Self> {
        let audio_io_layout = config.audio_io_layout_or_exit::<P>();
        let host = cpal::host_from_id(cpal_host_id).context("The Audio API is unavailable")?;
        let duplex = single_device_duplex(cpal_host_id);

        if duplex {
            if config.input_device.is_some() {
                nih_log!("ASIO: '--input-device' is ignored, the driver carries both directions");
            }
        } else if config.input_device.is_none() && audio_io_layout.main_input_channels.is_some() {
            nih_log!(
                "Audio inputs are not connected automatically to prevent feedback. Use the \
                 '--input-device' option to choose an input device."
            )
        }

        if config.midi_input.is_none() && P::MIDI_INPUT >= MidiConfig::Basic {
            nih_log!("Use the '--midi-input' option to select a MIDI input device.")
        }
        if config.midi_output.is_none() && P::MIDI_OUTPUT >= MidiConfig::Basic {
            nih_log!("Use the '--midi-output' option to select a MIDI output device.")
        }

        let mut config = config;
        let requested_sample_rate = config.sample_rate;
        let opened = Self::open_devices(
            &host,
            &mut config,
            &audio_io_layout,
            Start::Launch,
            &[],
            if duplex {
                DeviceUse::Duplex { period_cap: None }
            } else {
                DeviceUse::Shared
            },
            Default::default(),
        )?;
        if (config.sample_rate - requested_sample_rate).abs() > 0.1 {
            nih_log!(
                "Device native sample rate is {} Hz, using that instead of requested {} Hz",
                config.sample_rate,
                requested_sample_rate
            );
        }

        // There's no obvious way to do sidechain inputs and additional outputs with the CPAL
        // backends like there is with JACK. So we'll just provide empty buffers instead.
        if !audio_io_layout.aux_input_ports.is_empty() {
            nih_warn!("Sidechain inputs are not supported with this audio backend");
        }
        if !audio_io_layout.aux_output_ports.is_empty() {
            nih_warn!("Auxiliary outputs are not supported with this audio backend");
        }

        let midi_input = match &config.midi_input {
            Some(midi_input_name) => {
                // Midir lets us preemptively ignore MIDI messages we'll never use like active
                // sensing and timing, but for maximum flexibility with NIH-plug's SysEx parsing
                // types (which could technically be used to also parse those things) we won't do
                // that.
                let midi_backend = MidiInput::new(P::NAME)
                    .context("Could not initialize the MIDI input backend")?;
                let available_ports = midi_backend.ports();

                // In case there somehow is a MIDI port with an empty name, we'll still want to
                // preserve the behavior of an empty argument resulting in a listing of options.
                let found_port = if !midi_input_name.is_empty() {
                    // This API is a bit weird
                    available_ports
                        .iter()
                        .find(|port| midi_backend.port_name(port).as_deref() == Ok(midi_input_name))
                } else {
                    None
                };

                match found_port {
                    Some(port) => Some(MidirInputDevice {
                        backend: midi_backend,
                        port: port.clone(),
                    }),
                    None => {
                        let mut message = format!(
                            "Unknown input MIDI device '{midi_input_name}'. Available devices are:"
                        );
                        for port in available_ports {
                            match midi_backend.port_name(&port) {
                                Ok(device_name) => message.push_str(&format!("\n{device_name}")),
                                Err(err) => message.push_str(&format!("\nERROR: {err:#}")),
                            }
                        }

                        anyhow::bail!(message);
                    }
                }
            }
            None => None,
        };

        let midi_output = match &config.midi_output {
            Some(midi_output_name) => {
                let midi_backend = MidiOutput::new(P::NAME)
                    .context("Could not initialize the MIDI output backend")?;
                let available_ports = midi_backend.ports();

                let found_port = if !midi_output_name.is_empty() {
                    available_ports.iter().find(|port| {
                        midi_backend.port_name(port).as_deref() == Ok(midi_output_name)
                    })
                } else {
                    None
                };

                match found_port {
                    Some(port) => Some(MidirOutputDevice {
                        backend: midi_backend,
                        port: port.clone(),
                    }),
                    None => {
                        let mut message = format!(
                            "Unknown output MIDI device '{midi_output_name}'. Available devices \
                             are:"
                        );
                        for port in available_ports {
                            match midi_backend.port_name(&port) {
                                Ok(device_name) => message.push_str(&format!("\n{device_name}")),
                                Err(err) => message.push_str(&format!("\nERROR: {err:#}")),
                            }
                        }

                        anyhow::bail!(message);
                    }
                }
            }
            None => None,
        };

        let launch_period = config.period_size;
        Ok(CpalMidir {
            config,
            audio_io_layout,
            host_id: cpal_host_id,
            host,
            duplex,
            launch_period,
            overflows: Arc::new(AtomicU64::new(0)),
            underruns: Arc::new(AtomicU64::new(0)),

            input: opened.input,
            output: opened.output,

            midi_input: Mutex::new(midi_input),
            midi_output: Mutex::new(midi_output),

            watch: Mutex::new(DeviceWatch::new(
                opened.absent,
                opened.refused,
                Instant::now(),
            )),
            in_use: Mutex::new(opened.in_use),
            quarantined: Vec::new(),
        })
    }

    /// Returns the actual sample rate being used, which may differ from the requested rate
    /// if the device's native rate was used instead.
    pub fn actual_sample_rate(&self) -> f32 {
        self.config.sample_rate
    }

    /// The period the stream was opened with; a duplex driver may have clamped it.
    pub fn actual_period_size(&self) -> u32 {
        self.config.period_size
    }

    /// How a restart opens the output: a duplex driver follows its range up to the period the
    /// wrapper was initialized with.
    fn device_use(&self) -> DeviceUse {
        if self.duplex {
            DeviceUse::Duplex {
                period_cap: Some(self.launch_period),
            }
        } else {
            DeviceUse::Shared
        }
    }

    /// Whether a wanted device could run the session's stream right now: the restart's own
    /// resolution, then a silent stream built and started on that device and dropped as soon as
    /// it delivers a callback. A device can match every configuration and still refuse a stream
    /// (held exclusively by another application, not ready yet after a replug, a driver without
    /// a clock), and only a stream shows it. Nothing here touches the stream that is playing.
    fn opens(&self, host: &cpal::Host, wanted: &Wanted) -> bool {
        let resolved = match wanted.kind {
            Kind::Output => Self::resolve_output(
                host,
                &wanted.name,
                &self.config,
                main_channels(self.audio_io_layout.main_output_channels),
                Start::Restart,
                self.device_use(),
            ),
            Kind::Input => Self::resolve_input(
                host,
                &wanted.name,
                &self.config,
                main_channels(self.audio_io_layout.main_input_channels),
                Start::Restart,
            ),
        };
        match resolved {
            Resolved::Open(opened) => Self::stream_starts(&opened, wanted.kind),
            Resolved::Refused(_) | Resolved::Absent => false,
        }
    }

    fn stream_starts(opened: &CpalDevice, kind: Kind) -> bool {
        let called = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let error_cb = {
            let failed = failed.clone();
            move |_: cpal::StreamError| failed.store(true, Ordering::Release)
        };
        let stream = match kind {
            Kind::Output => {
                let called = called.clone();
                opened.device.build_output_stream_raw(
                    &opened.config,
                    opened.sample_format,
                    move |data: &mut cpal::Data, _: &cpal::OutputCallbackInfo| {
                        data.bytes_mut().fill(0);
                        called.store(true, Ordering::Release);
                    },
                    error_cb,
                    None,
                )
            }
            Kind::Input => {
                let called = called.clone();
                opened.device.build_input_stream_raw(
                    &opened.config,
                    opened.sample_format,
                    move |_: &cpal::Data, _: &cpal::InputCallbackInfo| {
                        called.store(true, Ordering::Release);
                    },
                    error_cb,
                    None,
                )
            }
        };
        let Ok(stream) = stream else {
            return false;
        };
        if stream.play().is_err() {
            return false;
        }
        let deadline = Instant::now() + PROBE_FIRST_CALLBACK;
        while Instant::now() < deadline {
            if called.load(Ordering::Acquire) {
                return true;
            }
            if failed.load(Ordering::Acquire) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Open the requested devices the way both a start and a restart do. A requested output that
    /// is not connected, or is connected but cannot run the stream, is stood in for by the system
    /// default; a requested input in either state leaves the run without capture (the output
    /// callback hands the plugin silence). Both are logged, kept as wanted and watched for. A
    /// device in `quarantined` — its stream kept dying — is refused without an attempt. Only "no
    /// output device at all" is an error.
    ///
    /// A start takes the opened output's native sample rate into `config`, so CoreAudio and WASAPI
    /// do no internal conversion; a restart keeps the session's rate, which the plugin cannot
    /// change, and never switches a requested device's own rate to reach it. What the stream is
    /// open on is published either way, so the host application's status is true even while
    /// nothing could be opened.
    ///
    /// A duplex driver carries both directions: its period is adopted into `config` (A4) and the
    /// input, when the plugin has one and the driver offers input channels, is the output's own
    /// device (A2); `--input-device` plays no part.
    fn open_devices(
        host: &cpal::Host,
        config: &mut WrapperConfig,
        layout: &AudioIOLayout,
        start: Start,
        quarantined: &[Wanted],
        device_use: DeviceUse,
        listed: (Vec<String>, Vec<String>),
    ) -> Result<OpenedDevices> {
        let duplex = device_use.is_duplex();
        let (outputs, inputs) = device_lists(start, duplex, listed, || {
            names_of(host.output_devices().ok())
        });
        let mut in_use = AudioDevicesInUse {
            outputs,
            inputs,
            ..Default::default()
        };
        let mut opened = OpenedDevicesSoFar::default();

        let output_channels = main_channels(layout.main_output_channels);
        let mut output = None;
        if let Some(name) = config.output_device.clone() {
            let wanted = Wanted {
                name: name.clone(),
                kind: Kind::Output,
            };
            let resolved = if quarantined.contains(&wanted) {
                Resolved::Refused(anyhow::anyhow!("the stream on it keeps dying"))
            } else {
                Self::resolve_output(host, &name, config, output_channels, start, device_use)
            };
            match resolved {
                Resolved::Open(device) => {
                    in_use.output = Some(name);
                    output = Some(device);
                }
                Resolved::Refused(err) => {
                    nih_error!(
                        "Output device '{name}' cannot open the audio stream, playing through the \
                         system default until it can: {err:#}"
                    );
                    in_use.refused_output = Some(name);
                    opened.refused(wanted, quarantined);
                }
                Resolved::Absent => {
                    nih_log!(
                        "Output device '{name}' is not connected, playing through the system \
                         default until it is back{}",
                        start.available(host)
                    );
                    opened.absent.push(wanted);
                }
            }
        }
        let output = match output {
            Some(output) => output,
            None => {
                let result = host
                    .default_output_device()
                    .context("No default audio output device available")
                    .and_then(|device| {
                        Self::open_output(device, config, output_channels, start, device_use)
                    });
                match result {
                    Ok(output) => output,
                    Err(err) => {
                        publish_audio_devices_in_use(in_use);
                        return Err(err);
                    }
                }
            }
        };
        if start == Start::Launch {
            config.sample_rate = output.config.sample_rate.0 as f32;
        }
        if let (true, cpal::BufferSize::Fixed(period)) = (duplex, output.config.buffer_size) {
            if period != config.period_size {
                nih_log!(
                    "A period of {} samples is outside the driver's range, using {period}",
                    config.period_size
                );
                config.period_size = period;
            }
        }

        let mut input = None;
        if duplex {
            let wanted = main_channels(layout.main_input_channels) as u16;
            let rate = output.config.sample_rate;
            let configs: Vec<_> = output
                .device
                .supported_input_configs()
                .map(|configs| {
                    configs
                        .filter(|c| c.min_sample_rate() <= rate && rate <= c.max_sample_rate())
                        .collect()
                })
                .unwrap_or_default();
            let available: Vec<u16> = configs.iter().map(|c| c.channels()).collect();
            match duplex_input_channels(wanted, &available) {
                Some(channels) => {
                    let sample_format = configs
                        .iter()
                        .find(|c| c.channels() == channels)
                        .map_or(output.sample_format, |c| c.sample_format());
                    input = Some(CpalDevice {
                        device: output.device.clone(),
                        config: StreamConfig {
                            channels,
                            sample_rate: rate,
                            buffer_size: output.config.buffer_size,
                        },
                        sample_format,
                    });
                    in_use.input = in_use.output.clone().or_else(|| output.device.name().ok());
                }
                None if wanted > 0 => {
                    nih_log!("The ASIO driver offers no input channels, audio input is off")
                }
                None => {}
            }
        } else if let Some(name) = config.input_device.clone() {
            let input_channels = main_channels(layout.main_input_channels);
            let wanted = Wanted {
                name: name.clone(),
                kind: Kind::Input,
            };
            let resolved = if quarantined.contains(&wanted) {
                Resolved::Refused(anyhow::anyhow!("the stream on it keeps dying"))
            } else {
                Self::resolve_input(host, &name, config, input_channels, start)
            };
            match resolved {
                Resolved::Open(device) => {
                    in_use.input = Some(name);
                    input = Some(device);
                }
                Resolved::Refused(err) => {
                    nih_error!(
                        "Input device '{name}' cannot open the audio stream, audio input is off \
                         until it can: {err:#}"
                    );
                    in_use.refused_input = Some(name);
                    opened.refused(wanted, quarantined);
                }
                Resolved::Absent => {
                    nih_log!(
                        "Input device '{name}' is not connected, audio input is off until it is \
                         back{}",
                        start.available(host)
                    );
                    opened.absent.push(wanted);
                }
            }
        }

        publish_audio_devices_in_use(in_use.clone());
        Ok(OpenedDevices {
            output,
            input,
            in_use,
            absent: opened.absent,
            refused: opened.refused,
            quarantined: opened.quarantined,
        })
    }

    /// Every device the host lists under `name`. CoreAudio lists an interface once, for both
    /// directions; WASAPI lists each endpoint, and two of them may share a name. A plain listing:
    /// no configuration queries on the devices that are not asked for.
    fn devices_named(host: &cpal::Host, name: &str) -> Vec<Device> {
        host.devices()
            .map(|devices| {
                devices
                    .filter(|d| d.name().as_deref().map(|n| n == name).unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The first candidate that `open` accepts; refused when there are candidates but none opens,
    /// absent when there are none.
    fn resolve(candidates: Vec<Device>, open: impl Fn(Device) -> Result<CpalDevice>) -> Resolved {
        let mut refused = None;
        for device in candidates {
            match open(device) {
                Ok(opened) => return Resolved::Open(opened),
                Err(err) => refused = Some(err),
            }
        }
        match refused {
            Some(err) => Resolved::Refused(err),
            None => Resolved::Absent,
        }
    }

    fn resolve_output(
        host: &cpal::Host,
        name: &str,
        config: &WrapperConfig,
        num_output_channels: usize,
        start: Start,
        device_use: DeviceUse,
    ) -> Resolved {
        let candidates = Self::devices_named(host, name)
            .into_iter()
            .filter(|device| {
                device
                    .supported_output_configs()
                    .map(|mut configs| configs.next().is_some())
                    .unwrap_or(false)
            })
            .collect();
        Self::resolve(candidates, |device| {
            if start == Start::Restart && device_use == DeviceUse::Shared {
                let current = device.default_output_config().ok().map(|c| c.sample_rate());
                Self::runs_at_session_rate(current, config)?;
            }
            Self::open_output(device, config, num_output_channels, start, device_use)
        })
    }

    fn resolve_input(
        host: &cpal::Host,
        name: &str,
        config: &WrapperConfig,
        num_input_channels: usize,
        start: Start,
    ) -> Resolved {
        let candidates = Self::devices_named(host, name)
            .into_iter()
            .filter(|device| {
                device
                    .supported_input_configs()
                    .map(|mut configs| configs.next().is_some())
                    .unwrap_or(false)
            })
            .collect();
        Self::resolve(candidates, |device| {
            if start == Start::Restart {
                let current = device.default_input_config().ok().map(|c| c.sample_rate());
                Self::runs_at_session_rate(current, config)?;
            }
            Self::build_input_cpal_device(device, config, num_input_channels)
        })
    }

    /// A requested device that comes back mid-session has to run at the session's rate already.
    /// Opening it at another rate would switch the device — system-wide on CoreAudio — under any
    /// other application using it, so it is refused instead, with the reason.
    fn runs_at_session_rate(
        current: Option<cpal::SampleRate>,
        config: &WrapperConfig,
    ) -> Result<()> {
        if let Some(rate) = current {
            if (rate.0 as f32 - config.sample_rate).abs() > 0.1 {
                anyhow::bail!(
                    "the device runs at {} Hz while this session runs at {} Hz; a device is never \
                     switched to another rate under other applications, so set it to {} Hz or \
                     restart to adopt its rate",
                    rate.0,
                    config.sample_rate,
                    config.sample_rate
                );
            }
        }
        Ok(())
    }

    /// The output configuration `device` runs the stream with: at its own native rate on a start
    /// (the rate the session then adopts), at the session's rate on a restart. A duplex driver's
    /// period follows its buffer-size range (`DeviceUse::period`).
    fn open_output(
        device: Device,
        config: &WrapperConfig,
        num_output_channels: usize,
        start: Start,
        device_use: DeviceUse,
    ) -> Result<CpalDevice> {
        let current = match (start, device_use) {
            (Start::Restart, DeviceUse::Shared) => None,
            _ => device.default_output_config().ok(),
        };
        let mut config = config.clone();
        if let Some(current) = &current {
            let driver_rate = current.sample_rate().0 as f32;
            match start {
                Start::Launch => config.sample_rate = driver_rate,
                Start::Restart if (driver_rate - config.sample_rate).abs() > 0.1 => nih_log!(
                    "ASIO driver rate changed to {driver_rate} Hz; restarting at the session rate \
                     of {} Hz, the new rate applies at the next launch",
                    config.sample_rate
                ),
                Start::Restart => {}
            }
            if device_use.is_duplex() {
                let range = *current.buffer_size();
                let period = device_use.period(config.period_size, range);
                if period < clamp_period(period, range) {
                    nih_log!(
                        "The driver's smallest buffer is now larger than this session's {period} \
                         samples; relaunch to use it"
                    );
                }
                config.period_size = period;
            }
        }
        Self::build_output_cpal_device(device, &config, num_output_channels)
    }

    fn build_input_cpal_device(
        device: Device,
        config: &WrapperConfig,
        num_input_channels: usize,
    ) -> Result<CpalDevice> {
        if num_input_channels == 0 {
            anyhow::bail!("The plugin has no main audio input to connect an input device to");
        }
        let requested_sample_rate = cpal::SampleRate(config.sample_rate as u32);
        let requested_buffer_size = cpal::BufferSize::Fixed(config.period_size);

        // Any device with an input channel is accepted: its first channels feed the main input in
        // order and a mono device feeds all of them (`input_source_channel()`). CoreAudio reports
        // only a device's total channel count, so an exact match would refuse every mono and every
        // multichannel interface.
        let mut input_configs: Vec<_> = device
            .supported_input_configs()
            .context("Could not get supported audio input configurations")?
            .filter(|c| match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    c.channels() >= 1
                        && (c.min_sample_rate()..=c.max_sample_rate())
                            .contains(&requested_sample_rate)
                        && (min..=max).contains(&&config.period_size)
                }
                cpal::SupportedBufferSize::Unknown => false,
            })
            .collect();
        input_configs.sort_by_key(|c| {
            input_config_rank(
                c.channels() as usize,
                num_input_channels,
                c.sample_format() == SampleFormat::F32,
            )
        });
        let input_config_range = input_configs.first().cloned().with_context(|| {
            format!(
                "The audio input device does not support a sample rate of {} Hz and a period size \
                 of {} samples",
                config.sample_rate, config.period_size,
            )
        })?;

        // We already checked that these settings are valid
        let input_config = StreamConfig {
            channels: input_config_range.channels(),
            sample_rate: requested_sample_rate,
            buffer_size: requested_buffer_size,
        };
        let input_sample_format = input_config_range.sample_format();

        Ok(CpalDevice {
            device,
            config: input_config,
            sample_format: input_sample_format,
        })
    }

    fn build_output_cpal_device(
        device: Device,
        config: &WrapperConfig,
        num_output_channels: usize,
    ) -> Result<CpalDevice> {
        let requested_sample_rate = cpal::SampleRate(config.sample_rate as u32);
        let requested_buffer_size = cpal::BufferSize::Fixed(config.period_size);

        let mut output_configs: Vec<_> = device
            .supported_output_configs()
            .context("Could not get supported audio output configurations")?
            .filter(|c| match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    // Accept devices with more channels than needed (e.g. multichannel
                    // interfaces like UAD Apollo). We'll write to the first N channels and
                    // silence the rest in the output callback.
                    c.channels() as usize >= num_output_channels
                        && (c.min_sample_rate()..=c.max_sample_rate())
                            .contains(&requested_sample_rate)
                        && (min..=max).contains(&&config.period_size)
                }
                cpal::SupportedBufferSize::Unknown => false,
            })
            .collect();
        // Prefer F32 with the fewest channels to minimize wasted bandwidth
        output_configs.sort_by_key(|c| c.channels());
        let output_config_range = output_configs
            .iter()
            .find(|c| c.sample_format() == SampleFormat::F32)
            .or_else(|| output_configs.first())
            .cloned()
            .with_context(|| {
                format!(
                    "The audio output device does not support at least {} audio channels at a \
                     sample rate of {} Hz and a period size of {} samples",
                    num_output_channels, config.sample_rate, config.period_size,
                )
            })?;
        let device_channels = output_config_range.channels();
        if device_channels as usize > num_output_channels {
            nih_log!(
                "Output device has {} channels, plugin needs {}. Writing to first {} channels.",
                device_channels,
                num_output_channels,
                num_output_channels,
            );
        }
        let output_config = StreamConfig {
            channels: device_channels,
            sample_rate: requested_sample_rate,
            buffer_size: requested_buffer_size,
        };
        let output_sample_format = output_config_range.sample_format();

        Ok(CpalDevice {
            device,
            config: output_config,
            sample_format: output_sample_format,
        })
    }

    fn build_input_data_callback<T>(
        &self,
        input_unparker: Unparker,
        mut input_rb_producer: rtrb::Producer<f32>,
    ) -> impl FnMut(&[T], &InputCallbackInfo) + Send + 'static
    where
        T: Sample,
        // The CPAL update made the whole interface more complicated by switching to dasp's sample
        // trait, and then they also forgot to expose the `ToSample` trait so now you need to do
        // this
        f32: FromSample<T>,
    {
        // This callback needs to copy input samples to a ring buffer that can be read from in the
        // output data callback
        let duplex = self.duplex;
        #[cfg(target_os = "windows")]
        let mut input_promotion_pending =
            super::super::wrapper::callback_thread_needs_promotion(duplex);
        let overflows = self.overflows.clone();
        let device_channels = self
            .input
            .as_ref()
            .map_or(1, |input| input.config.channels as usize);
        let plugin_channels = self
            .audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        move |data, _info| {
            // The promoted output callback busy-spins on this thread's ring
            // (see the pop loop in the output callback) — leaving the capture
            // thread at cpal's silently-broken NORMAL priority would be a
            // priority inversion: a real-time spinner starved of the very
            // samples it waits for.
            // A duplex capture runs on the driver's own thread, which nothing spins on.
            #[cfg(target_os = "windows")]
            if input_promotion_pending {
                input_promotion_pending = false;
                super::super::wrapper::promote_audio_thread();
            }

            fold_input_frames(data, device_channels, plugin_channels, |sample| {
                if duplex {
                    duplex_push(&mut input_rb_producer, sample, &overflows);
                } else {
                    // If for whatever reason the input callback is fired twice before an output
                    // callback, then just spin on this until the push succeeds
                    while input_rb_producer.push(sample).is_err() {}
                }
            });

            // The run function is blocked until a single period has been processed here. After this
            // point output playback can start.
            input_unparker.unpark();
        }
    }

    fn build_midi_input_thread<P: Plugin>(
        &self,
        mut midi_input_rb_producer: rtrb::Producer<PluginNoteEvent<P>>,
    ) -> impl FnMut(u64, &[u8], &mut ()) + Send + 'static {
        // This callback parses the received MIDI bytes and sends them to a ring buffer
        move |_timing, midi_data, _data| {
            // Since this is system MIDI there's no real useful timing information and we'll set all
            // the timings to the first sample in the buffer
            if let Ok(event) = NoteEvent::from_midi(0, midi_data) {
                if midi_input_rb_producer.push(event).is_err() {
                    nih_error!("The MIDI input event queue was full, dropping event");
                }
            }
        }
    }

    fn build_output_data_callback<P, T>(
        &self,
        unparker: Unparker,
        mut input_rb_consumer: Option<rtrb::Consumer<f32>>,
        mut input_event_rb_consumer: Option<rtrb::Consumer<PluginNoteEvent<P>>>,
        mut output_event_rb_producer: Option<crossbeam::channel::Sender<MidiOutputTask<P>>>,
        mut cb: impl FnMut(
                &mut Buffer,
                &mut AuxiliaryBuffers,
                Transport,
                &[PluginNoteEvent<P>],
                &mut Vec<PluginNoteEvent<P>>,
            ) -> bool
            + 'static
            + Send,
        liveness: Liveness,
    ) -> impl FnMut(&mut [T], &OutputCallbackInfo) + Send + 'static
    where
        P: Plugin,
        T: Sample + FromSample<f32>,
    {
        // We'll receive interlaced input samples from CPAL. These need to converted to deinterlaced
        // channels, processed, and then copied those back to an interlaced buffer for the output.
        let buffer_size = self.config.period_size as usize;
        let num_output_channels = self
            .audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        let num_input_channels = self
            .audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        // The device may have more channels than the plugin needs (e.g. multichannel interfaces)
        let device_output_channels = self.output.config.channels as usize;
        // This may contain excess unused space at the end if we get fewer samples than configured
        // from CPAL
        let mut main_io_storage = vec![vec![0.0f32; buffer_size]; num_output_channels];

        // This backend does not support auxiliary inputs and outputs, so in order to have the same
        // behavior as the other backends we'll provide some dummy buffers that we'll zero out every
        // time
        let mut aux_input_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        for channel_count in self.audio_io_layout.aux_input_ports {
            aux_input_storage.push(vec![
                vec![0.0f32; buffer_size];
                channel_count.get() as usize
            ]);
        }

        let mut aux_output_storage: Vec<Vec<Vec<f32>>> = Vec::new();
        for channel_count in self.audio_io_layout.aux_output_ports {
            aux_output_storage.push(vec![
                vec![0.0f32; buffer_size];
                channel_count.get() as usize
            ]);
        }

        // The actual buffer management here works the same as in the JACK backend. See that
        // implementation for more information.
        let mut buffer_manager =
            BufferManager::for_audio_io_layout(buffer_size, self.audio_io_layout);
        let mut main_io_channel_pointers = ChannelPointerVec(Vec::with_capacity(
            self.audio_io_layout
                .main_output_channels
                .map(NonZeroU32::get)
                .unwrap_or(0) as usize,
        ));
        let mut aux_input_channel_pointers =
            Vec::with_capacity(self.audio_io_layout.aux_input_ports.len());
        for channel_count in self.audio_io_layout.aux_input_ports {
            aux_input_channel_pointers.push(ChannelPointerVec(Vec::with_capacity(
                channel_count.get() as usize,
            )));
        }
        let mut aux_output_channel_pointers =
            Vec::with_capacity(self.audio_io_layout.aux_output_ports.len());
        for channel_count in self.audio_io_layout.aux_output_ports {
            aux_output_channel_pointers.push(ChannelPointerVec(Vec::with_capacity(
                channel_count.get() as usize,
            )));
        }

        let mut midi_input_events = Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY);
        let mut midi_output_events = Vec::with_capacity(MIDI_EVENT_QUEUE_CAPACITY);

        // Can't borrow from `self` in the callback
        let config = self.config.clone();
        let mut num_processed_samples = 0usize;
        let duplex = self.duplex;
        let overflows = self.overflows.clone();
        let underruns = self.underruns.clone();
        let mut backlog_discarded = false;
        move |data, _info| {
            liveness.stamp();
            if duplex && !backlog_discarded {
                backlog_discarded = true;
                // The capture callback runs first in every driver period, so from here on the
                // ring holds exactly the period being processed. What it dropped before the
                // output started is not an xrun.
                if let Some(input_rb_consumer) = &mut input_rb_consumer {
                    let frames = data.len() / device_output_channels.max(1);
                    duplex_discard_backlog(input_rb_consumer, frames * num_input_channels);
                }
                overflows.store(0, Ordering::Relaxed);
            }

            // CoreAudio and other backends may deliver more samples per callback than the
            // configured period size. On macOS this reliably happens right after the default
            // output device changes to a device with a different sample rate or a larger IO
            // buffer (the DefaultOutput audio unit follows the system default device and its
            // resampler then pulls `period_size * old_rate / new_rate` samples). Instead of
            // panicking, process the data in chunks of at most `buffer_size` samples.
            let total_sample_count = data.len() / device_output_channels;
            let mut chunk_start = 0usize;
            while chunk_start < total_sample_count {
                let chunk_size = (total_sample_count - chunk_start).min(buffer_size);

                let mut transport = Transport::new(config.sample_rate);
                transport.pos_samples = Some(num_processed_samples as i64);
                transport.tempo = Some(config.tempo as f64);
                transport.time_sig_numerator = Some(config.timesig_num as i32);
                transport.time_sig_denominator = Some(config.timesig_denom as i32);
                transport.playing = true;

                // If an input was configured, then the output buffer is filled with (interleaved) input
                // samples. Otherwise it gets filled with silence. There is no need to zero out any of
                // the other buffers. The `BufferManager` will copy the auxiliary input data to its own
                // storage buffers because it cannot assume that these buffers are safe to write to.
                // Because of that we'll never need to reinitialize these, and the output storage is
                // write-only (with `BufferManager` always zeroing them out when creating the buffers).
                match &mut input_rb_consumer {
                    Some(input_rb_consumer) if duplex => {
                        deinterleave_frames(
                            || duplex_pop(input_rb_consumer, &underruns),
                            &mut main_io_storage,
                            num_input_channels,
                            chunk_size,
                        );
                    }
                    Some(input_rb_consumer) => {
                        // Keep spinning on this if the output callback somehow outpaces the input
                        // callback
                        deinterleave_frames(
                            || loop {
                                if let Ok(input_sample) = input_rb_consumer.pop() {
                                    break input_sample;
                                }
                            },
                            &mut main_io_storage,
                            num_input_channels,
                            chunk_size,
                        );
                    }
                    None => {
                        for channel in main_io_storage.iter_mut() {
                            channel[..chunk_size].fill(0.0);
                        }
                    }
                }

                // Things may have been moved in between callbacks, so these pointers need to be set up
                // again on each invocation
                main_io_channel_pointers.get().clear();
                for channel in main_io_storage.iter_mut() {
                    assert!(channel.len() == buffer_size);

                    main_io_channel_pointers.get().push(channel.as_mut_ptr());
                }

                for (input_channel_pointers, input_storage) in aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_storage.iter_mut())
                {
                    input_channel_pointers.get().clear();
                    for channel in input_storage.iter_mut() {
                        assert!(channel.len() == buffer_size);

                        input_channel_pointers.get().push(channel.as_mut_ptr());
                    }
                }

                for (output_channel_pointers, output_storage) in aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_storage.iter_mut())
                {
                    output_channel_pointers.get().clear();
                    for channel in output_storage.iter_mut() {
                        assert!(channel.len() == buffer_size);

                        output_channel_pointers.get().push(channel.as_mut_ptr());
                    }
                }

                {
                    let buffers = unsafe {
                        buffer_manager.create_buffers(0, chunk_size, |buffer_sources| {
                            *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                                ptrs: NonNull::new(main_io_channel_pointers.get().as_mut_ptr())
                                    .unwrap(),
                                num_channels: main_io_channel_pointers.get().len(),
                            });
                            *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                                ptrs: NonNull::new(main_io_channel_pointers.get().as_mut_ptr())
                                    .unwrap(),
                                num_channels: num_input_channels
                                    .min(main_io_channel_pointers.get().len()),
                            });

                            for (input_source_channel_pointers, input_channel_pointers) in
                                buffer_sources
                                    .aux_input_channel_pointers
                                    .iter_mut()
                                    .zip(aux_input_channel_pointers.iter_mut())
                            {
                                *input_source_channel_pointers = Some(ChannelPointers {
                                    ptrs: NonNull::new(input_channel_pointers.get().as_mut_ptr())
                                        .unwrap(),
                                    num_channels: input_channel_pointers.get().len(),
                                });
                            }

                            for (output_source_channel_pointers, output_channel_pointers) in
                                buffer_sources
                                    .aux_output_channel_pointers
                                    .iter_mut()
                                    .zip(aux_output_channel_pointers.iter_mut())
                            {
                                *output_source_channel_pointers = Some(ChannelPointers {
                                    ptrs: NonNull::new(output_channel_pointers.get().as_mut_ptr())
                                        .unwrap(),
                                    num_channels: output_channel_pointers.get().len(),
                                });
                            }
                        })
                    };

                    midi_input_events.clear();
                    if let Some(input_event_rb_consumer) = &mut input_event_rb_consumer {
                        if let Ok(event) = input_event_rb_consumer.pop() {
                            midi_input_events.push(event);
                        }
                    }

                    midi_output_events.clear();
                    let mut aux = AuxiliaryBuffers {
                        inputs: buffers.aux_inputs,
                        outputs: buffers.aux_outputs,
                    };
                    if !cb(
                        buffers.main_buffer,
                        &mut aux,
                        transport,
                        &midi_input_events,
                        &mut midi_output_events,
                    ) {
                        // TODO: Some way to immediately terminate the stream here would be nice
                        unparker.unpark();
                        return;
                    }
                }

                // The buffer's samples need to be written to `data` in an interlaced format.
                // When the device has more channels than the plugin, write plugin output to the
                // first channels and silence the rest.
                // SAFETY: Dropping `buffers` allows us to borrow `main_io_storage` again
                let chunk_data = &mut data[chunk_start * device_output_channels
                    ..(chunk_start + chunk_size) * device_output_channels];
                for (i, output_sample) in chunk_data.iter_mut().enumerate() {
                    let ch = i % device_output_channels;
                    let n = i / device_output_channels;
                    if ch < num_output_channels {
                        *output_sample = T::from_sample(main_io_storage[ch][n]);
                    } else {
                        *output_sample = T::from_sample(0.0f32);
                    }
                }

                if let Some(output_event_rb_producer) = &mut output_event_rb_producer {
                    for event in midi_output_events.drain(..) {
                        if output_event_rb_producer
                            .try_send(MidiOutputTask::Send(event))
                            .is_err()
                        {
                            nih_error!("The MIDI output event queue was full, dropping event");
                            break;
                        }
                    }
                }

                num_processed_samples += chunk_size;
                chunk_start += chunk_size;
            }
        }
    }
}

/// The device channel that feeds `plugin_channel` of the plugin's main input: the same-numbered
/// channel when the device has it, the only channel of a mono device for every plugin channel,
/// and none (silence) for plugin channels a multichannel device does not have.
fn input_source_channel(plugin_channel: usize, device_channels: usize) -> Option<usize> {
    if device_channels == 1 {
        Some(0)
    } else if plugin_channel < device_channels {
        Some(plugin_channel)
    } else {
        None
    }
}

/// Converts one capture callback's interleaved device frames into interleaved frames of the
/// plugin's main input channel count, handing each sample to `push` in order.
fn fold_input_frames<T>(
    data: &[T],
    device_channels: usize,
    plugin_channels: usize,
    mut push: impl FnMut(f32),
) where
    T: Sample,
    f32: FromSample<T>,
{
    for frame in data.chunks_exact(device_channels.max(1)) {
        for plugin_channel in 0..plugin_channels {
            push(
                match input_source_channel(plugin_channel, device_channels) {
                    Some(source) => frame[source].to_sample::<f32>(),
                    None => 0.0,
                },
            );
        }
    }
}

/// Fills channel-major `storage` from `frames` interleaved frames of `ring_channels` samples
/// each, pulled in order from `next_sample`. Storage channels past the ring's are silenced; ring
/// channels without a storage channel are still consumed so the ring stays frame-aligned.
fn deinterleave_frames(
    mut next_sample: impl FnMut() -> f32,
    storage: &mut [Vec<f32>],
    ring_channels: usize,
    frames: usize,
) {
    for frame in 0..frames {
        for channel in 0..ring_channels {
            let sample = next_sample();
            if let Some(dst) = storage.get_mut(channel) {
                dst[frame] = sample;
            }
        }
    }
    for dst in storage.iter_mut().skip(ring_channels) {
        dst[..frames].fill(0.0);
    }
}

/// Sort key over the capture configurations that can run the stream: the fewest channels that
/// still cover the plugin's main input first, then smaller devices, and floating point before
/// integer formats within one channel count.
fn input_config_rank(
    device_channels: usize,
    plugin_channels: usize,
    is_f32: bool,
) -> (bool, usize, bool) {
    (device_channels < plugin_channels, device_channels, !is_f32)
}

/// A built and started capture stream, or `None` (logged) when either step fails, so a capture
/// device that refuses to run costs the session its input rather than its output. `start_failed`
/// runs only after `play` succeeded: it waits for the first capture period and reports whether
/// the stream's error callback fired meanwhile, because WASAPI's `play()` merely queues the start
/// and a refused `IAudioClient::Start()` arrives through the error callback instead.
fn started_capture<S, B, P>(
    built: Result<S, B>,
    play: impl FnOnce(&S) -> Result<(), P>,
    start_failed: impl FnOnce() -> bool,
) -> Option<S>
where
    B: std::fmt::Display,
    P: std::fmt::Display,
{
    let stream = match built {
        Ok(stream) => stream,
        Err(err) => {
            nih_error!("Could not create the capture stream, running without audio input: {err:#}");
            return None;
        }
    };
    if let Err(err) = play(&stream) {
        nih_error!("Could not start the capture stream, running without audio input: {err:#}");
        return None;
    }
    if start_failed() {
        nih_error!(
            "The capture stream failed or delivered nothing while starting, running without audio \
             input"
        );
        return None;
    }
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through_the_backend(device: &[f32], device_channels: usize, frames: usize) -> Vec<Vec<f32>> {
        let mut ring = Vec::new();
        fold_input_frames(device, device_channels, 2, |sample| ring.push(sample));
        let mut storage = vec![vec![f32::NAN; frames]; 2];
        let mut popped = ring.into_iter();
        deinterleave_frames(|| popped.next().unwrap(), &mut storage, 2, frames);
        storage
    }

    #[test]
    fn names_of_drops_blank_and_duplicate_names() {
        let names = dedupe_names(vec![
            "A".to_string(),
            "".to_string(),
            "A".to_string(),
            "B".to_string(),
        ]);
        assert_eq!(names, vec!["A".to_string(), "B".to_string()]);
    }

    fn listed(names: &[&str]) -> (Vec<String>, Vec<String>) {
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        (names.clone(), names)
    }

    #[test]
    fn the_default_host_leaves_its_callback_thread_to_us() {
        assert!(!single_device_duplex(cpal::default_host().id()));
    }

    #[test]
    fn a_duplex_launch_lists_the_host_once() {
        let mut calls = 0;
        let lists = device_lists(Start::Launch, true, Default::default(), || {
            calls += 1;
            vec!["A".to_string(), "B".to_string()]
        });
        assert_eq!(calls, 1);
        assert_eq!(lists, listed(&["A", "B"]));
    }

    #[test]
    fn a_duplex_restart_carries_the_launch_lists_without_listing() {
        let lists = device_lists(Start::Restart, true, listed(&["A", "B"]), || {
            panic!("a restart must not list the host while its driver is loaded")
        });
        assert_eq!(lists, listed(&["A", "B"]));
    }

    #[test]
    fn a_host_that_is_not_duplex_never_lists() {
        for start in [Start::Launch, Start::Restart] {
            let lists = device_lists(start, false, Default::default(), || {
                panic!("only a duplex host is listed")
            });
            assert_eq!(lists, listed(&[]));
        }
    }

    #[test]
    fn a_left_only_stereo_input_lands_in_the_left_channel_only() {
        let device = [0.1, 0.0, 0.2, 0.0, 0.3, 0.0, 0.4, 0.0];
        let storage = through_the_backend(&device, 2, 4);
        assert_eq!(storage[0], vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(storage[1], vec![0.0; 4]);
    }

    #[test]
    fn a_multichannel_device_feeds_its_first_two_channels_only() {
        let device = [1.0, 2.0, 9.0, 9.0, 1.5, 2.5, 9.0, 9.0];
        let storage = through_the_backend(&device, 4, 2);
        assert_eq!(storage[0], vec![1.0, 1.5]);
        assert_eq!(storage[1], vec![2.0, 2.5]);
    }

    #[test]
    fn a_mono_device_drives_both_channels() {
        let device = [0.25, -0.5, 0.75];
        let storage = through_the_backend(&device, 1, 3);
        assert_eq!(storage[0], vec![0.25, -0.5, 0.75]);
        assert_eq!(storage[1], vec![0.25, -0.5, 0.75]);
    }

    #[test]
    fn storage_channels_past_the_ring_are_silenced_and_extra_ring_channels_dropped() {
        let mut wide = vec![vec![f32::NAN; 3]; 4];
        let mut ring = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0].into_iter();
        deinterleave_frames(|| ring.next().unwrap(), &mut wide, 2, 3);
        assert_eq!(wide[0], vec![1.0, 3.0, 5.0]);
        assert_eq!(wide[1], vec![2.0, 4.0, 6.0]);
        assert_eq!(wide[2], vec![0.0; 3]);
        assert_eq!(wide[3], vec![0.0; 3]);

        let mut narrow = vec![vec![f32::NAN; 2]; 1];
        let mut ring = [1.0f32, 2.0, 3.0, 4.0].into_iter();
        deinterleave_frames(|| ring.next().unwrap(), &mut narrow, 2, 2);
        assert_eq!(narrow[0], vec![1.0, 3.0]);
        assert!(ring.next().is_none(), "the ring must stay frame-aligned");
    }

    #[test]
    fn a_partial_chunk_leaves_the_tail_of_the_period_alone() {
        let mut storage = vec![vec![7.0f32; 4]; 2];
        let mut ring = [1.0f32, 2.0].into_iter();
        deinterleave_frames(|| ring.next().unwrap(), &mut storage, 2, 1);
        assert_eq!(storage[0], vec![1.0, 7.0, 7.0, 7.0]);
        assert_eq!(storage[1], vec![2.0, 7.0, 7.0, 7.0]);
    }

    #[test]
    fn the_input_config_prefers_the_fewest_channels_that_cover_the_plugin() {
        assert!(input_config_rank(2, 2, true) < input_config_rank(8, 2, true));
        assert!(input_config_rank(8, 2, true) < input_config_rank(1, 2, true));
        assert!(input_config_rank(2, 2, true) < input_config_rank(2, 2, false));
    }

    #[test]
    fn integer_devices_convert_on_the_way_in() {
        let device: [i16; 4] = [i16::MAX, 0, 0, i16::MIN];
        let mut ring = Vec::new();
        fold_input_frames(&device, 2, 2, |sample| ring.push(sample));
        assert!((ring[0] - 1.0).abs() < 1.0e-4);
        assert_eq!(ring[1], 0.0);
        assert_eq!(ring[2], 0.0);
        assert!((ring[3] + 1.0).abs() < 1.0e-4);
    }

    #[test]
    fn a_capture_that_cannot_build_or_start_is_dropped_instead_of_ending_the_run() {
        let started = |_: &u8| Ok::<(), &str>(());
        let refused = |_: &u8| Err::<(), &str>("privacy switch off");
        let running = || false;
        let not_waited_for =
            || -> bool { unreachable!("a capture that never started was awaited") };
        assert_eq!(
            started_capture(Ok::<u8, &str>(7), started, running),
            Some(7)
        );
        assert_eq!(
            started_capture(Err::<u8, &str>("no such device"), started, not_waited_for),
            None
        );
        assert_eq!(
            started_capture(Ok::<u8, &str>(7), refused, not_waited_for),
            None
        );
    }

    #[test]
    fn a_full_duplex_ring_drops_and_counts_instead_of_spinning() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(2);
        let overflows = AtomicU64::new(0);
        duplex_push(&mut producer, 1.0, &overflows);
        duplex_push(&mut producer, 2.0, &overflows);
        duplex_push(&mut producer, 3.0, &overflows);
        assert_eq!(overflows.load(Ordering::Relaxed), 1);
        let underruns = AtomicU64::new(0);
        assert_eq!(duplex_pop(&mut consumer, &underruns), 1.0);
        assert_eq!(duplex_pop(&mut consumer, &underruns), 2.0);
        assert_eq!(duplex_pop(&mut consumer, &underruns), 0.0);
        assert_eq!(underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_first_output_period_keeps_only_the_newest_input_period() {
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(8);
        for sample in 1..=6 {
            producer.push(sample as f32).unwrap();
        }
        duplex_discard_backlog(&mut consumer, 2);
        assert_eq!(consumer.pop(), Ok(5.0));
        assert_eq!(consumer.pop(), Ok(6.0));
        assert!(consumer.pop().is_err());

        producer.push(7.0).unwrap();
        duplex_discard_backlog(&mut consumer, 4);
        assert_eq!(consumer.pop(), Ok(7.0), "a short backlog is kept whole");
    }

    #[test]
    fn liveness_expires_after_the_timeout_and_not_before() {
        assert!(!liveness_expired(2000, 0));
        assert!(liveness_expired(2001, 0));
        assert!(!liveness_expired(5, 10), "clock skew never trips it");
    }

    fn range(min: u32, max: u32) -> cpal::SupportedBufferSize {
        cpal::SupportedBufferSize::Range { min, max }
    }

    #[test]
    fn period_is_clamped_into_the_driver_range() {
        assert_eq!(clamp_period(64, range(128, 2048)), 128);
        assert_eq!(clamp_period(4096, range(128, 2048)), 2048);
        assert_eq!(clamp_period(512, range(128, 2048)), 512);
        assert_eq!(clamp_period(512, cpal::SupportedBufferSize::Unknown), 512);
        assert_eq!(clamp_period(512, range(2048, 128)), 512);
    }

    #[test]
    fn a_shared_device_keeps_the_requested_period() {
        assert_eq!(DeviceUse::Shared.period(64, range(128, 2048)), 64);
    }

    #[test]
    fn a_duplex_restart_follows_the_driver_but_never_past_the_wrapper_buffer() {
        let launch = DeviceUse::Duplex { period_cap: None };
        assert_eq!(launch.period(64, range(128, 2048)), 128);
        let restart = DeviceUse::Duplex {
            period_cap: Some(512),
        };
        assert_eq!(restart.period(512, range(1024, 2048)), 512);
        assert_eq!(restart.period(512, range(64, 256)), 256);
        assert_eq!(restart.period(256, range(64, 2048)), 512);
    }

    #[test]
    fn a_duplex_restart_returns_to_the_launch_period_once_the_driver_allows_it() {
        let restart = DeviceUse::Duplex {
            period_cap: Some(512),
        };
        let shrunk = restart.period(512, range(64, 256));
        assert_eq!(shrunk, 256);
        assert_eq!(restart.period(shrunk, range(64, 2048)), 512);
    }

    #[test]
    fn duplex_input_takes_the_smallest_config_that_covers_the_plugin_or_the_largest_there_is() {
        assert_eq!(duplex_input_channels(2, &[1, 2, 8]), Some(2));
        assert_eq!(duplex_input_channels(2, &[8, 1]), Some(8));
        assert_eq!(duplex_input_channels(2, &[1]), Some(1));
        assert_eq!(duplex_input_channels(2, &[]), None);
        assert_eq!(duplex_input_channels(0, &[2]), None);
    }

    #[test]
    fn only_asio_is_a_single_device_duplex_host() {
        assert!(!single_device_duplex(cpal::default_host().id()));
    }

    #[test]
    fn a_start_refused_through_the_error_callback_drops_the_capture_too() {
        let queued = |_: &u8| Ok::<(), &str>(());
        let refused_on_the_stream_thread = || true;
        assert_eq!(
            started_capture(Ok::<u8, &str>(7), queued, refused_on_the_stream_thread),
            None
        );
    }
}
