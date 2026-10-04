#![allow(dead_code)]

use nih_plug::prelude::*;
// Explicit: both globs below export a `ProcessContext` (nih-plug's trait, the vst3 crate's
// struct); an explicit import shadows both.
use nih_plug::prelude::ProcessContext;
use nih_plug::wrapper::vst3::vst3::Steinberg::Vst::*;
use nih_plug::wrapper::vst3::vst3::Steinberg::*;
use nih_plug::wrapper::vst3::vst3::{Class, ComWrapper};
use nih_plug::wrapper::vst3::Wrapper;
use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

pub mod clap_rig;

/// One `process` call as the plug-in saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub samples: usize,
    /// `(note, timing)` of every note-on in the block.
    pub notes: Vec<(u8, u32)>,
    pub pos_samples: Option<i64>,
    pub pos_beats: Option<f64>,
    pub bar_start_pos_beats: Option<f64>,
    pub bar_number: Option<i32>,
    /// Length of aux input 0, channel 0, as handed to the plug-in (H6 pins it to `samples`).
    pub aux_len: usize,
    /// The gain parameter's value when the block started.
    pub gain: f32,
    pub event_timings: Vec<u32>,
}

thread_local! {
    /// The wrapper constructs the plug-in itself, so the test reaches its log through this
    /// slot. Thread-local: every `#[test]` thread has its own log.
    pub static CALLS: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
    /// `max_buffer_size` of the last `initialize` on this thread.
    pub static INIT_MAX_BUFFER: Cell<Option<u32>> = const { Cell::new(None) };
    pub static INIT_MIN_BUFFER: Cell<Option<Option<u32>>> = const { Cell::new(None) };
}

#[derive(Params)]
pub struct TestParams {
    #[id = "gain"]
    pub gain: FloatParam,
}

/// Every process call's record, shared by the plug-in variants below.
fn record<P: Plugin>(
    params: &TestParams,
    calls: &Mutex<Vec<Call>>,
    buffer: &mut Buffer,
    aux: &mut AuxiliaryBuffers,
    context: &mut impl ProcessContext<P>,
) -> ProcessStatus {
    let mut notes = Vec::new();
    let mut event_timings = Vec::new();
    while let Some(event) = context.next_event() {
        event_timings.push(event.timing());
        if let NoteEvent::NoteOn { note, timing, .. } = event {
            notes.push((note, timing));
        }
    }
    // A recognisable signal: left = 0.5 plus aux channel 0, right = 0.25.
    let aux0: Vec<f32> = aux
        .inputs
        .first()
        .and_then(|b| b.as_slice_immutable().first().map(|c| c.to_vec()))
        .unwrap_or_default();
    for (i, mut frame) in buffer.iter_samples().enumerate() {
        if let Some(l) = frame.get_mut(0) {
            *l = 0.5 + aux0.get(i).copied().unwrap_or(0.0);
        }
        if let Some(r) = frame.get_mut(1) {
            *r = 0.25;
        }
    }
    calls.lock().unwrap().push(Call {
        samples: buffer.samples(),
        notes,
        pos_samples: context.transport().pos_samples,
        pos_beats: context.transport().pos_beats,
        bar_start_pos_beats: context.transport().bar_start_pos_beats,
        bar_number: context.transport().bar_number,
        aux_len: aux0.len(),
        gain: params.gain.value(),
        event_timings,
    });
    ProcessStatus::Normal
}

/// `TestPlugin` without and `SaaTestPlugin` with sample-accurate automation.
macro_rules! test_plugin {
    ($name:ident, $saa:expr, $vst3_id:expr, $clap_id:expr) => {
        pub struct $name {
            pub params: Arc<TestParams>,
            pub calls: Arc<Mutex<Vec<Call>>>,
        }

        impl Default for $name {
            fn default() -> Self {
                Self {
                    params: Arc::new(TestParams {
                        gain: FloatParam::new(
                            "Gain",
                            1.0,
                            FloatRange::Linear { min: 0.0, max: 2.0 },
                        ),
                    }),
                    calls: CALLS.with(|c| c.clone()),
                }
            }
        }

        impl Plugin for $name {
            const NAME: &'static str = "Host Misbehaviour Test";
            const VENDOR: &'static str = "nih-plug fork tests";
            const URL: &'static str = "";
            const EMAIL: &'static str = "";
            const VERSION: &'static str = "0.0.0";
            const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
                main_input_channels: NonZeroU32::new(2),
                main_output_channels: NonZeroU32::new(2),
                aux_input_ports: &[new_nonzero_u32(2)],
                aux_output_ports: &[],
                names: PortNames::const_default(),
            }];
            const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
            const MIDI_OUTPUT: MidiConfig = MidiConfig::MidiCCs;
            const SAMPLE_ACCURATE_AUTOMATION: bool = $saa;
            type SysExMessage = ();
            type BackgroundTask = ();

            fn params(&self) -> Arc<dyn Params> {
                self.params.clone()
            }

            fn initialize(
                &mut self,
                _audio_io_layout: &AudioIOLayout,
                buffer_config: &BufferConfig,
                _context: &mut impl InitContext<Self>,
            ) -> bool {
                INIT_MAX_BUFFER.with(|m| m.set(Some(buffer_config.max_buffer_size)));
                INIT_MIN_BUFFER.with(|m| m.set(Some(buffer_config.min_buffer_size)));
                true
            }

            fn process(
                &mut self,
                buffer: &mut Buffer,
                aux: &mut AuxiliaryBuffers,
                context: &mut impl ProcessContext<Self>,
            ) -> ProcessStatus {
                record(&self.params, &self.calls, buffer, aux, context)
            }
        }

        impl Vst3Plugin for $name {
            const VST3_CLASS_ID: [u8; 16] = *$vst3_id;
            const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx];
        }

        impl ClapPlugin for $name {
            const CLAP_ID: &'static str = $clap_id;
            const CLAP_DESCRIPTION: Option<&'static str> = None;
            const CLAP_MANUAL_URL: Option<&'static str> = None;
            const CLAP_SUPPORT_URL: Option<&'static str> = None;
            const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::AudioEffect];
        }
    };
}

test_plugin!(
    TestPlugin,
    false,
    b"HostMisbehaveTst",
    "test.host-misbehaviour"
);
test_plugin!(
    SaaTestPlugin,
    true,
    b"HostMisbehaveSAA",
    "test.host-misbehaviour-saa"
);

/// A host-side `IBStream`. `seekable = false` answers `kNotImplemented` to `seek`/`tell`,
/// `chunk` caps every `read`/`write` (short transfers), `eof_is_error` makes the read after
/// the last byte return `kResultFalse` instead of zero bytes, `endless` never runs out,
/// `write_fails` refuses every write, `silent_count` accepts writes without reporting a count.
pub struct TestStream {
    pub data: RefCell<Vec<u8>>,
    pub pos: Cell<usize>,
    pub seekable: bool,
    pub chunk: usize,
    pub eof_is_error: bool,
    pub endless: bool,
    pub write_fails: bool,
    pub silent_count: bool,
    /// Bytes handed out by `read` so far.
    pub delivered: Cell<usize>,
}

impl TestStream {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: RefCell::new(data),
            pos: Cell::new(0),
            seekable: true,
            chunk: usize::MAX,
            eof_is_error: false,
            endless: false,
            write_fails: false,
            silent_count: false,
            delivered: Cell::new(0),
        }
    }

    pub fn into_com(self) -> ComWrapper<Self> {
        ComWrapper::new(self)
    }
}

impl Class for TestStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for TestStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: i32, num_read: *mut i32) -> tresult {
        let want = (num_bytes.max(0) as usize).min(self.chunk);
        if self.endless {
            std::ptr::write_bytes(buffer as *mut u8, b' ', want);
            *num_read = want as i32;
            self.delivered.set(self.delivered.get() + want);
            return kResultOk;
        }
        let data = self.data.borrow();
        let pos = self.pos.get();
        if pos >= data.len() {
            *num_read = 0;
            return if self.eof_is_error {
                kResultFalse
            } else {
                kResultOk
            };
        }
        let n = want.min(data.len() - pos);
        std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
        self.pos.set(pos + n);
        self.delivered.set(self.delivered.get() + n);
        *num_read = n as i32;
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: i32, num_written: *mut i32) -> tresult {
        if self.write_fails {
            *num_written = 0;
            return kResultFalse;
        }
        let n = (num_bytes.max(0) as usize).min(self.chunk);
        let src = std::slice::from_raw_parts(buffer as *const u8, n);
        self.data.borrow_mut().extend_from_slice(src);
        self.pos.set(self.pos.get() + n);
        if !self.silent_count {
            *num_written = n as i32;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> tresult {
        if !self.seekable {
            return kNotImplemented;
        }
        use IBStream_::IStreamSeekMode_::*;
        let len = self.data.borrow().len() as i64;
        let new = match mode {
            m if m == kIBSeekSet as i32 => pos,
            m if m == kIBSeekCur as i32 => self.pos.get() as i64 + pos,
            _ => len + pos,
        }
        .clamp(0, len);
        self.pos.set(new as usize);
        if !result.is_null() {
            *result = new;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        if !self.seekable {
            return kNotImplemented;
        }
        *pos = self.pos.get() as i64;
        kResultOk
    }
}

/// One parameter's automation points for a `process` call: `(sample_offset, normalized)`.
pub struct TestParamQueue {
    pub id: ParamID,
    pub points: Vec<(i32, f64)>,
}

impl Class for TestParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for TestParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.len() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        match usize::try_from(index).ok().and_then(|i| self.points.get(i)) {
            Some(&(o, v)) => {
                *offset = o;
                *value = v;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn addPoint(&self, _offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kNotImplemented
    }
}

/// A host-side `IParameterChanges` carrying the given queues.
pub struct TestParamChanges {
    pub queues: Vec<ComWrapper<TestParamQueue>>,
}

impl TestParamChanges {
    pub fn single(id: ParamID, points: Vec<(i32, f64)>) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            queues: vec![ComWrapper::new(TestParamQueue { id, points })],
        })
    }
}

impl Class for TestParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for TestParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.len() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.queues.get(i))
            .and_then(|q| q.as_com_ref::<IParamValueQueue>())
            .map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }

    unsafe fn addParameterData(
        &self,
        _id: *const ParamID,
        _index: *mut int32,
    ) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

pub fn param_changes_ptr(changes: &ComWrapper<TestParamChanges>) -> *mut IParameterChanges {
    changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr()
}

pub fn stream_ptr(stream: &ComWrapper<TestStream>) -> *mut IBStream {
    stream.as_com_ref::<IBStream>().unwrap().as_ptr()
}

/// `setupProcessing` + `setActive(true)` + `setProcessing(true)` at 48 kHz.
pub unsafe fn setup_and_activate<P: Vst3Plugin>(wrapper: &Wrapper<P>, max_block: i32) {
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        maxSamplesPerBlock: max_block,
        sampleRate: 48000.0,
    };
    assert_eq!(wrapper.setupProcessing(&mut setup), kResultOk);
    assert_eq!(wrapper.setActive(1), kResultOk);
    assert_eq!(wrapper.setProcessing(1), kResultOk);
}

/// Owned host-side audio memory for one `process` call.
pub struct HostBuffers {
    pub channels: Vec<Vec<f32>>,
    pub ptrs: Vec<*mut f32>,
    pub bus: AudioBusBuffers,
}

impl HostBuffers {
    pub fn new(num_channels: usize, num_samples: usize, fill: f32) -> Self {
        let mut channels: Vec<Vec<f32>> =
            (0..num_channels).map(|_| vec![fill; num_samples]).collect();
        let ptrs: Vec<*mut f32> = channels.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let mut this = Self {
            channels,
            ptrs,
            bus: unsafe { std::mem::zeroed() },
        };
        this.bus.numChannels = num_channels as i32;
        this.bus.__field0.channelBuffers32 = this.ptrs.as_mut_ptr();
        this
    }

    /// Null one channel pointer (a host that lists a channel it does not provide).
    pub fn null_channel(&mut self, idx: usize) {
        self.ptrs[idx] = std::ptr::null_mut();
    }
}

pub fn process_data(
    num_samples: i32,
    inputs: &mut [AudioBusBuffers],
    outputs: &mut [AudioBusBuffers],
) -> ProcessData {
    ProcessData {
        processMode: ProcessModes_::kRealtime as i32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
        numSamples: num_samples,
        numInputs: inputs.len() as i32,
        numOutputs: outputs.len() as i32,
        inputs: if inputs.is_empty() {
            std::ptr::null_mut()
        } else {
            inputs.as_mut_ptr()
        },
        outputs: if outputs.is_empty() {
            std::ptr::null_mut()
        } else {
            outputs.as_mut_ptr()
        },
        inputParameterChanges: std::ptr::null_mut(),
        outputParameterChanges: std::ptr::null_mut(),
        inputEvents: std::ptr::null_mut(),
        outputEvents: std::ptr::null_mut(),
        processContext: std::ptr::null_mut(),
    }
}

pub fn new_wrapper() -> Wrapper<TestPlugin> {
    Wrapper::<TestPlugin>::new()
}

pub fn new_saa_wrapper() -> Wrapper<SaaTestPlugin> {
    Wrapper::<SaaTestPlugin>::new()
}

/// The plug-in instance's call log (see [`CALLS`]).
pub fn calls_of(_wrapper: &Wrapper<TestPlugin>) -> Arc<Mutex<Vec<Call>>> {
    calls_of_any()
}

/// The call log of the plug-in constructed on this thread, whichever wrapper made it.
pub fn calls_of_any() -> Arc<Mutex<Vec<Call>>> {
    CALLS.with(|c| c.clone())
}

/// A host-side `IEventList` handing the given events to `process`.
pub struct TestEventList {
    pub events: Vec<Event>,
    /// Indices past the real events that `getEventCount` still reports (`getEvent` fails).
    pub phantom: usize,
}

impl TestEventList {
    pub fn new(events: Vec<Event>) -> Self {
        Self { events, phantom: 0 }
    }

    pub fn into_com(self) -> ComWrapper<Self> {
        ComWrapper::new(self)
    }
}

impl Class for TestEventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for TestEventList {
    unsafe fn getEventCount(&self) -> int32 {
        (self.events.len() + self.phantom) as int32
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        match usize::try_from(index).ok().and_then(|i| self.events.get(i)) {
            Some(ev) => {
                *e = *ev;
                kResultOk
            }
            None => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, _e: *mut Event) -> tresult {
        kNotImplemented
    }
}

pub fn event_list_ptr(events: &ComWrapper<TestEventList>) -> *mut IEventList {
    events.as_com_ref::<IEventList>().unwrap().as_ptr()
}

pub fn note_on(pitch: i16, sample_offset: i32) -> Event {
    let mut e: Event = unsafe { std::mem::zeroed() };
    e.busIndex = 0;
    e.sampleOffset = sample_offset;
    e.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
    e.__field0.noteOn = NoteOnEvent {
        channel: 0,
        pitch,
        tuning: 0.0,
        velocity: 0.8,
        length: 0,
        noteId: -1,
    };
    e
}
