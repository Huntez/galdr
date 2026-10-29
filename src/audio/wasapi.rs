//! Low-latency output on Windows. cpal opens devices in plain shared mode and keeps WASAPI's whole
//! ~22 ms buffer full, which with Windows' own buffering puts ~40 ms between the queue and the
//! device. Here the stream uses the shortest period the device allows (IAudioClient3, 2.67 ms on
//! VB-CABLE) and only a couple of periods are kept queued.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, IAudioClient3, IAudioRenderClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    WaitForSingleObject,
};
use windows::core::{GUID, w};

use super::{PcmQueue, Resampler};
use crate::opus::SAMPLE_RATE;

/// Periods kept queued in the device: one being played, one ready. Fewer underruns the moment
/// the thread is late; more is just added delay.
const QUEUED_PERIODS: u32 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// A running output; dropping it stops playback.
pub struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts playing `queue` into the output named `name` (as cpal lists it), or the default one.
/// Fails when the device can't be driven this way, e.g. its mix format isn't float.
pub fn start(name: Option<&str>, queue: Arc<PcmQueue>) -> Result<Output> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, started) = mpsc::channel();
    let flag = stop.clone();
    let name = name.map(str::to_string);
    let thread = thread::Builder::new()
        .name("galdr-output".into())
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            match open(name.as_deref()) {
                Ok(stream) => {
                    let _ = ready.send(Ok(()));
                    if let Err(e) = stream.run(&queue, &flag) {
                        eprintln!("audio: {e:#}");
                    }
                    let _ = CloseHandle(stream.event);
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            }
            CoUninitialize();
        })?;
    started.recv().context("output thread ended")??;
    Ok(Output {
        stop,
        thread: Some(thread),
    })
}

struct Stream {
    client: IAudioClient3,
    render: IAudioRenderClient,
    event: HANDLE,
    channels: usize,
    rate: u32,
    period: u32,
    buffer: u32,
}

unsafe fn open(name: Option<&str>) -> Result<Stream> {
    unsafe {
        let device = find(name)?;
        let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None)?;
        let format = client.GetMixFormat()?;
        let result = init(&client, format);
        CoTaskMemFree(Some(format as *const _ as _));
        let (channels, rate, period) = result?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        Ok(Stream {
            render: client.GetService()?,
            buffer: client.GetBufferSize()?,
            client,
            event,
            channels,
            rate,
            period,
        })
    }
}

/// Opens the stream with the device's shortest shared-mode period; returns (channels, rate, period).
unsafe fn init(client: &IAudioClient3, format: *const WAVEFORMATEX) -> Result<(usize, u32, u32)> {
    unsafe {
        let f = &*format;
        let float = match f.wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT => true,
            WAVE_FORMAT_EXTENSIBLE => {
                let ext = &*(format as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat; // copied out: the struct is packed
                sub == SUBTYPE_IEEE_FLOAT
            }
            _ => false,
        };
        if !float || f.wBitsPerSample != 32 {
            bail!("mix format is not 32-bit float");
        }
        let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
        client.GetSharedModeEnginePeriod(
            format,
            &mut default,
            &mut fundamental,
            &mut min,
            &mut max,
        )?;
        client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, format, None)?;
        Ok((f.nChannels as usize, f.nSamplesPerSec, min))
    }
}

unsafe fn find(name: Option<&str>) -> Result<IMMDevice> {
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let Some(want) = name else {
            return Ok(devices.GetDefaultAudioEndpoint(eRender, eConsole)?);
        };
        let all = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..all.GetCount()? {
            let device = all.Item(i)?;
            let props = device.OpenPropertyStore(STGM_READ)?;
            if props.GetValue(&PKEY_Device_FriendlyName)?.to_string() == want {
                return Ok(device);
            }
        }
        bail!("no output named {want:?}")
    }
}

impl Stream {
    unsafe fn run(&self, queue: &PcmQueue, stop: &AtomicBool) -> Result<()> {
        unsafe {
            // "Pro Audio" scheduling, as audio engines use for their own threads.
            let mut task = 0;
            let mmcss = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task).ok();
            let result = self.pump(queue, stop);
            if let Some(h) = mmcss {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
            result
        }
    }

    unsafe fn pump(&self, queue: &PcmQueue, stop: &AtomicBool) -> Result<()> {
        unsafe {
            let mut resampler = Resampler::new(SAMPLE_RATE, self.rate);
            let mut scratch = Vec::new();
            let target = (self.period * QUEUED_PERIODS).min(self.buffer);
            self.client.Start()?;
            while !stop.load(Ordering::Relaxed) {
                // Timeout: the event stops firing if the device goes away; check `stop` anyway.
                WaitForSingleObject(self.event, 100);
                let queued = self.client.GetCurrentPadding()?;
                let frames = target.saturating_sub(queued);
                if frames == 0 {
                    continue;
                }
                resampler.fill(frames as usize, queue, &mut scratch);
                let data = self.render.GetBuffer(frames)?;
                let out = std::slice::from_raw_parts_mut(
                    data as *mut f32,
                    frames as usize * self.channels,
                );
                for (frame, &v) in out.chunks_mut(self.channels).zip(&scratch) {
                    frame.fill(v);
                }
                self.render.ReleaseBuffer(frames, 0)?;
            }
            self.client.Stop()?;
            Ok(())
        }
    }
}
