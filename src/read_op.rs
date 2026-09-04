use crate::error::{check_err, Result};
use crate::ffi;
use crate::ioctx::IoCtx;
use crate::omap::{OmapIter, OmapKeys, OmapPage};
use libc::{c_char, c_int, c_uchar, size_t};
use std::any::Any;
use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::ptr;

/// A step's output slots. librados receives their addresses when the step is added and writes
/// through them during `operate`, so every step is boxed and stays put until then.
trait ReadStep {
    fn prval(&self) -> c_int;
    fn into_result(self: Box<Self>) -> Box<dyn Any>;
}

/// Where a step's result lands in `ReadResults`.
pub struct Handle<T> {
    index: usize,
    _marker: PhantomData<T>,
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

struct ReadStepData {
    buf: Vec<u8>,
    bytes_read: size_t,
    prval: c_int,
}

impl ReadStep for ReadStepData {
    fn prval(&self) -> c_int {
        self.prval
    }

    fn into_result(mut self: Box<Self>) -> Box<dyn Any> {
        self.buf.truncate(self.bytes_read);
        Box::new(self.buf)
    }
}

struct OmapValsStep {
    iter: OmapIter,
    more: c_uchar,
    prval: c_int,
}

impl ReadStep for OmapValsStep {
    fn prval(&self) -> c_int {
        self.prval
    }

    fn into_result(mut self: Box<Self>) -> Box<dyn Any> {
        Box::new(OmapPage {
            entries: self.iter.pairs().into_iter().collect(),
            more: self.more != 0,
        })
    }
}

struct OmapKeysStep {
    iter: OmapIter,
    more: c_uchar,
    prval: c_int,
}

impl ReadStep for OmapKeysStep {
    fn prval(&self) -> c_int {
        self.prval
    }

    fn into_result(mut self: Box<Self>) -> Box<dyn Any> {
        Box::new(OmapKeys {
            keys: self.iter.pairs().into_iter().map(|(k, _)| k).collect(),
            more: self.more != 0,
        })
    }
}

/// A compound read. Each method adds a step and returns the handle its result will arrive
/// under; `operate` runs the whole op and moves the results out.
pub struct ReadOp {
    op: ffi::rados_read_op_t,
    steps: Vec<Box<dyn ReadStep>>,
}

impl Drop for ReadOp {
    fn drop(&mut self) {
        // SAFETY: op came from rados_create_read_op and this uniquely-dropped value releases
        // it exactly once.
        unsafe { ffi::rados_release_read_op(self.op) };
    }
}

impl Default for ReadOp {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadOp {
    pub fn new() -> Self {
        // SAFETY: rados_create_read_op takes no arguments and always returns an owned op.
        Self {
            op: unsafe { ffi::rados_create_read_op() },
            steps: Vec::new(),
        }
    }

    pub fn assert_version(&mut self, ver: u64) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_read_op_assert_version(self.op, ver) };
    }

    fn push<T>(&mut self, step: Box<dyn ReadStep>) -> Handle<T> {
        self.steps.push(step);
        Handle {
            index: self.steps.len() - 1,
            _marker: PhantomData,
        }
    }

    pub fn read(&mut self, offset: u64, len: usize) -> Handle<Vec<u8>> {
        let mut step = Box::new(ReadStepData {
            buf: vec![0u8; len],
            bytes_read: 0,
            prval: 0,
        });
        // SAFETY: the buffer is writable for len bytes and the out slots live in the boxed
        // step, which is kept until operate has run.
        unsafe {
            ffi::rados_read_op_read(
                self.op,
                offset,
                len,
                step.buf.as_mut_ptr().cast(),
                &mut step.bytes_read,
                &mut step.prval,
            );
        }
        self.push(step)
    }

    /// `start_after` and `filter_prefix` reach the OSD as NUL-terminated strings
    /// (`librados_c.cc:4411-4412`), so a key holding a NUL cannot be named here; such a value
    /// returns `RadosError::Nul`. The range excludes `start_after`, and `None` is passed as
    /// `""`, so an entry stored under the empty key is never returned by this call.
    /// `omap_get_vals_by_keys` reads it.
    pub fn omap_get_vals(
        &mut self,
        start_after: Option<&str>,
        filter_prefix: Option<&str>,
        max_return: u64,
    ) -> Result<Handle<OmapPage>> {
        let start = start_after.map(CString::new).transpose()?;
        let prefix = filter_prefix.map(CString::new).transpose()?;
        let mut step = Box::new(OmapValsStep {
            iter: OmapIter::empty(),
            more: 0,
            prval: 0,
        });
        // SAFETY: both strings are NUL-terminated and alive for this call, and the out slots
        // live in the boxed step, which is kept until operate has run.
        unsafe {
            ffi::rados_read_op_omap_get_vals2(
                self.op,
                start.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                prefix.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                max_return,
                step.iter.slot(),
                &mut step.more,
                &mut step.prval,
            );
        }
        Ok(self.push(step))
    }

    /// `start_after` carries the same two limits as `omap_get_vals`: it cannot name a key
    /// holding a NUL, and the empty key never appears in the result.
    pub fn omap_get_keys(
        &mut self,
        start_after: Option<&str>,
        max_return: u64,
    ) -> Result<Handle<OmapKeys>> {
        let start = start_after.map(CString::new).transpose()?;
        let mut step = Box::new(OmapKeysStep {
            iter: OmapIter::empty(),
            more: 0,
            prval: 0,
        });
        // SAFETY: start is NUL-terminated and alive for this call, and the out slots live in
        // the boxed step, which is kept until operate has run.
        unsafe {
            ffi::rados_read_op_omap_get_keys2(
                self.op,
                start.as_ref().map_or(ptr::null(), |s| s.as_ptr()),
                max_return,
                step.iter.slot(),
                &mut step.more,
                &mut step.prval,
            );
        }
        Ok(self.push(step))
    }

    /// The returned page never has `more` set: `rados_read_op_omap_get_vals_by_keys2` has no
    /// pmore out-parameter.
    pub fn omap_get_vals_by_keys<K: AsRef<[u8]>>(&mut self, keys: &[K]) -> Handle<OmapPage> {
        let key_ptrs: Vec<*const c_char> =
            keys.iter().map(|k| k.as_ref().as_ptr().cast()).collect();
        let key_lens: Vec<size_t> = keys.iter().map(|k| k.as_ref().len()).collect();
        let mut step = Box::new(OmapValsStep {
            iter: OmapIter::empty(),
            more: 0,
            prval: 0,
        });
        // SAFETY: both arrays hold keys.len() elements, each pointer is readable for its
        // recorded length and librados copies the keys here; the out slots live in the boxed
        // step, which is kept until operate has run.
        unsafe {
            ffi::rados_read_op_omap_get_vals_by_keys2(
                self.op,
                key_ptrs.as_ptr(),
                keys.len(),
                key_lens.as_ptr(),
                step.iter.slot(),
                &mut step.prval,
            );
        }
        self.push(step)
    }

    pub fn operate(mut self, io: &IoCtx, oid: &str) -> Result<ReadResults> {
        let c_oid = CString::new(oid)?;
        // SAFETY: op and the ioctx are live, and c_oid is NUL-terminated for this call.
        let ret = unsafe { ffi::rados_read_op_operate(self.op, io.raw(), c_oid.as_ptr(), 0) };
        check_err(ret)?;
        let steps = std::mem::take(&mut self.steps);
        Ok(ReadResults {
            results: steps
                .into_iter()
                .map(|step| Some((step.prval(), step.into_result())))
                .collect(),
        })
    }
}

/// The results of a completed read op, in the order the steps were added.
pub struct ReadResults {
    results: Vec<Option<(c_int, Box<dyn Any>)>>,
}

impl fmt::Debug for ReadResults {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for slot in &self.results {
            match slot {
                Some((prval, _)) => list.entry(&format_args!("prval {prval}")),
                None => list.entry(&format_args!("taken")),
            };
        }
        list.finish()
    }
}

impl ReadResults {
    /// The OSD stops at the first failing sub-operation, so a step that did not run reports
    /// `prval` 0 and yields its empty result.
    pub fn take<T: 'static>(&mut self, handle: Handle<T>) -> Result<T> {
        let (prval, value) = self.results[handle.index]
            .take()
            .expect("result already taken");
        check_err(prval)?;
        Ok(*value.downcast::<T>().expect("handle names its own step"))
    }
}
