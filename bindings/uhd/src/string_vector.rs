use std::ffi::CString;
use std::ptr;

use crate::check_status;
use crate::error::Error;
use crate::utils::copy_string;

pub(crate) struct StringVector(uhd_sys::uhd_string_vector_handle);

impl StringVector {
    pub fn new() -> Result<Self, Error> {
        let mut handle: uhd_sys::uhd_string_vector_handle = ptr::null_mut();

        // SAFETY: The output handle storage is live and the input C strings remain borrowed until this synchronous call returns.
        check_status(unsafe { uhd_sys::uhd_string_vector_make(&mut handle) })
            .map(|_| StringVector(handle))
    }

    pub fn len(&self) -> Result<usize, Error> {
        let mut len = 0;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let status = unsafe { uhd_sys::uhd_string_vector_size(self.0, &mut len) };
        check_status(status)?;
        Ok(len)
    }

    #[allow(dead_code)]
    pub fn push(&mut self, value: String) -> Result<(), Error> {
        let value_c = CString::new(value)?;
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let status = unsafe { uhd_sys::uhd_string_vector_push_back(&mut self.0, value_c.as_ptr()) };
        check_status(status)?;
        Ok(())
    }

    pub fn get(&self, index: usize) -> Option<Result<String, Error>> {
        // SAFETY: Native handles and argument buffers remain live for this synchronous FFI call.
        let status = copy_string(|buffer, length| unsafe {
            uhd_sys::uhd_string_vector_at(self.0, index as _, buffer, length as _)
        });
        match status {
            Ok(value) => Some(Ok(value)),
            Err(e) => match e {
                Error::StdExcept => None,
                _ => Some(Err(e)),
            },
        }
    }

    /// Returns an iterator over the items in this vector
    ///
    /// Returns an error if the vector length cannot be queried.
    pub fn iter(&self) -> Result<Iter<'_>, Error> {
        Ok(Iter {
            vector: self,
            next: 0,
            length: self.len()?,
        })
    }

    pub(crate) fn handle_mut(&mut self) -> &mut uhd_sys::uhd_string_vector_handle {
        &mut self.0
    }
}

impl Drop for StringVector {
    fn drop(&mut self) {
        // SAFETY: This wrapper owns the live native handle and frees it once before its storage is dropped.
        let _ = unsafe { uhd_sys::uhd_string_vector_free(&mut self.0) };
    }
}

pub struct Iter<'v> {
    vector: &'v StringVector,
    next: usize,
    length: usize,
}

impl Iterator for Iter<'_> {
    type Item = Result<String, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.length {
            None
        } else {
            let item = self.vector.get(self.next)?;
            self.next += 1;
            Some(item)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = self.length - self.next;
        (size, Some(size))
    }

    fn count(self) -> usize
    where
        Self: Sized,
    {
        self.length - self.next
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl From<StringVector> for Vec<String> {
    fn from(strings: StringVector) -> Self {
        From::from(&strings)
    }
}
impl From<&'_ StringVector> for Vec<String> {
    fn from(strings: &StringVector) -> Self {
        match strings.iter() {
            Ok(iter) => iter.flat_map(|string_or_err| string_or_err.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

mod fmt {
    use super::StringVector;
    use std::fmt::*;

    impl Debug for StringVector {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            match self.iter() {
                Ok(iter) => f
                    .debug_list()
                    .entries(iter.map(|item| item.unwrap_or_else(|_| "<invalid UTF-8>".to_owned())))
                    .finish(),
                Err(e) => write!(f, "<error querying length: {}>", e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_vector_empty() -> Result<(), Box<dyn std::error::Error>> {
        let vector = StringVector::new()?;
        assert!(vector.get(0).is_none());
        Ok(())
    }

    #[test]
    fn string_vector_small() -> Result<(), Box<dyn std::error::Error>> {
        let mut vector = StringVector::new()?;
        assert!(vector.get(0).is_none());
        let value0 = "slithy toves";
        vector.push(value0.to_owned())?;

        assert!(vector.get(0).unwrap().unwrap() == value0);
        Ok(())
    }

    #[test]
    fn string_vector_large() -> Result<(), Box<dyn std::error::Error>> {
        let mut vector = StringVector::new()?;
        assert!(vector.get(0).is_none());
        let value0 = "mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths \
        outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe mome raths outgrabe \
        mome raths outgrabe mome raths outgrabe mome raths outgrabe ";
        vector.push(value0.to_owned())?;
        assert!(vector.get(0).unwrap().unwrap() == value0);
        Ok(())
    }
}
