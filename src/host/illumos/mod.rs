use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use illumos_audio::mixer::AudioInfo;
use illumos_audio::sys::AudioFormats;
use illumos_audio::Dsp;

use crate::traits::{DeviceTrait, HostTrait, StreamTrait};
use crate::{
    BackendSpecificError, BuildStreamError, Data, DefaultStreamConfigError, DeviceNameError,
    DevicesError, InputCallbackInfo, OutputCallbackInfo, OutputStreamTimestamp, PauseStreamError,
    PlayStreamError, SampleFormat, SampleRate, StreamConfig, StreamError, StreamInstant,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

#[derive(Default)]
pub struct Devices {
    info: Vec<AudioInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    devnode: String,
    info: Option<AudioInfo>,
}

impl Device {
    fn open_dsp(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
    ) -> Result<illumos_audio::Dsp, BuildStreamError> {
        let dsp = illumos_audio::Dsp::open_path(&self.devnode).map_err(|e| {
            BuildStreamError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("open(\"{}\"): {e}", self.devnode),
                },
            }
        })?;

        let format = match sample_format {
            SampleFormat::I8 => illumos_audio::sys::AudioFormats::AFMT_S8,
            SampleFormat::U8 => illumos_audio::sys::AudioFormats::AFMT_U8,
            SampleFormat::I16 => illumos_audio::sys::AudioFormats::AFMT_S16_NE,
            SampleFormat::U16 => illumos_audio::sys::AudioFormats::AFMT_U16_NE,
            SampleFormat::I32 => illumos_audio::sys::AudioFormats::AFMT_S32_NE,
            _ => {
                return Err(BuildStreamError::StreamConfigNotSupported);
            }
        };
        dsp.format_set(format)
            .map_err(|e| BuildStreamError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("format_set(\"{}\"): {e}", self.devnode),
                },
            })?;

        dsp.channels_set(config.channels.try_into().unwrap())
            .map_err(|e| BuildStreamError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("channels_set(\"{}\"): {e}", self.devnode),
                },
            })?;

        dsp.speed_set(config.sample_rate.0.try_into().unwrap())
            .map_err(|e| BuildStreamError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("speed_set(\"{}\"): {e}", self.devnode),
                },
            })?;

        Ok(dsp)
    }
}

/// The default illumos host type.
#[derive(Debug)]
pub struct Host;

//#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[derive(Debug)]
pub struct Stream {
    inner: Arc<StreamInner>,
}

struct StreamWorkerContext {
    buffer: Vec<u8>,
}

impl StreamWorkerContext {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }
}

#[derive(Debug)]
struct StreamInner {
    dsp: Dsp,
    sample_format: SampleFormat,
    config: StreamConfig,
    playing: AtomicBool,
}

fn output_stream_worker(
    inner: &StreamInner,
    data_callback: &mut (dyn FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static),
    error_callback: &mut (dyn FnMut(StreamError) + Send + 'static),
    _timeout: Option<Duration>,
) {
    let mut ctxt = StreamWorkerContext::new();

    /*
     * Use hrtime for stream timestamps.
     */
    let start = Instant::now();

    /*
     * Get the available buffer size prior to sending any data.
     */
    let ospc = inner.dsp.space_output().unwrap_or_else(|err| {
        let description = format!("output space error: {err}");
        error_callback(BackendSpecificError { description }.into());
        Default::default()
    });

    /*
     * What is the size of a single sample in bytes?
     */
    let sampsz = (inner.sample_format.sample_size() as u64) * (inner.config.channels as u64);

    /*
     * What is the size of 10msec worth of audio data?
     */
    let sleepdel = 10;
    let sleepsz = inner.config.sample_rate.0 as u64 * sampsz * sleepdel / 1000;

    /*
     * Should we do sleeps to wait for a minimal amount of buffer space?
     */
    let origsz: u64 = ospc.bytes.try_into().unwrap();
    let sleeps = origsz >= 2 * sleepsz;

    loop {
        if !inner.playing.load(Ordering::Relaxed) {
            /*
             * Wait to be told to play before sending more samples to the
             * device.
             */
            std::thread::sleep(Duration::from_millis(sleepdel));
            continue;
        }

        /*
         * How much available space is there in the buffer?
         */
        let ospc = inner.dsp.space_output().unwrap_or_else(|err| {
            let description = format!("output space error: {err}");
            error_callback(BackendSpecificError { description }.into());
            Default::default()
        });
        let cursz: u64 = ospc.bytes.try_into().unwrap();

        if cursz == 0 || sleeps && cursz < 2 * sleepsz {
            /*
             * Wait for more buffer space.
             */
            std::thread::sleep(Duration::from_millis(sleepdel));
            continue;
        }

        /*
         * Check for errors.
         */
        // match inner.dsp.errors() {
        //     Ok(errs) => {
        //         if !errs.is_ok() {
        //             println!("ERRORS: {errs:?}");
        //         }
        //     }
        //     Err(e) => {
        //         println!("ERRORS: COULD NOT GET ERRORS! {e}");
        //     }
        // }

        /*
         * Determine the current output delay.  This is the number of bytes of
         * audio data to be played before any new data we write will be played.
         */
        let delayb = inner.dsp.delay().unwrap_or_else(|err| {
            let description = format!("delay error: {err}");
            error_callback(BackendSpecificError { description }.into());
            0
        });

        /*
         * Turn the output delay in bytes into an output delay in nanoseconds
         * at our given sample rate.
         */
        let delay = (delayb as u64) * 1_000_000_000 / sampsz / (inner.config.sample_rate.0 as u64);

        /*
         * XXX
         */
        ctxt.buffer.resize(ospc.bytes as usize, 0u8);

        {
            let data = ctxt.buffer.as_mut_ptr() as *mut ();
            let len = ctxt.buffer.len() / inner.sample_format.sample_size();
            let mut data = unsafe { Data::from_parts(data, len, inner.sample_format) };

            let callback = StreamInstant::from_nanos_i128(
                Instant::now()
                    .duration_since(start)
                    .as_nanos()
                    .try_into()
                    .unwrap(),
            )
            .unwrap();
            let playback = callback.add(Duration::from_nanos(delay)).unwrap();

            let info = OutputCallbackInfo {
                timestamp: OutputStreamTimestamp { callback, playback },
            };

            data_callback(&mut data, &info);
        }

        match inner.dsp.play(&ctxt.buffer) {
            Ok(()) => (),
            Err(e) => {
                let description = format!("play errror: {e}");
                error_callback(BackendSpecificError { description }.into());
                continue;
            }
        }
    }
}

impl Stream {
    fn new_output<D, E>(
        inner: Arc<StreamInner>,
        mut data_callback: D,
        mut error_callback: E,
        timeout: Option<Duration>,
    ) -> Self
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        /*
         * Start a thread to shovel data out to the audio device.
         */
        let inner0 = Arc::clone(&inner);
        thread::Builder::new()
            .name("cpal_audio_out".to_owned())
            .spawn(move || {
                output_stream_worker(&inner0, &mut data_callback, &mut error_callback, timeout);
            })
            .unwrap();

        Self { inner }
    }
}

pub struct SupportedInputConfigs;
pub struct SupportedOutputConfigs {
    configs: Vec<SupportedStreamConfigRange>,
}

impl Host {
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Host)
    }
}

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        let mixer = illumos_audio::Mixer::open().map_err(|e| DevicesError::BackendSpecific {
            err: BackendSpecificError {
                description: format!("open(\"/dev/mixer\"): {e}"),
            },
        })?;

        let mut info = Vec::new();
        let si = mixer.sysinfo().map_err(|e| DevicesError::BackendSpecific {
            err: BackendSpecificError {
                description: format!("SNDCTL_SYSINFO: {e}"),
            },
        })?;
        for idx in 0..si.num_audios {
            info.push(
                mixer
                    .audioinfo(idx)
                    .map_err(|e| DevicesError::BackendSpecific {
                        err: BackendSpecificError {
                            description: format!("SNDCTL_AUDIOINFO {idx}: {e}"),
                        },
                    })?,
            );
        }

        Ok(Devices { info })
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    #[inline]
    fn name(&self) -> Result<String, DeviceNameError> {
        Ok(self
            .info
            .as_ref()
            /*
             * XXX We should ask the card right now, using SNDCTL_AUDIOINFO,
             * rather than use the cached value here.
             */
            .map(|info| info.name.to_string())
            .unwrap_or_else(|| "default".to_string()))
    }

    #[inline]
    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        Ok(SupportedInputConfigs)
    }

    #[inline]
    fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        let dsp = illumos_audio::Dsp::open_path(&self.devnode).map_err(|e| {
            SupportedStreamConfigsError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("open(\"{}\"): {e}", self.devnode),
                },
            }
        })?;

        let formats = dsp
            .formats()
            .map_err(|e| SupportedStreamConfigsError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("get formats: {e}"),
                },
            })?;

        let mut sf = Vec::new();
        if formats.contains(AudioFormats::AFMT_S32_NE) {
            sf.push(SampleFormat::I32);
        }
        if formats.contains(AudioFormats::AFMT_S16_NE) {
            sf.push(SampleFormat::I16);
        }
        if formats.contains(AudioFormats::AFMT_U16_NE) {
            sf.push(SampleFormat::U16);
        }
        if formats.contains(AudioFormats::AFMT_S8) {
            sf.push(SampleFormat::I8);
        }
        if formats.contains(AudioFormats::AFMT_U8) {
            sf.push(SampleFormat::U8);
        }

        let channels = self
            .info
            .as_ref()
            .map(|info| info.max_channels)
            .unwrap_or(2)
            .try_into()
            .unwrap();
        let min_sample_rate =
            SampleRate(self.info.as_ref().map(|info| info.min_rate).unwrap_or(8000));
        let max_sample_rate = SampleRate(
            self.info
                .as_ref()
                .map(|info| info.max_rate)
                .unwrap_or(48000),
        );

        Ok(SupportedOutputConfigs {
            configs: sf
                .into_iter()
                .map(|sample_format| SupportedStreamConfigRange {
                    channels,
                    min_sample_rate,
                    max_sample_rate,
                    buffer_size: SupportedBufferSize::Unknown,
                    sample_format,
                })
                .collect::<Vec<_>>(),
        })
    }

    #[inline]
    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Err(DefaultStreamConfigError::StreamTypeNotSupported)
    }

    #[inline]
    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let dsp = illumos_audio::Dsp::open_path(&self.devnode).map_err(|e| {
            DefaultStreamConfigError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("open(\"{}\"): {e}", self.devnode),
                },
            }
        })?;

        let channels = dsp
            .channels()
            .map_err(|e| DefaultStreamConfigError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("channels(\"{}\"): {e}", self.devnode),
                },
            })?;

        let speed = dsp
            .speed()
            .map_err(|e| DefaultStreamConfigError::BackendSpecific {
                err: BackendSpecificError {
                    description: format!("speed(\"{}\"): {e}", self.devnode),
                },
            })?;

        Ok(SupportedStreamConfig {
            channels: channels.try_into().unwrap(),
            sample_rate: SampleRate(speed.try_into().unwrap()),
            buffer_size: SupportedBufferSize::Unknown, /* XXX */
            sample_format: SampleFormat::I16, /* XXX */
        })
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        unimplemented!()
    }

    /// Create an output stream.
    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let inner = Arc::new(StreamInner {
            dsp: self.open_dsp(config, sample_format)?,
            config: config.clone(),
            sample_format,
            playing: AtomicBool::new(false),
        });

        Ok(Stream::new_output(
            inner,
            data_callback,
            error_callback,
            timeout,
        ))
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Devices::new()
    }

    fn default_input_device(&self) -> Option<Device> {
        Some(Device {
            devnode: "/dev/dsp".to_string(),
            info: None,
        })
    }

    fn default_output_device(&self) -> Option<Device> {
        Some(Device {
            devnode: "/dev/dsp".to_string(),
            info: None,
        })
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        self.inner.playing.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        self.inner.playing.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Iterator for Devices {
    type Item = Device;

    #[inline]
    fn next(&mut self) -> Option<Device> {
        self.info.pop().map(|info| Device {
            devnode: info.devnode.to_string(),
            info: Some(info),
        })
    }
}

impl Iterator for SupportedInputConfigs {
    type Item = SupportedStreamConfigRange;

    #[inline]
    fn next(&mut self) -> Option<SupportedStreamConfigRange> {
        None
    }
}

impl Iterator for SupportedOutputConfigs {
    type Item = SupportedStreamConfigRange;

    #[inline]
    fn next(&mut self) -> Option<SupportedStreamConfigRange> {
        self.configs.pop()
    }
}
