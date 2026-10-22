use std::mem::MaybeUninit;
use std::sync::Arc;

use num_complex::Complex32;

use crate::device::Device;
use crate::error::{check_lms, Error};

#[derive(Debug, Clone, Default)]
pub struct StreamMeta {
    /// Hardware timestamp in samples.
    pub timestamp: u64,
    /// TX only: wait for HW timestamp before sending.
    pub wait_for_timestamp: bool,
    pub flush_partial_packet: bool,
}

#[derive(Debug, Clone, Default)]
pub struct StreamStatus {
    pub active: bool,
    pub fifo_filled: u32,
    pub fifo_size: u32,
    pub underrun: u32,
    pub overrun: u32,
    pub dropped_packets: u32,
    pub sample_rate: f64,
    pub link_rate: f64,
    pub timestamp: u64,
}

/// A transmit stream. Co-owns the `Arc<Device>` because `LMS_DestroyStream`
/// dereferences the device handle, so the stream must outlive any `LMS_Close`.
pub struct TxStream {
    inner: limesuite_sys::lms_stream_t,
    device: Arc<Device>,
    started: bool,
}

impl TxStream {
    /// Create a new TX stream. Must call start() before sending.
    pub fn new(device: Arc<Device>, channel: u32, fifo_size: u32) -> Result<Self, Error> {
        Self::with_throughput(device, channel, fifo_size, 0.5)
    }

    pub fn with_throughput(
        device: Arc<Device>,
        channel: u32,
        fifo_size: u32,
        throughput_vs_latency: f32,
    ) -> Result<Self, Error> {
        let mut stream =
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { MaybeUninit::<limesuite_sys::lms_stream_t>::zeroed().assume_init() };
        stream.channel = channel;
        stream.fifoSize = fifo_size;
        stream.throughputVsLatency = throughput_vs_latency;
        stream.isTx = true;
        stream.dataFmt = limesuite_sys::lms_stream_t__bindgen_ty_1::LMS_FMT_F32;
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_SetupStream(device.raw(), &mut stream) },
            "LMS_SetupStream(TX)",
        )?;
        Ok(TxStream {
            inner: stream,
            device,
            started: false,
        })
    }

    pub fn start(&mut self) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_StartStream(&mut self.inner) },
            "LMS_StartStream(TX)",
        )?;
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        if self.started {
            check_lms(
                // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
                unsafe { limesuite_sys::LMS_StopStream(&mut self.inner) },
                "LMS_StopStream(TX)",
            )?;
            self.started = false;
        }
        Ok(())
    }

    pub fn send(
        &mut self,
        samples: &[Complex32],
        meta: &StreamMeta,
        timeout_ms: u32,
    ) -> Result<usize, Error> {
        let c_meta = limesuite_sys::lms_stream_meta_t {
            timestamp: meta.timestamp,
            waitForTimestamp: meta.wait_for_timestamp,
            flushPartialPacket: meta.flush_partial_packet,
        };
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let ret = unsafe {
            limesuite_sys::LMS_SendStream(
                &mut self.inner,
                samples.as_ptr() as *const _,
                samples.len(),
                &c_meta,
                timeout_ms,
            )
        };
        if ret < 0 {
            return Err(Error::Lms("LMS_SendStream failed".into()));
        }
        Ok(ret as usize)
    }

    /// Get stream status (includes current HW timestamp).
    pub fn status(&mut self) -> Result<StreamStatus, Error> {
        let mut st =
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { MaybeUninit::<limesuite_sys::lms_stream_status_t>::zeroed().assume_init() };
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_GetStreamStatus(&mut self.inner, &mut st) },
            "LMS_GetStreamStatus(TX)",
        )?;
        Ok(StreamStatus {
            active: st.active,
            fifo_filled: st.fifoFilledCount,
            fifo_size: st.fifoSize,
            underrun: st.underrun,
            overrun: st.overrun,
            dropped_packets: st.droppedPackets,
            sample_rate: st.sampleRate,
            link_rate: st.linkRate,
            timestamp: st.timestamp,
        })
    }
}

impl Drop for TxStream {
    fn drop(&mut self) {
        let _ = self.stop();
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        unsafe { limesuite_sys::LMS_DestroyStream(self.device.raw(), &mut self.inner) };
    }
}

pub struct RxStream {
    inner: limesuite_sys::lms_stream_t,
    device: Arc<Device>,
    started: bool,
}

impl RxStream {
    /// Create a new RX stream. Must call start() before receiving.
    pub fn new(device: Arc<Device>, channel: u32, fifo_size: u32) -> Result<Self, Error> {
        Self::with_throughput(device, channel, fifo_size, 0.5)
    }

    pub fn with_throughput(
        device: Arc<Device>,
        channel: u32,
        fifo_size: u32,
        throughput_vs_latency: f32,
    ) -> Result<Self, Error> {
        let mut stream =
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { MaybeUninit::<limesuite_sys::lms_stream_t>::zeroed().assume_init() };
        stream.channel = channel;
        stream.fifoSize = fifo_size;
        stream.throughputVsLatency = throughput_vs_latency;
        stream.isTx = false;
        stream.dataFmt = limesuite_sys::lms_stream_t__bindgen_ty_1::LMS_FMT_F32;
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_SetupStream(device.raw(), &mut stream) },
            "LMS_SetupStream(RX)",
        )?;
        Ok(RxStream {
            inner: stream,
            device,
            started: false,
        })
    }

    pub fn start(&mut self) -> Result<(), Error> {
        if self.started {
            return Ok(());
        }
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_StartStream(&mut self.inner) },
            "LMS_StartStream(RX)",
        )?;
        self.started = true;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        if self.started {
            check_lms(
                // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
                unsafe { limesuite_sys::LMS_StopStream(&mut self.inner) },
                "LMS_StopStream(RX)",
            )?;
            self.started = false;
        }
        Ok(())
    }

    pub fn recv(
        &mut self,
        buf: &mut [Complex32],
        meta: &mut StreamMeta,
        timeout_ms: u32,
    ) -> Result<usize, Error> {
        let mut c_meta = limesuite_sys::lms_stream_meta_t {
            timestamp: 0,
            waitForTimestamp: false,
            flushPartialPacket: false,
        };
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let ret = unsafe {
            limesuite_sys::LMS_RecvStream(
                &mut self.inner,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                &mut c_meta,
                timeout_ms,
            )
        };
        if ret < 0 {
            return Err(Error::Lms("LMS_RecvStream failed".into()));
        }
        meta.timestamp = c_meta.timestamp;
        Ok(ret as usize)
    }

    /// Get stream status (includes current HW timestamp).
    pub fn status(&mut self) -> Result<StreamStatus, Error> {
        let mut st =
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { MaybeUninit::<limesuite_sys::lms_stream_status_t>::zeroed().assume_init() };
        check_lms(
            // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
            unsafe { limesuite_sys::LMS_GetStreamStatus(&mut self.inner, &mut st) },
            "LMS_GetStreamStatus(RX)",
        )?;
        Ok(StreamStatus {
            active: st.active,
            fifo_filled: st.fifoFilledCount,
            fifo_size: st.fifoSize,
            underrun: st.underrun,
            overrun: st.overrun,
            dropped_packets: st.droppedPackets,
            sample_rate: st.sampleRate,
            link_rate: st.linkRate,
            timestamp: st.timestamp,
        })
    }
}

impl Drop for RxStream {
    fn drop(&mut self) {
        let _ = self.stop();
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        unsafe { limesuite_sys::LMS_DestroyStream(self.device.raw(), &mut self.inner) };
    }
}

// Send/Sync: streams internally use thread-safe FIFO buffers.
// Each stream must only be accessed from one thread (enforced by &mut self).
// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl Send for TxStream {}
// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl Send for RxStream {}
