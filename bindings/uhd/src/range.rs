use crate::error::{check_status, Error};
use std::ptr;

#[derive(Clone)]
pub struct Range(uhd_sys::uhd_range_t);

impl Default for Range {
    fn default() -> Self {
        Range(uhd_sys::uhd_range_t {
            start: 0.0,
            stop: 0.0,
            step: 0.0,
        })
    }
}

pub struct MetaRange(uhd_sys::uhd_meta_range_handle);

impl MetaRange {
    pub fn new() -> Result<Self, Error> {
        let mut handle = ptr::null_mut();
        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_meta_range_make(&mut handle) })?;
        Ok(MetaRange(handle))
    }

    pub fn start(&self) -> Result<f64, Error> {
        let mut start = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_meta_range_start(self.0, &mut start) })?;
        Ok(start)
    }

    pub fn stop(&self) -> Result<f64, Error> {
        let mut stop = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_meta_range_stop(self.0, &mut stop) })?;
        Ok(stop)
    }

    pub fn step(&self) -> Result<f64, Error> {
        let mut step = 0.0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_meta_range_step(self.0, &mut step) })?;
        Ok(step)
    }

    pub fn len(&self) -> Result<usize, Error> {
        let mut length = 0usize;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe {
            uhd_sys::uhd_meta_range_size(self.0, &mut length as *mut usize as *mut _)
        })?;
        Ok(length)
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    pub fn get(&self, index: usize) -> Result<Option<Range>, Error> {
        let mut range = Range::default();
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        match check_status(unsafe { uhd_sys::uhd_meta_range_at(self.0, index as _, &mut range.0) })
        {
            Ok(()) => Ok(Some(range)),
            Err(e) => match e {
                Error::StdExcept => Ok(None),
                _ => Err(e),
            },
        }
    }
    pub fn push(&mut self, range: Range) -> Result<(), Error> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        check_status(unsafe { uhd_sys::uhd_meta_range_push_back(self.0, &range.0) })
    }

    pub fn iter(&self) -> Result<Iter<'_>, Error> {
        Ok(Iter {
            range: self,
            next: 0,
            length: self.len()?,
        })
    }

    pub(crate) fn handle(&mut self) -> uhd_sys::uhd_meta_range_handle {
        self.0
    }
}

impl Drop for MetaRange {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_meta_range_free(&mut self.0) };
    }
}

pub struct Iter<'m> {
    range: &'m MetaRange,
    next: usize,
    length: usize,
}

impl Iterator for Iter<'_> {
    type Item = Result<Range, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.length {
            None
        } else {
            let item = match self.range.get(self.next) {
                Ok(Some(range)) => Ok(range),
                Ok(None) => return None,
                Err(e) => Err(e),
            };
            self.next += 1;
            Some(item)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.length - self.next;
        (remaining, Some(remaining))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.length - self.next
    }
}

impl ExactSizeIterator for Iter<'_> {}

mod fmt {
    use super::{MetaRange, Range};
    use std::fmt::{Debug, Formatter, Result};

    impl Debug for MetaRange {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            match self.iter() {
                Ok(iter) => f.debug_list().entries(iter.map(|r| r.ok())).finish(),
                Err(e) => write!(f, "<error querying length: {}>", e),
            }
        }
    }

    impl Debug for Range {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            f.debug_struct("Range")
                .field("start", &self.0.start)
                .field("stop", &self.0.stop)
                .field("step", &self.0.step)
                .finish()
        }
    }
}
