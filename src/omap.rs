use crate::ffi;
use std::collections::{BTreeMap, BTreeSet};
use std::ptr;

/// Key/value pairs returned by an omap read, with `more` set when the OSD truncated the
/// answer at `osd_max_omap_entries_per_request`.
#[derive(Debug)]
pub struct OmapPage {
    pub entries: BTreeMap<Vec<u8>, Vec<u8>>,
    pub more: bool,
}

#[derive(Debug)]
pub struct OmapKeys {
    pub keys: BTreeSet<Vec<u8>>,
    pub more: bool,
}

/// The iterator librados allocates when an omap step is registered on a read op. It is
/// independent of the read op and `rados_omap_get_end` is its only deallocator.
pub(crate) struct OmapIter {
    raw: ffi::rados_omap_iter_t,
}

impl Drop for OmapIter {
    fn drop(&mut self) {
        // SAFETY: raw is null, or the iterator rados_read_op_omap_* stored here and this
        // uniquely-dropped value ends exactly once.
        unsafe { ffi::rados_omap_get_end(self.raw) };
    }
}

impl OmapIter {
    pub(crate) fn empty() -> Self {
        Self {
            raw: ptr::null_mut(),
        }
    }

    pub(crate) fn slot(&mut self) -> *mut ffi::rados_omap_iter_t {
        &mut self.raw
    }

    /// Drains the iterator, copying every key and value out of the memory librados owns.
    pub(crate) fn pairs(&mut self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut pairs = Vec::new();
        loop {
            let mut key: *mut libc::c_char = ptr::null_mut();
            let mut val: *mut libc::c_char = ptr::null_mut();
            let mut key_len: libc::size_t = 0;
            let mut val_len: libc::size_t = 0;
            // SAFETY: raw is a live iterator and the four out slots are valid for the call.
            // The returned pointers stay valid until rados_omap_get_end, which runs in Drop.
            unsafe {
                ffi::rados_omap_get_next2(self.raw, &mut key, &mut val, &mut key_len, &mut val_len);
                if key.is_null() {
                    break;
                }
                let key = std::slice::from_raw_parts(key.cast::<u8>(), key_len).to_vec();
                // An empty value is reported as a null pointer: bufferlist::c_str() returns
                // NULL when the list holds no buffer.
                let val = if val.is_null() {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(val.cast::<u8>(), val_len).to_vec()
                };
                pairs.push((key, val));
            }
        }
        pairs
    }
}
