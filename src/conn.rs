use crate::error::{RadosError, Result, check_err};
use crate::ffi;
use crate::ioctx::IoCtx;
use libc::c_char;
use std::ffi::CString;
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct RadosHandle(pub(crate) ffi::rados_t);

impl Drop for RadosHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful rados_create and, being owned by this
        // uniquely-dropped value, is shut down exactly once.
        unsafe { ffi::rados_shutdown(self.0) };
    }
}

// SAFETY: librados serializes access to the cluster handle internally, so it may be used
// from any thread and shared between threads.
unsafe impl Send for RadosHandle {}
unsafe impl Sync for RadosHandle {}

#[derive(Clone)]
pub struct Rados {
    handle: Arc<RadosHandle>,
}

impl Rados {
    pub fn create() -> Result<Self> {
        Self::create_with_id(ptr::null())
    }

    pub fn with_id(id: &str) -> Result<Self> {
        let c_id = CString::new(id)?;
        Self::create_with_id(c_id.as_ptr())
    }

    fn create_with_id(id: *const c_char) -> Result<Self> {
        let mut handle: ffi::rados_t = ptr::null_mut();
        let ret = unsafe { ffi::rados_create(&mut handle, id) };
        check_err(ret)?;
        Ok(Self {
            handle: Arc::new(RadosHandle(handle)),
        })
    }

    pub fn conf_read_file(&self, path: &str) -> Result<()> {
        let c_path = CString::new(path)?;
        let ret = unsafe { ffi::rados_conf_read_file(self.handle.0, c_path.as_ptr()) };
        check_err(ret)
    }

    pub fn conf_set(&self, option: &str, value: &str) -> Result<()> {
        let c_opt = CString::new(option)?;
        let c_val = CString::new(value)?;
        let ret = unsafe { ffi::rados_conf_set(self.handle.0, c_opt.as_ptr(), c_val.as_ptr()) };
        check_err(ret)
    }

    pub fn connect(&self) -> Result<()> {
        let ret = unsafe { ffi::rados_connect(self.handle.0) };
        check_err(ret)
    }

    pub fn create_pool(&self, pool_name: &str) -> Result<()> {
        let c_name = CString::new(pool_name)?;
        let ret = unsafe { ffi::rados_pool_create(self.handle.0, c_name.as_ptr()) };
        check_err(ret)
    }

    pub fn delete_pool(&self, pool_name: &str) -> Result<()> {
        let c_name = CString::new(pool_name)?;
        let ret = unsafe { ffi::rados_pool_delete(self.handle.0, c_name.as_ptr()) };
        check_err(ret)
    }

    /// Pool names, as NUL-separated bytes written into a buffer grown until they all fit.
    /// `rados_pool_list` returns the needed length plus one and writes only what fits.
    pub fn list_pools(&self) -> Result<Vec<String>> {
        let mut buf = vec![0u8; 1024];
        let used = loop {
            let ret =
                unsafe { ffi::rados_pool_list(self.handle.0, buf.as_mut_ptr().cast(), buf.len()) };
            check_err(ret)?;
            let needed = ret as usize - 1;
            if needed <= buf.len() {
                break needed;
            }
            buf.resize(needed, 0);
        };

        Ok(buf[..used]
            .split(|&b| b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect())
    }

    /// Blocklists a client address for `expire`, truncated to whole seconds; zero seconds
    /// leaves the duration to the cluster default. `addr` is an `entity_addr_t` string of the
    /// `<type>:<host>:<port>/<nonce>` shape, which `IoCtx::list_lockers` reports; librados
    /// parses it with `entity_addr_t::parse` and accepts exactly one address
    /// (`RadosClient.cc:781-794`).
    pub fn blocklist_add(&self, addr: &str, expire: Duration) -> Result<()> {
        let mut c_addr = CString::new(addr)?.into_bytes_with_nul();
        let expire_seconds =
            u32::try_from(expire.as_secs()).map_err(|_| RadosError::Rados(libc::EOVERFLOW))?;
        // SAFETY: c_addr is NUL-terminated and alive for this call; rados_blocklist_add takes
        // a mutable pointer but only reads the string (RadosClient.cc:781-794).
        let ret = unsafe {
            ffi::rados_blocklist_add(
                self.handle.0,
                c_addr.as_mut_ptr().cast::<c_char>(),
                expire_seconds,
            )
        };
        check_err(ret)
    }

    pub fn wait_for_latest_osdmap(&self) -> Result<()> {
        // SAFETY: the handle is live for the lifetime of the Arc held by self.
        let ret = unsafe { ffi::rados_wait_for_latest_osdmap(self.handle.0) };
        check_err(ret)
    }

    pub fn create_ioctx(&self, pool_name: &str) -> Result<IoCtx> {
        let c_name = CString::new(pool_name)?;
        let mut ioctx: ffi::rados_ioctx_t = ptr::null_mut();
        let ret = unsafe { ffi::rados_ioctx_create(self.handle.0, c_name.as_ptr(), &mut ioctx) };
        check_err(ret)?;
        Ok(IoCtx::new(
            self.handle.clone(),
            ioctx,
            pool_name.to_string(),
        ))
    }
}
