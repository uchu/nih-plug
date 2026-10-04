//! Helpers for safely constructing [`Buffer`]s from a plugin host's audio buffers.

use std::num::NonZeroU32;
use std::ptr::NonNull;

use crate::prelude::{AudioIOLayout, Buffer};

/// Buffers created using [`create_buffers`]. At some point the main `Plugin::process()` should
/// probably also take an argument like this instead of main+aux buffers if we also want to provide
/// access to overflowing input channels for e.g. stereo to mono plugins.
pub struct Buffers<'a, 'buffer: 'a> {
    pub main_buffer: &'a mut Buffer<'buffer>,

    // We can't use `AuxiliaryBuffers` here directly because we need different lifetimes for `'a`
    // and `'buffer` while `AuxiliaryBuffers` uses the same lifetime for both.
    pub aux_inputs: &'a mut [Buffer<'buffer>],
    pub aux_outputs: &'a mut [Buffer<'buffer>],
}

/// A helper for safely creating and initializing [`Buffer`]s based on the host's input and output
/// buffers.
pub struct BufferManager {
    // These are the storage backing the fields in `BufferSource`. The wrapper needs to set these
    // values to match the channel pointers provided by the host. If audio buffers are not provided
    // for a bus, then they should be set to `None`. This helper will then copy data to the buffers
    // or fill them with zeroes if there is no data, while also accounting for in-place main IO
    // buffers.
    main_input_channel_pointers: Option<ChannelPointers>,
    main_output_channel_pointers: Option<ChannelPointers>,
    aux_input_channel_pointers: Vec<Option<ChannelPointers>>,
    aux_output_channel_pointers: Vec<Option<ChannelPointers>>,

    /// The backing buffers that will be filled during `create_buffers`. This `'static` lifetime
    /// will be shortened when returning a reference to these buffers in `create_buffers` to match
    /// the function's lifetime.
    main_buffer: Buffer<'static>,

    aux_input_buffers: Vec<Buffer<'static>>,
    /// Stores the data to back `aux_input_buffers`. We need to copy the host's auxiliary input
    /// buffers to our own first because the `Buffer` API is designed around mutable buffers, and
    /// the host may reuse its input buffers between plugins.
    aux_input_storage: Vec<Vec<Vec<f32>>>,

    aux_output_buffers: Vec<Buffer<'static>>,

    /// The declared main input channel count. Host input channels past it are ignored.
    main_input_channels: usize,
    /// Backs a declared main output channel the host did not provide, so the plugin still runs
    /// on the channels the host does have. Sized `max_buffer_size` per channel.
    main_output_scratch: Vec<Vec<f32>>,
    /// The same for every auxiliary output port.
    aux_output_scratch: Vec<Vec<Vec<f32>>>,
    /// The capacity every scratch and aux storage buffer was allocated with.
    max_buffer_size: usize,
}

// SAFETY: The raw pointers in the `ChannelPointers` fields/vectors are only used as scratch storage
//         inside of the `create_buffers()` function.
unsafe impl Send for BufferManager {}
unsafe impl Sync for BufferManager {}

/// Host data that the plugin's [`Buffer`]s should be created from. Leave these fields as `None`
/// values
#[derive(Debug)]
pub struct BufferSource<'a> {
    pub main_input_channel_pointers: &'a mut Option<ChannelPointers>,
    pub main_output_channel_pointers: &'a mut Option<ChannelPointers>,
    pub aux_input_channel_pointers: &'a mut [Option<ChannelPointers>],
    pub aux_output_channel_pointers: &'a mut [Option<ChannelPointers>],
}

/// Pointers to raw multichannel audio data for this port.
#[derive(Debug, Clone, Copy)]
pub struct ChannelPointers {
    /// A raw pointer to an array of f32 arrays, containing one array for each channel. `ptrs` must
    /// contain (at least) `num_channel` `*const f32`s, and each of those inner arrays must contain
    /// (at least) `num_samples` `f32` values.
    pub ptrs: NonNull<*mut f32>,
    /// The number of audio channels used for this port.
    pub num_channels: usize,
}

impl BufferManager {
    /// Initialize managed buffers for a specific audio IO layout. The actual buffers can be set up
    /// using channel pointer data using [`create_buffers()`][Self::create_buffers()].
    pub fn for_audio_io_layout(max_buffer_size: usize, audio_io_layout: AudioIOLayout) -> Self {
        let main_input_channels = audio_io_layout
            .main_input_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;
        let main_output_channels = audio_io_layout
            .main_output_channels
            .map(NonZeroU32::get)
            .unwrap_or(0) as usize;

        // The buffers are preallocated so that `create_buffers()` can be called without having to
        // allocate
        let mut main_buffer = Buffer::default();
        unsafe {
            main_buffer.set_slices(0, |output_slices| {
                output_slices.resize_with(main_output_channels, || &mut []);
            })
        };

        let mut aux_input_buffers = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        let mut aux_input_storage = Vec::with_capacity(audio_io_layout.aux_input_ports.len());
        for num_channels in audio_io_layout.aux_input_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_input_buffers.push(buffer);
            aux_input_storage.push(vec![
                vec![0.0; max_buffer_size];
                num_channels.get() as usize
            ]);
        }

        let mut aux_output_buffers = Vec::with_capacity(audio_io_layout.aux_output_ports.len());
        for num_channels in audio_io_layout.aux_output_ports {
            let mut buffer = Buffer::default();
            unsafe {
                buffer.set_slices(0, |slices| {
                    slices.resize_with(num_channels.get() as usize, || &mut []);
                })
            };

            aux_output_buffers.push(buffer);
        }

        Self {
            main_input_channel_pointers: None,
            main_output_channel_pointers: None,
            aux_input_channel_pointers: vec![None; audio_io_layout.aux_input_ports.len()],
            aux_output_channel_pointers: vec![None; audio_io_layout.aux_output_ports.len()],

            main_buffer,

            aux_input_buffers,
            aux_input_storage,

            aux_output_buffers,

            main_input_channels,
            main_output_scratch: vec![vec![0.0; max_buffer_size]; main_output_channels],
            aux_output_scratch: audio_io_layout
                .aux_output_ports
                .iter()
                .map(|n| vec![vec![0.0; max_buffer_size]; n.get() as usize])
                .collect(),
            max_buffer_size,
        }
    }

    /// The block length this manager was allocated for. Wrappers split host blocks by this value
    /// rather than by the current buffer config, which a host may change while the plugin is
    /// active.
    pub fn max_buffer_size(&self) -> usize {
        self.max_buffer_size
    }

    /// Initialize the buffers using the host provided buffer pointers and return a reference to the
    /// created buffers that can be passed to `Plugin::process()`. This accounts for in-place main
    /// IO, missing channel pointers, null pointers, and mismatching channel counts. All
    /// uninitialized buffer data (aux outputs, main output channels with no matching input channel,
    /// and the whole main buffer when the host provides no main input) are filled with zeroes.
    ///
    /// `sample_offset` and `num_samples` can be used to slice a set of host channel pointers for
    /// sample accurate automation. Channel counts that differ from the layout are absorbed: a
    /// declared output channel the host did not provide (too few channels, or a null channel
    /// pointer) is backed by zeroed scratch storage, host output channels past the declared count
    /// are zeroed, and host input channels past the declared count are ignored. A null input
    /// channel pointer reads as silence. Only a main output bus the host did not provide at all
    /// leaves the main buffer's slices empty.
    ///
    /// `num_samples` must not exceed the `max_buffer_size` the manager was created with.
    ///
    /// # Safety
    ///
    /// Any provided `ChannelPointers` must point to memory regions that remain valid to read from
    /// or write to for the lifetime of the returned [`Buffers`].
    pub unsafe fn create_buffers<'a, 'buffer: 'a>(
        &'a mut self,
        sample_offset: usize,
        num_samples: usize,
        set_buffer_sources: impl FnOnce(&mut BufferSource),
    ) -> Buffers<'a, 'buffer> {
        nih_debug_assert!(num_samples <= self.max_buffer_size());

        // Make sure the caller can't forget to unset previously set values
        self.main_input_channel_pointers = None;
        self.main_output_channel_pointers = None;
        self.aux_input_channel_pointers.fill(None);
        self.aux_output_channel_pointers.fill(None);
        set_buffer_sources(&mut BufferSource {
            main_input_channel_pointers: &mut self.main_input_channel_pointers,
            main_output_channel_pointers: &mut self.main_output_channel_pointers,
            aux_input_channel_pointers: &mut self.aux_input_channel_pointers,
            aux_output_channel_pointers: &mut self.aux_output_channel_pointers,
        });

        // The main buffer points directly to the main output pointers
        let main_output_scratch = &mut self.main_output_scratch;
        self.main_buffer.set_slices(num_samples, |output_slices| {
            match self.main_output_channel_pointers {
                Some(output_channel_pointers) => point_output_slices(
                    output_slices,
                    output_channel_pointers,
                    main_output_scratch,
                    sample_offset,
                    num_samples,
                ),
                None => {
                    nih_debug_assert_eq!(output_slices.len(), 0);

                    // If the caller/host should have provided buffer pointers but didn't then we
                    // must get rid of any dangling slices
                    output_slices.fill_with(|| &mut [])
                }
            }
        });

        // Since NIH-plug processes audio in-place, main input data needs to be copied to the main
        // output buffers
        if let (Some(input_channel_pointers), Some(_)) = (
            self.main_input_channel_pointers,
            self.main_output_channel_pointers,
        ) {
            let main_input_channels = self.main_input_channels;
            self.main_buffer.set_slices(num_samples, |output_slices| {
                let copy_channels = input_channel_pointers
                    .num_channels
                    .min(main_input_channels)
                    .min(output_slices.len());
                for (channel_idx, output_slice) in
                    output_slices.iter_mut().enumerate().take(copy_channels)
                {
                    let input_channel_pointer =
                        *input_channel_pointers.ptrs.as_ptr().add(channel_idx);
                    if input_channel_pointer.is_null() {
                        output_slice.fill(0.0);
                        continue;
                    }

                    // If the host processes the main IO out of place then the inputs need to be
                    // copied to the output buffers. Otherwise the input should already be there.
                    let input_slice_pointer = input_channel_pointer.add(sample_offset);
                    if !std::ptr::eq(input_slice_pointer, output_slice.as_ptr()) {
                        output_slice.copy_from_slice(std::slice::from_raw_parts(
                            input_slice_pointer,
                            num_samples,
                        ))
                    }
                }

                // Any excess channels will need to be filled with zeroes since they'd otherwise
                // point to whatever was left in the buffer
                for output_slice in output_slices.iter_mut().skip(copy_channels) {
                    output_slice.fill(0.0);
                }
            });
        } else if self.main_output_channel_pointers.is_some() {
            // No main input from the host (VST3 `numInputs == 0` or a null input bus, CLAP
            // `audio_inputs_count == 0` or null `data32`). The main buffer aliases the host's output
            // memory, which still holds whatever the host left there, and a plugin reading its
            // in-place input would take that for audio. Zero it, like the excess channels above.
            self.main_buffer.set_slices(num_samples, |output_slices| {
                for slice in output_slices.iter_mut() {
                    slice.fill(0.0);
                }
            });
        }

        // Because NIH-plug's `Buffer` type is geared around in-place processing, auxiliary inputs
        // need to be copied to our own buffers first (backed by the 'storage' vectors on this
        // object). That way the plugin can modify those buffers like any other buffers.
        for (input_channel_pointers, (input_storage, input_buffer)) in
            self.aux_input_channel_pointers.iter().zip(
                self.aux_input_storage
                    .iter_mut()
                    .zip(self.aux_input_buffers.iter_mut()),
            )
        {
            // Since these buffers are backed by our own storage, we can fill them with zeroes if
            // the pointers are missing for whatever reason that might be, like an unconnected
            // sidechain. Every channel is resized to `num_samples` first, so a bus that goes
            // missing before a longer block never leaves a short slice behind.
            nih_debug_assert!(input_storage
                .iter()
                .all(|channel| num_samples <= channel.capacity()));
            for (channel_idx, channel) in input_storage.iter_mut().enumerate() {
                channel.resize(num_samples, 0.0);
                let input_channel_pointer = match input_channel_pointers {
                    Some(pointers) if channel_idx < pointers.num_channels => {
                        *pointers.ptrs.as_ptr().add(channel_idx)
                    }
                    _ => std::ptr::null_mut(),
                };
                if input_channel_pointer.is_null() {
                    channel.fill(0.0);
                } else {
                    channel.copy_from_slice(std::slice::from_raw_parts(
                        input_channel_pointer.add(sample_offset),
                        num_samples,
                    ))
                }
            }

            input_buffer.set_slices(num_samples, |input_slices| {
                // Since we initialized both `input_buffer` and `input_storage` this invariant
                // should never fail unless we made an error ourselves
                debug_assert_eq!(input_slices.len(), input_storage.len());

                for (channel_slice, channel_storage) in
                    input_slices.iter_mut().zip(input_storage.iter_mut())
                {
                    // SAFETY: `channel_storage` is no longer used accessed directly after this
                    *channel_slice = &mut *(channel_storage.as_mut_slice() as *mut [f32]);
                }
            });
        }

        // The auxiliary output buffers can point directly to the host's buffers. This logic is the
        // same as the main outputs, minus the copying of input data. A port the host did not
        // provide at all is backed entirely by scratch storage.
        for ((output_channel_pointers, output_buffer), output_scratch) in self
            .aux_output_channel_pointers
            .iter()
            .zip(self.aux_output_buffers.iter_mut())
            .zip(self.aux_output_scratch.iter_mut())
        {
            output_buffer.set_slices(num_samples, |output_slices| {
                match output_channel_pointers {
                    Some(output_channel_pointers) => point_output_slices(
                        output_slices,
                        *output_channel_pointers,
                        output_scratch,
                        sample_offset,
                        num_samples,
                    ),
                    None => {
                        for (output_slice, channel) in
                            output_slices.iter_mut().zip(output_scratch.iter_mut())
                        {
                            *output_slice = zeroed_scratch_slice(channel, num_samples);
                        }
                    }
                }

                // The host may not zero out the buffers, and assume the plugin always writes
                // something there
                for output_slice in output_slices.iter_mut() {
                    output_slice.fill(0.0);
                }
            });
        }

        // SAFETY: The 'static lifetimes on the objects are needed so we can store the buffers.
        //         Their actual lifetimes are `'a`, so we need to shrink them here. The contents are
        //         valid for as long as the returned object is borrowed.
        std::mem::transmute::<Buffers<'a, 'static>, Buffers<'a, 'buffer>>(Buffers {
            main_buffer: &mut self.main_buffer,
            aux_inputs: &mut self.aux_input_buffers,
            aux_outputs: &mut self.aux_output_buffers,
        })
    }
}

/// Point `output_slices` at the host's output channels. A declared channel the host did not
/// provide is backed by `scratch`, and host channels past the declared count are zeroed so they
/// never carry stale memory.
///
/// # Safety
///
/// `output_channel_pointers` must hold `num_channels` channel pointers, each either null or
/// valid for `sample_offset + num_samples` samples.
unsafe fn point_output_slices(
    output_slices: &mut [&'static mut [f32]],
    output_channel_pointers: ChannelPointers,
    scratch: &mut [Vec<f32>],
    sample_offset: usize,
    num_samples: usize,
) {
    for (channel_idx, (output_slice, channel)) in
        output_slices.iter_mut().zip(scratch.iter_mut()).enumerate()
    {
        let host_pointer = if channel_idx < output_channel_pointers.num_channels {
            *output_channel_pointers.ptrs.as_ptr().add(channel_idx)
        } else {
            std::ptr::null_mut()
        };
        *output_slice = if host_pointer.is_null() {
            zeroed_scratch_slice(channel, num_samples)
        } else {
            std::slice::from_raw_parts_mut(host_pointer.add(sample_offset), num_samples)
        };
    }

    for channel_idx in output_slices.len()..output_channel_pointers.num_channels {
        let host_pointer = *output_channel_pointers.ptrs.as_ptr().add(channel_idx);
        if !host_pointer.is_null() {
            std::slice::from_raw_parts_mut(host_pointer.add(sample_offset), num_samples).fill(0.0);
        }
    }
}

/// A zeroed `num_samples`-long slice of `channel`, with the lifetime erased so it can be stored
/// in a [`Buffer<'static>`]. The caller shortens it again before handing it out.
unsafe fn zeroed_scratch_slice(channel: &mut Vec<f32>, num_samples: usize) -> &'static mut [f32] {
    nih_debug_assert!(num_samples <= channel.len());
    if channel.len() < num_samples {
        channel.resize(num_samples, 0.0);
    }
    let slice = &mut channel[..num_samples];
    slice.fill(0.0);
    std::slice::from_raw_parts_mut(slice.as_mut_ptr(), num_samples)
}

#[cfg(any(miri, test))]
mod miri {
    use super::*;
    use crate::prelude::{new_nonzero_u32, PortNames};

    const BUFFER_SIZE: usize = 512;
    const NUM_MAIN_INPUT_CHANNELS: usize = 1;
    const NUM_MAIN_OUTPUT_CHANNELS: usize = 2;

    const NUM_AUX_CHANNELS: usize = 2;
    const NUM_AUX_PORTS: usize = 2;

    const AUDIO_IO_LAYOUT: AudioIOLayout = AudioIOLayout {
        main_input_channels: Some(new_nonzero_u32(NUM_MAIN_INPUT_CHANNELS as u32)),
        main_output_channels: Some(new_nonzero_u32(NUM_MAIN_OUTPUT_CHANNELS as u32)),
        aux_input_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        aux_output_ports: &[new_nonzero_u32(NUM_AUX_CHANNELS as u32); NUM_AUX_PORTS],
        names: PortNames::const_default(),
    };

    #[test]
    fn buffer_io() {
        // This works very similarly to the standalone CPAL and dummy backends
        let mut main_io_storage = vec![vec![0.0f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS];
        let mut aux_input_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];
        let mut aux_output_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];

        let mut main_io_channel_pointers: Vec<*mut f32> = main_io_storage
            .iter_mut()
            .map(|channel_slice| channel_slice.as_mut_ptr())
            .collect();
        let mut aux_input_channel_pointers: Vec<Vec<*mut f32>> = aux_input_storage
            .iter_mut()
            .map(|aux_input_storage| {
                aux_input_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();
        let mut aux_output_channel_pointers: Vec<Vec<*mut f32>> = aux_output_storage
            .iter_mut()
            .map(|aux_output_storage| {
                aux_output_storage
                    .iter_mut()
                    .map(|channel_slice| channel_slice.as_mut_ptr())
                    .collect()
            })
            .collect();

        // The actual buffer management here works the same as in the JACK backend. See that
        // implementation for more information.
        let mut buffer_manager = BufferManager::for_audio_io_layout(BUFFER_SIZE, AUDIO_IO_LAYOUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, BUFFER_SIZE, |buffer_sources| {
                *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: main_io_channel_pointers.len(),
                });
                *buffer_sources.main_input_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: NUM_MAIN_INPUT_CHANNELS.min(main_io_channel_pointers.len()),
                });

                for (input_source_channel_pointers, input_channel_pointers) in buffer_sources
                    .aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_channel_pointers.iter_mut())
                {
                    *input_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(input_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: input_channel_pointers.len(),
                    });
                }

                for (output_source_channel_pointers, output_channel_pointers) in buffer_sources
                    .aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_channel_pointers.iter_mut())
                {
                    *output_source_channel_pointers = Some(ChannelPointers {
                        ptrs: NonNull::new(output_channel_pointers.as_mut_ptr()).unwrap(),
                        num_channels: output_channel_pointers.len(),
                    });
                }
            })
        };

        for channel_samples in buffers
            .main_buffer
            .iter_samples()
            .chain(
                buffers
                    .aux_inputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
            .chain(
                buffers
                    .aux_outputs
                    .iter_mut()
                    .flat_map(|buffer| buffer.iter_samples()),
            )
        {
            for sample in channel_samples {
                *sample += 1.0;
            }
        }

        // These checks are fine due to stacked borrows even without explicitly dropping `buffers`.
        // If we were to access `buffers` again after this miri would trigger an error.
        for channel in main_io_storage
            .iter()
            .chain(aux_output_storage.iter().flat_map(|storage| storage.iter()))
        {
            for sample in channel {
                assert!(*sample == 1.0);
            }
        }

        for channel in aux_input_storage.iter().flat_map(|storage| storage.iter()) {
            for sample in channel {
                assert!(*sample == 0.0);
            }
        }
    }

    #[test]
    fn missing_main_input_zeroes_the_in_place_buffer() {
        // The host's output memory still holds its last block. With no main input pointers that
        // must not reach the plugin as input.
        let mut main_io_storage = vec![vec![0.5f32; BUFFER_SIZE]; NUM_MAIN_OUTPUT_CHANNELS];
        let mut aux_input_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];
        let mut aux_output_storage =
            vec![vec![vec![0.0f32; BUFFER_SIZE]; NUM_AUX_CHANNELS]; NUM_AUX_PORTS];

        let mut main_io_channel_pointers: Vec<*mut f32> = main_io_storage
            .iter_mut()
            .map(|channel_slice| channel_slice.as_mut_ptr())
            .collect();
        let mut aux_input_channel_pointers: Vec<Vec<*mut f32>> = aux_input_storage
            .iter_mut()
            .map(|storage| storage.iter_mut().map(|c| c.as_mut_ptr()).collect())
            .collect();
        let mut aux_output_channel_pointers: Vec<Vec<*mut f32>> = aux_output_storage
            .iter_mut()
            .map(|storage| storage.iter_mut().map(|c| c.as_mut_ptr()).collect())
            .collect();

        let mut buffer_manager = BufferManager::for_audio_io_layout(BUFFER_SIZE, AUDIO_IO_LAYOUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, BUFFER_SIZE, |buffer_sources| {
                *buffer_sources.main_output_channel_pointers = Some(ChannelPointers {
                    ptrs: NonNull::new(main_io_channel_pointers.as_mut_ptr()).unwrap(),
                    num_channels: main_io_channel_pointers.len(),
                });
                for (source, pointers) in buffer_sources
                    .aux_input_channel_pointers
                    .iter_mut()
                    .zip(aux_input_channel_pointers.iter_mut())
                {
                    *source = Some(ChannelPointers {
                        ptrs: NonNull::new(pointers.as_mut_ptr()).unwrap(),
                        num_channels: pointers.len(),
                    });
                }
                for (source, pointers) in buffer_sources
                    .aux_output_channel_pointers
                    .iter_mut()
                    .zip(aux_output_channel_pointers.iter_mut())
                {
                    *source = Some(ChannelPointers {
                        ptrs: NonNull::new(pointers.as_mut_ptr()).unwrap(),
                        num_channels: pointers.len(),
                    });
                }
            })
        };

        assert_eq!(buffers.main_buffer.channels(), NUM_MAIN_OUTPUT_CHANNELS);
        for channel_samples in buffers.main_buffer.iter_samples() {
            for sample in channel_samples {
                assert_eq!(*sample, 0.0);
            }
        }
    }

    fn pointers(channels: &mut [*mut f32]) -> ChannelPointers {
        ChannelPointers {
            ptrs: NonNull::new(channels.as_mut_ptr()).unwrap(),
            num_channels: channels.len(),
        }
    }

    fn channel_pointers(storage: &mut [Vec<f32>]) -> Vec<*mut f32> {
        storage.iter_mut().map(|c| c.as_mut_ptr()).collect()
    }

    const STEREO_OUT: AudioIOLayout = AudioIOLayout {
        main_input_channels: None,
        main_output_channels: Some(new_nonzero_u32(2)),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    };

    #[test]
    fn host_output_with_more_channels_than_declared_zeroes_the_extras() {
        let mut host = vec![vec![0.7f32; 64]; 4];
        let mut host_pointers = channel_pointers(&mut host);

        let mut buffer_manager = BufferManager::for_audio_io_layout(64, STEREO_OUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, 64, |sources| {
                *sources.main_output_channel_pointers = Some(pointers(&mut host_pointers));
            })
        };
        assert_eq!(buffers.main_buffer.channels(), 2);
        for channel in buffers.main_buffer.as_slice() {
            channel.fill(1.0);
        }

        assert!(host[0].iter().chain(&host[1]).all(|&x| x == 1.0));
        assert!(host[2].iter().chain(&host[3]).all(|&x| x == 0.0));
    }

    #[test]
    fn host_output_with_fewer_channels_than_declared_backs_the_rest_with_scratch() {
        let mut host = vec![vec![0.7f32; 64]; 1];
        let mut host_pointers = channel_pointers(&mut host);

        let mut buffer_manager = BufferManager::for_audio_io_layout(64, STEREO_OUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, 64, |sources| {
                *sources.main_output_channel_pointers = Some(pointers(&mut host_pointers));
            })
        };
        let slices = buffers.main_buffer.as_slice();
        assert_eq!(slices.len(), 2);
        assert!(slices.iter().all(|slice| slice.len() == 64));
        assert!(slices[1].iter().all(|&x| x == 0.0));
        slices[0].fill(1.0);
        slices[1].fill(2.0);

        assert!(host[0].iter().all(|&x| x == 1.0));
    }

    #[test]
    fn a_null_host_output_channel_is_backed_by_scratch() {
        let mut host = vec![vec![0.7f32; 64]; 2];
        let mut host_pointers = channel_pointers(&mut host);
        host_pointers[1] = std::ptr::null_mut();

        let mut buffer_manager = BufferManager::for_audio_io_layout(64, STEREO_OUT);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, 64, |sources| {
                *sources.main_output_channel_pointers = Some(pointers(&mut host_pointers));
            })
        };
        let slices = buffers.main_buffer.as_slice();
        assert!(slices.iter().all(|slice| slice.len() == 64));
        slices[0].fill(1.0);
        slices[1].fill(2.0);

        assert!(host[0].iter().all(|&x| x == 1.0));
        assert!(host[1].iter().all(|&x| x == 0.7));
    }

    #[test]
    fn host_input_channels_past_the_declared_count_are_ignored() {
        const STEREO_TO_MONO: AudioIOLayout = AudioIOLayout {
            main_input_channels: Some(new_nonzero_u32(2)),
            main_output_channels: Some(new_nonzero_u32(1)),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        };
        let mut input = vec![vec![0.3f32; 64], vec![0.6f32; 64]];
        let mut input_pointers = channel_pointers(&mut input);
        let mut output = vec![vec![0.0f32; 64]];
        let mut output_pointers = channel_pointers(&mut output);

        let mut buffer_manager = BufferManager::for_audio_io_layout(64, STEREO_TO_MONO);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, 64, |sources| {
                *sources.main_input_channel_pointers = Some(pointers(&mut input_pointers));
                *sources.main_output_channel_pointers = Some(pointers(&mut output_pointers));
            })
        };
        assert_eq!(buffers.main_buffer.channels(), 1);
        assert!(buffers.main_buffer.as_slice()[0].iter().all(|&x| x == 0.3));
    }

    #[test]
    fn a_null_host_input_channel_reads_as_silence() {
        const STEREO_IO: AudioIOLayout = AudioIOLayout {
            main_input_channels: Some(new_nonzero_u32(2)),
            main_output_channels: Some(new_nonzero_u32(2)),
            aux_input_ports: &[new_nonzero_u32(2)],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        };
        let mut input = vec![vec![0.3f32; 64]; 2];
        let mut input_pointers = channel_pointers(&mut input);
        input_pointers[1] = std::ptr::null_mut();
        let mut aux = vec![vec![0.4f32; 64]; 2];
        let mut aux_pointers = channel_pointers(&mut aux);
        aux_pointers[0] = std::ptr::null_mut();
        let mut output = vec![vec![0.9f32; 64]; 2];
        let mut output_pointers = channel_pointers(&mut output);

        let mut buffer_manager = BufferManager::for_audio_io_layout(64, STEREO_IO);
        let buffers = unsafe {
            buffer_manager.create_buffers(0, 64, |sources| {
                *sources.main_input_channel_pointers = Some(pointers(&mut input_pointers));
                *sources.main_output_channel_pointers = Some(pointers(&mut output_pointers));
                sources.aux_input_channel_pointers[0] = Some(pointers(&mut aux_pointers));
            })
        };
        let main = buffers.main_buffer.as_slice();
        assert!(main[0].iter().all(|&x| x == 0.3));
        assert!(main[1].iter().all(|&x| x == 0.0));
        let aux_slices = buffers.aux_inputs[0].as_slice();
        assert!(aux_slices[0].iter().all(|&x| x == 0.0));
        assert!(aux_slices[1].iter().all(|&x| x == 0.4));
    }

    #[test]
    fn aux_fill_covers_the_whole_block_after_a_shorter_one() {
        const WITH_SIDECHAIN: AudioIOLayout = AudioIOLayout {
            main_input_channels: None,
            main_output_channels: Some(new_nonzero_u32(2)),
            aux_input_ports: &[new_nonzero_u32(2)],
            aux_output_ports: &[new_nonzero_u32(2)],
            names: PortNames::const_default(),
        };
        let mut output = vec![vec![0.0f32; 512]; 2];
        let mut output_pointers = channel_pointers(&mut output);
        let mut aux = vec![vec![0.4f32; 256]; 2];
        let mut aux_pointers = channel_pointers(&mut aux);
        let mut aux_output = vec![vec![0.4f32; 256]; 2];
        let mut aux_output_pointers = channel_pointers(&mut aux_output);

        let mut buffer_manager = BufferManager::for_audio_io_layout(512, WITH_SIDECHAIN);
        unsafe {
            let buffers = buffer_manager.create_buffers(0, 256, |sources| {
                *sources.main_output_channel_pointers = Some(pointers(&mut output_pointers));
                sources.aux_input_channel_pointers[0] = Some(pointers(&mut aux_pointers));
                sources.aux_output_channel_pointers[0] = Some(pointers(&mut aux_output_pointers));
            });
            assert!(buffers.aux_inputs[0].as_slice()[0]
                .iter()
                .all(|&x| x == 0.4));
        }

        let buffers = unsafe {
            buffer_manager.create_buffers(0, 512, |sources| {
                *sources.main_output_channel_pointers = Some(pointers(&mut output_pointers));
            })
        };
        for slice in buffers.aux_inputs[0].as_slice() {
            assert_eq!(slice.len(), 512);
            assert!(slice.iter().all(|&x| x == 0.0));
        }
        for slice in buffers.aux_outputs[0].as_slice() {
            assert_eq!(slice.len(), 512);
            assert!(slice.iter().all(|&x| x == 0.0));
        }
    }

    #[test]
    fn reports_the_capacity_it_was_allocated_with() {
        let buffer_manager = BufferManager::for_audio_io_layout(BUFFER_SIZE, AUDIO_IO_LAYOUT);
        assert_eq!(buffer_manager.max_buffer_size(), BUFFER_SIZE);
    }
}
