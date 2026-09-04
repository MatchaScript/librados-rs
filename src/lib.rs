pub mod conn;
pub mod error;
pub mod ffi;
pub mod ioctx;
pub mod omap;
pub mod read_op;
pub mod write_op;

pub use conn::Rados;
pub use error::{RadosError, Result};
pub use ioctx::{
    IoCtx, LOCK_FLAG_MAY_RENEW, LOCK_FLAG_MUST_RENEW, Locker, Lockers, Notification, NotifyAck,
    NotifyResponse, NotifyTimeout, ObjectStat, Watch,
};
pub use omap::{OmapKeys, OmapPage};
pub use read_op::{Handle, ReadOp, ReadResults};
pub use write_op::{CMPXATTR_OP_EQ, CmpHandle, WriteError, WriteOp};

/// Helper to get the librados version as (major, minor, extra)
pub fn version() -> (i32, i32, i32) {
    let mut major = 0;
    let mut minor = 0;
    let mut extra = 0;
    unsafe {
        ffi::rados_version(&mut major, &mut minor, &mut extra);
    }
    (major, minor, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        // rados_version reports the librados API version (LIBRADOS_VER_MAJOR, librados.h:43),
        // not the Ceph release: it is 3.0.0 on a host running Ceph 20.2.3.
        let (major, minor, extra) = version();
        assert!(major >= 1, "librados version {major}.{minor}.{extra}");
    }
}
