use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;

use crate::{
    error::{check_status, Error, Result},
    stream::StreamCommand,
    utils::check_equal_buffer_lengths,
    ReceiveMetadata,
};

#[derive(Debug)]
pub struct ReceiveStreamer<I> {
    handle: uhd_sys::uhd_rx_streamer_handle,
    buffer_pointers: Vec<*mut c_void>,
    item_phantom: PhantomData<I>,
}

impl<I> ReceiveStreamer<I> {
    /// Creates a receive streamer with a null streamer handle (for internal use only)
    ///
    /// After creating a streamer with this function, its streamer handle must be initialized.
    pub(crate) fn new() -> Self {
        ReceiveStreamer {
            handle: ptr::null_mut(),
            buffer_pointers: Vec::new(),
            item_phantom: PhantomData,
        }
    }

    pub(crate) fn handle_mut(&mut self) -> &mut uhd_sys::uhd_rx_streamer_handle {
        &mut self.handle
    }
    pub(crate) fn handle(&mut self) -> uhd_sys::uhd_rx_streamer_handle {
        self.handle
    }

    pub fn send_command(&mut self, command: &StreamCommand) -> Result<(), Error> {
        let command_c = command.as_c_command();
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_rx_streamer_issue_stream_cmd(self.handle, &command_c) })
    }

    pub fn num_channels(&self) -> Result<usize, Error> {
        let mut num_channels = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_rx_streamer_num_channels(
                self.handle,
                &mut num_channels as *mut usize as *mut _,
            )
        })?;
        Ok(num_channels)
    }

    pub fn receive(
        &mut self,
        buffers: &mut [&mut [I]],
        timeout: f64,
        one_packet: bool,
    ) -> Result<ReceiveMetadata, Error> {
        let mut metadata = ReceiveMetadata::new()?;
        let mut samples_received = 0usize;

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
            *entry = buffer.as_mut_ptr() as *mut c_void;
        }

        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_rx_streamer_recv(
                self.handle,
                self.buffer_pointers.as_mut_ptr(),
                buffer_length as _,
                metadata.handle_mut(),
                timeout,
                one_packet,
                &mut samples_received as *mut usize as *mut _,
            )
        })?;
        metadata.set_samples(samples_received);

        Ok(metadata)
    }

    pub fn receive_simple(&mut self, buffer: &mut [I]) -> Result<ReceiveMetadata> {
        self.receive(&mut [buffer], 0.1, false)
    }
}

impl<I> Drop for ReceiveStreamer<I> {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_rx_streamer_free(&mut self.handle) };
    }
}

// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl<I> Send for ReceiveStreamer<I> {}
// SAFETY: The stream is transferred to one owner thread and is not used concurrently.
unsafe impl<I> Sync for ReceiveStreamer<I> {}
