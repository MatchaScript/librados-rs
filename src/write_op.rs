use crate::error::{RadosError, Result, check_err};
use crate::ffi;
use crate::ioctx::IoCtx;
use libc::{c_char, c_int, size_t};
use std::ffi::CString;
use std::ptr;
use thiserror::Error;

/// The equality comparison operator for `cmpxattr` and `omap_cmp`, from
/// `enum librados_cmpxattr_op` (`librados.h:102-109`). The other five operators the enum
/// declares are added when a caller needs one.
pub const CMPXATTR_OP_EQ: u8 = ffi::librados_cmpxattr_op::LIBRADOS_CMPXATTR_OP_EQ as u8;

/// Names one `omap_cmp` by the order it was added to the op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmpHandle(usize);

/// A failed `operate_report`, naming the `omap_cmp` that rejected the write when one did.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct WriteError {
    #[source]
    pub error: RadosError,
    pub failed_cmp: Option<CmpHandle>,
}

/// A compound write. Every step copies its input into the op, so no buffer needs to outlive
/// the call that adds it. The exception is the `prval` an `omap_cmp` writes through: those
/// slots are boxed here and stay put until `operate` has run.
pub struct WriteOp {
    op: ffi::rados_write_op_t,
    // Each prval is boxed on purpose: librados holds its address from omap_cmp until operate,
    // and a Vec<c_int> would move them as it grows.
    #[allow(clippy::vec_box)]
    cmps: Vec<Box<c_int>>,
}

impl Drop for WriteOp {
    fn drop(&mut self) {
        // SAFETY: op came from rados_create_write_op and this uniquely-dropped value releases
        // it exactly once.
        unsafe { ffi::rados_release_write_op(self.op) };
    }
}

impl Default for WriteOp {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteOp {
    pub fn new() -> Self {
        // SAFETY: rados_create_write_op takes no arguments and always returns an owned op.
        Self {
            op: unsafe { ffi::rados_create_write_op() },
            cmps: Vec::new(),
        }
    }

    pub fn assert_version(&mut self, ver: u64) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_write_op_assert_version(self.op, ver) };
    }

    pub fn assert_exists(&mut self) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_write_op_assert_exists(self.op) };
    }

    /// `exclusive` picks `LIBRADOS_CREATE_EXCLUSIVE`, which fails an existing object with
    /// `-EEXIST`; otherwise the create is idempotent. No category is set: librados ignores it.
    pub fn create(&mut self, exclusive: bool) {
        let mode = if exclusive {
            ffi::LIBRADOS_CREATE_EXCLUSIVE
        } else {
            ffi::LIBRADOS_CREATE_IDEMPOTENT
        };
        // SAFETY: op is live for the lifetime of self and a null category is accepted.
        unsafe { ffi::rados_write_op_create(self.op, mode as c_int, ptr::null()) };
    }

    pub fn write(&mut self, data: &[u8], offset: u64) {
        // SAFETY: data is readable for data.len() bytes and librados copies it here.
        unsafe {
            ffi::rados_write_op_write(self.op, data.as_ptr().cast(), data.len(), offset);
        }
    }

    pub fn write_full(&mut self, data: &[u8]) {
        // SAFETY: data is readable for data.len() bytes and librados copies it here.
        unsafe { ffi::rados_write_op_write_full(self.op, data.as_ptr().cast(), data.len()) };
    }

    pub fn append(&mut self, data: &[u8]) {
        // SAFETY: data is readable for data.len() bytes and librados copies it here.
        unsafe { ffi::rados_write_op_append(self.op, data.as_ptr().cast(), data.len()) };
    }

    pub fn remove(&mut self) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_write_op_remove(self.op) };
    }

    pub fn truncate(&mut self, offset: u64) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_write_op_truncate(self.op, offset) };
    }

    pub fn omap_set<K: AsRef<[u8]>, V: AsRef<[u8]>>(&mut self, entries: &[(K, V)]) {
        let keys: Vec<*const c_char> = entries
            .iter()
            .map(|(k, _)| k.as_ref().as_ptr().cast())
            .collect();
        let key_lens: Vec<size_t> = entries.iter().map(|(k, _)| k.as_ref().len()).collect();
        let vals: Vec<*const c_char> = entries
            .iter()
            .map(|(_, v)| v.as_ref().as_ptr().cast())
            .collect();
        let val_lens: Vec<size_t> = entries.iter().map(|(_, v)| v.as_ref().len()).collect();
        // SAFETY: the four arrays hold entries.len() elements, each pointer is readable for
        // its recorded length, and librados copies keys and values here.
        unsafe {
            ffi::rados_write_op_omap_set2(
                self.op,
                keys.as_ptr(),
                vals.as_ptr(),
                key_lens.as_ptr(),
                val_lens.as_ptr(),
                entries.len(),
            );
        }
    }

    pub fn omap_rm_keys<K: AsRef<[u8]>>(&mut self, keys: &[K]) {
        let key_ptrs: Vec<*const c_char> =
            keys.iter().map(|k| k.as_ref().as_ptr().cast()).collect();
        let key_lens: Vec<size_t> = keys.iter().map(|k| k.as_ref().len()).collect();
        // SAFETY: both arrays hold keys.len() elements, each pointer is readable for its
        // recorded length, and librados copies the keys here.
        unsafe {
            ffi::rados_write_op_omap_rm_keys2(
                self.op,
                key_ptrs.as_ptr(),
                key_lens.as_ptr(),
                keys.len(),
            );
        }
    }

    pub fn setxattr(&mut self, name: &str, value: &[u8]) -> Result<()> {
        let c_name = CString::new(name)?;
        // SAFETY: c_name is NUL-terminated, value is readable for value.len() bytes, and
        // librados copies both here.
        unsafe {
            ffi::rados_write_op_setxattr(
                self.op,
                c_name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
            );
        }
        Ok(())
    }

    /// `op` is a `librados_cmpxattr_op` value. `rados_write_op_cmpxattr` has no `prval`
    /// out-parameter (`librados.h:2916-2920`), so a failure only shows as `-ECANCELED` on the
    /// whole op; `WriteError::failed_cmp` never names an xattr comparison.
    pub fn cmpxattr(&mut self, name: &str, op: u8, value: &[u8]) -> Result<()> {
        let c_name = CString::new(name)?;
        // SAFETY: c_name is NUL-terminated, value is readable for value.len() bytes, and
        // librados copies both here.
        unsafe {
            ffi::rados_write_op_cmpxattr(
                self.op,
                c_name.as_ptr(),
                op,
                value.as_ptr().cast(),
                value.len(),
            );
        }
        Ok(())
    }

    /// `op` is a `librados_cmpxattr_op` value. A missing key compares as an empty value
    /// (`PrimaryLogPG.cc:8088-8090`), so `CMPXATTR_OP_EQ` against `b""` matches both a missing
    /// key and one stored empty.
    pub fn omap_cmp(&mut self, key: &[u8], op: u8, value: &[u8]) -> CmpHandle {
        // The box goes into self.cmps first and the pointer is taken out of its final home:
        // moving a Box by value into the Vec afterwards would invalidate a pointer derived
        // from it, and librados writes through this one when operate runs. Later pushes only
        // relocate the Vec's buffer, which copies the Box values without retagging them.
        let cmp = CmpHandle(self.cmps.len());
        self.cmps.push(Box::new(0));
        let prval: *mut c_int = &mut *self.cmps[cmp.0];
        // SAFETY: key and value are readable for their recorded lengths and librados copies
        // them here; prval points into the box kept in self.cmps until operate has run.
        unsafe {
            ffi::rados_write_op_omap_cmp2(
                self.op,
                key.as_ptr().cast(),
                op,
                value.as_ptr().cast(),
                key.len(),
                value.len(),
                prval,
            );
        }
        cmp
    }

    /// Removes the keys in `[begin, end)`.
    pub fn omap_rm_range(&mut self, begin: &[u8], end: &[u8]) {
        // SAFETY: both keys are readable for their recorded lengths and librados copies them
        // here.
        unsafe {
            ffi::rados_write_op_omap_rm_range2(
                self.op,
                begin.as_ptr().cast(),
                begin.len(),
                end.as_ptr().cast(),
                end.len(),
            );
        }
    }

    pub fn omap_clear(&mut self) {
        // SAFETY: op is live for the lifetime of self.
        unsafe { ffi::rados_write_op_omap_clear(self.op) };
    }

    pub fn operate(self, io: &IoCtx, oid: &str) -> Result<()> {
        self.operate_report(io, oid).map_err(|e| e.error)
    }

    /// Runs the op and, on failure, names the `omap_cmp` that rejected it. The OSD stops at
    /// the first failing sub-op and leaves the later `rval`s at 0 (`PrimaryLogPG.cc:8379-8386`),
    /// so at most one comparison reports a non-zero `prval` and that one is the failing guard.
    pub fn operate_report(self, io: &IoCtx, oid: &str) -> std::result::Result<(), WriteError> {
        let c_oid = CString::new(oid).map_err(|e| WriteError {
            error: e.into(),
            failed_cmp: None,
        })?;
        // SAFETY: op and the ioctx are live, and c_oid is NUL-terminated for this call.
        let ret = unsafe {
            ffi::rados_write_op_operate(self.op, io.raw(), c_oid.as_ptr(), ptr::null_mut(), 0)
        };
        check_err(ret).map_err(|error| WriteError {
            error,
            failed_cmp: self
                .cmps
                .iter()
                .position(|prval| **prval != 0)
                .map(CmpHandle),
        })
    }
}
