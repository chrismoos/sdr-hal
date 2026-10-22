use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;

use crate::{
    error::{check_status, Error},
    transmitter::async_metadata::{AsyncMetadata, TransmitAsyncEvent},
    utils::check_equal_buffer_lengths,
    TransmitMetadata,
};

#[derive(Debug)]
pub struct TransmitStreamer<I> {
    handle: uhd_sys::uhd_tx_streamer_handle,

    buffer_pointers: Vec<*const c_void>,
    async_metadata: Option<AsyncMetadata>,
    item_phantom: PhantomData<I>,
}

impl<I> TransmitStreamer<I> {
    /// Creates a transmit streamer with a null streamer handle (for internal use only)
    ///
    /// After creating a streamer with this function, its streamer handle must be initialized.
    pub(crate) fn new() -> Self {
        TransmitStreamer {
            handle: ptr::null_mut(),
            buffer_pointers: Vec::new(),
            async_metadata: None,
            item_phantom: PhantomData,
        }
    }

    pub(crate) fn handle_mut(&mut self) -> &mut uhd_sys::uhd_tx_streamer_handle {
        &mut self.handle
    }
    pub(crate) fn handle(&mut self) -> uhd_sys::uhd_tx_streamer_handle {
        self.handle
    }

    pub fn num_channels(&self) -> Result<usize, Error> {
        let mut num_channels = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_tx_streamer_num_channels(
                self.handle,
                &mut num_channels as *mut usize as *mut _,
            )
        })?;
        Ok(num_channels)
    }

    pub fn transmit(
        &mut self,
        buffers: &mut [&[I]],
        timeout: f64,
    ) -> Result<TransmitMetadata, Error> {
        let mut metadata = TransmitMetadata::new()?;
        let mut samples_transmitted = 0usize;

        if self.buffer_pointers.is_empty() {
            self.buffer_pointers
                .resize(self.num_channels()?, ptr::null_mut());
        }
        assert_eq!(
            buffers.len(),
            self.buffer_pointers.len(),
            "Number of buffers is not equal to this streamer's number of channels"
        );
        let buffer_length = check_equal_buffer_lengths(buffers);

        for (entry, buffer) in self.buffer_pointers.iter_mut().zip(buffers.iter_mut()) {
            *entry = buffer.as_ptr() as *mut c_void;
        }

        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_tx_streamer_send(
                self.handle,
                self.buffer_pointers.as_mut_ptr(),
                buffer_length as _,
                metadata.handle_mut(),
                timeout,
                &mut samples_transmitted as *mut usize as *mut _,
            )
        })?;
        metadata.set_samples(samples_transmitted);

        Ok(metadata)
    }

    pub fn transmit_simple(&mut self, buffer: &mut [I]) -> Result<TransmitMetadata, Error> {
        self.transmit(&mut [buffer], 0.1)
    }

    pub fn send_with_metadata(
        &mut self,
        buffers: &mut [&[I]],
        metadata: &TransmitMetadata,
        timeout: f64,
    ) -> Result<usize, Error> {
        if self.buffer_pointers.is_empty() {
            self.buffer_pointers
                .resize(self.num_channels()?, ptr::null_mut());
        }
        assert_eq!(
            buffers.len(),
            self.buffer_pointers.len(),
            "Number of buffers is not equal to this streamer's number of channels"
        );
        let buffer_length = check_equal_buffer_lengths(buffers);

        for (entry, buffer) in self.buffer_pointers.iter_mut().zip(buffers.iter_mut()) {
            *entry = buffer.as_ptr() as *mut c_void;
        }

        let mut md_handle = metadata.handle();
        let mut samples_transmitted = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_tx_streamer_send(
                self.handle,
                self.buffer_pointers.as_mut_ptr(),
                buffer_length as _,
                &mut md_handle,
                timeout,
                &mut samples_transmitted as *mut usize as *mut _,
            )
        })?;
        Ok(samples_transmitted)
    }

    /// Receive one queued asynchronous TX event. A zero timeout is nonblocking.
    pub fn recv_async_msg(&mut self, timeout: f64) -> Result<Option<TransmitAsyncEvent>, Error> {
        if self.async_metadata.is_none() {
            self.async_metadata = Some(AsyncMetadata::new()?);
        }
        let metadata = self
            .async_metadata
            .as_mut()
            .expect("async metadata initialized above");
        let mut valid = false;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_tx_streamer_recv_async_msg(
                self.handle,
                metadata.handle_mut(),
                timeout,
                &mut valid,
            )
        })?;
        if valid {
            Ok(Some(metadata.event()?))
        } else {
            Ok(None)
        }
    }
}

impl<I> Drop for TransmitStreamer<I> {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_tx_streamer_free(&mut self.handle) };
    }
}

// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl<I> Send for TransmitStreamer<I> {}
// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl<I> Sync for TransmitStreamer<I> {}
