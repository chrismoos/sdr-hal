use std::os::raw::c_char;

use crate::error::{check_status, Error};

const INITIAL_SIZE: usize = 128;
const MAX_SIZE: usize = 1024 * 1024;

pub(crate) fn copy_string<F>(mut operation: F) -> Result<String, Error>
where
    F: FnMut(*mut c_char, usize) -> uhd_sys::uhd_error::Type,
{
    let mut buffer: Vec<u8> = Vec::new();
    for size in BufferSizes::new() {
        buffer.resize(size, b'\0');

        let status = operation(buffer.as_mut_ptr() as *mut c_char, buffer.len());
        check_status(status)?;

        if let Some(null_index) = buffer.iter().position(|b| *b == b'\0') {
            buffer.truncate(null_index);
            buffer.shrink_to_fit();
            return String::from_utf8(buffer).map_err(|_| Error::Utf8);
        } else {
            continue;
        }
    }
    Err(Error::StringLength)
}

pub(crate) fn check_equal_buffer_lengths<I, T>(buffers: &mut [T]) -> usize
where
    T: core::borrow::Borrow<[I]>,
{
    buffers
        .iter()
        .fold(None, |prev_size, buffer| {
            let buffer: &[I] = buffer.borrow();
            match prev_size {
                None => Some(buffer.len()),

                Some(prev_size) => {
                    assert_eq!(prev_size, buffer.len(), "Unequal buffer sizes");
                    Some(prev_size)
                }
            }
        })
        .unwrap_or(0)
}

struct BufferSizes {
    next: usize,
}

impl BufferSizes {
    pub fn new() -> Self {
        BufferSizes { next: INITIAL_SIZE }
    }
}

impl Iterator for BufferSizes {
    type Item = usize;
    fn next(&mut self) -> Option<Self::Item> {
        if self.next > MAX_SIZE {
            None
        } else {
            let current = self.next;
            self.next *= 2;
            Some(current)
        }
    }
}

pub fn alloc_boxed_slice<T: Default + Clone, const LEN: usize>() -> Box<[T; LEN]> {
    use std::convert::TryInto;
    match vec![T::default(); LEN].into_boxed_slice().try_into() {
        Ok(a) => a,
        Err(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_sizes() {
        let mut sizes = BufferSizes::new();
        assert_eq!(Some(128), sizes.next());
        assert_eq!(Some(256), sizes.next());
        assert_eq!(Some(512), sizes.next());
        assert_eq!(Some(1024), sizes.next());
        assert_eq!(Some(2048), sizes.next());
        assert_eq!(Some(4096), sizes.next());
        assert_eq!(Some(8192), sizes.next());
        assert_eq!(Some(16384), sizes.next());
        assert_eq!(Some(32768), sizes.next());
        assert_eq!(Some(65536), sizes.next());
        assert_eq!(Some(131072), sizes.next());
        assert_eq!(Some(262144), sizes.next());
        assert_eq!(Some(524288), sizes.next());
        assert_eq!(Some(1048576), sizes.next());
        assert_eq!(None, sizes.next());
    }
}
