use crate::conn::RadosHandle;
use crate::error::{check_err, Result};
use crate::ffi;
use crate::omap::{OmapKeys, OmapPage};
use crate::read_op::ReadOp;
use crate::write_op::WriteOp;
use libc::{c_char, c_int, c_void, size_t};
use std::ffi::CString;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::Arc;
use std::time::Duration;

/// `rados_lock_exclusive` flags (`librados.h:3625-3630`).
pub const LOCK_FLAG_MAY_RENEW: u8 = ffi::LIBRADOS_LOCK_FLAG_MAY_RENEW as u8;
pub const LOCK_FLAG_MUST_RENEW: u8 = ffi::LIBRADOS_LOCK_FLAG_MUST_RENEW as u8;

/// Owns the ioctx. `Drop::drop` runs before the fields are dropped, so the cluster handle
/// held here outlives `rados_ioctx_destroy`, as librados requires.
struct IoCtxHandle {
    ioctx: ffi::rados_ioctx_t,
    cluster: Arc<RadosHandle>,
}

impl Drop for IoCtxHandle {
    fn drop(&mut self) {
        // SAFETY: the ioctx came from a successful rados_ioctx_create, its cluster handle is
        // still alive in cluster, and this uniquely-dropped value destroys it exactly once.
        unsafe { ffi::rados_ioctx_destroy(self.ioctx) };
    }
}

// SAFETY: an ioctx carries settings — the write snap context, the read snap id, the locator
// key and the namespace — whose mutation librados.h:218-220 leaves to the caller to
// synchronize. This wrapper exposes no setter for any of them, so every wrapped call leaves
// the ioctx settings unchanged and the handle can be shared between threads. Wrapping one of
// those setters (rados_ioctx_snap_set_read and the rest) invalidates this.
unsafe impl Send for IoCtxHandle {}
unsafe impl Sync for IoCtxHandle {}

#[derive(Clone)]
pub struct IoCtx {
    handle: Arc<IoCtxHandle>,
    pool_name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct ObjectStat {
    pub size: u64,
    pub mtime: i64,
}

impl IoCtx {
    pub(crate) fn new(
        cluster: Arc<RadosHandle>,
        ioctx: ffi::rados_ioctx_t,
        pool_name: String,
    ) -> Self {
        Self {
            handle: Arc::new(IoCtxHandle { ioctx, cluster }),
            pool_name,
        }
    }

    pub fn pool_name(&self) -> &str {
        &self.pool_name
    }

    pub(crate) fn raw(&self) -> ffi::rados_ioctx_t {
        self.handle.ioctx
    }

    pub(crate) fn cluster(&self) -> ffi::rados_t {
        self.handle.cluster.0
    }

    /// The `user_version` of the last operation issued on this ioctx. librados keeps it per
    /// ioctx, so a shared `IoCtx` used from two threads reports whichever op finished last.
    pub fn last_version(&self) -> u64 {
        // SAFETY: the ioctx is live for the lifetime of self.
        unsafe { ffi::rados_get_last_version(self.raw()) }
    }

    pub fn write(&self, oid: &str, data: &[u8], offset: u64) -> Result<()> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and data is readable for data.len() bytes, both for
        // the duration of the call.
        let ret = unsafe {
            ffi::rados_write(
                self.raw(),
                c_oid.as_ptr(),
                data.as_ptr().cast::<c_char>(),
                data.len(),
                offset,
            )
        };
        check_err(ret)
    }

    pub fn write_full(&self, oid: &str, data: &[u8]) -> Result<()> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and data is readable for data.len() bytes, both for
        // the duration of the call.
        let ret = unsafe {
            ffi::rados_write_full(
                self.raw(),
                c_oid.as_ptr(),
                data.as_ptr().cast::<c_char>(),
                data.len(),
            )
        };
        check_err(ret)
    }

    pub fn append(&self, oid: &str, data: &[u8]) -> Result<()> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and data is readable for data.len() bytes, both for
        // the duration of the call.
        let ret = unsafe {
            ffi::rados_append(
                self.raw(),
                c_oid.as_ptr(),
                data.as_ptr().cast::<c_char>(),
                data.len(),
            )
        };
        check_err(ret)
    }

    pub fn read(&self, oid: &str, buf: &mut [u8], offset: u64) -> Result<usize> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and buf is writable for buf.len() bytes, both for
        // the duration of the call.
        let ret = unsafe {
            ffi::rados_read(
                self.raw(),
                c_oid.as_ptr(),
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                offset,
            )
        };
        check_err(ret)?;
        Ok(ret as usize)
    }

    pub fn stat(&self, oid: &str) -> Result<ObjectStat> {
        let c_oid = CString::new(oid)?;
        let mut size: u64 = 0;
        let mut mtime: libc::time_t = 0;
        // SAFETY: c_oid is NUL-terminated and both out slots are valid for the call.
        let ret = unsafe { ffi::rados_stat(self.raw(), c_oid.as_ptr(), &mut size, &mut mtime) };
        check_err(ret)?;
        Ok(ObjectStat {
            size,
            mtime: mtime as i64,
        })
    }

    pub fn truncate(&self, oid: &str, size: u64) -> Result<()> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and alive for this call.
        let ret = unsafe { ffi::rados_trunc(self.raw(), c_oid.as_ptr(), size) };
        check_err(ret)
    }

    pub fn remove(&self, oid: &str) -> Result<()> {
        let c_oid = CString::new(oid)?;
        // SAFETY: c_oid is NUL-terminated and alive for this call.
        let ret = unsafe { ffi::rados_remove(self.raw(), c_oid.as_ptr()) };
        check_err(ret)
    }

    pub fn omap_set<K: AsRef<[u8]>, V: AsRef<[u8]>>(
        &self,
        oid: &str,
        entries: &[(K, V)],
    ) -> Result<()> {
        let mut op = WriteOp::new();
        op.omap_set(entries);
        op.operate(self, oid)
    }

    pub fn omap_rm_keys<K: AsRef<[u8]>>(&self, oid: &str, keys: &[K]) -> Result<()> {
        let mut op = WriteOp::new();
        op.omap_rm_keys(keys);
        op.operate(self, oid)
    }

    pub fn omap_clear(&self, oid: &str) -> Result<()> {
        let mut op = WriteOp::new();
        op.omap_clear();
        op.operate(self, oid)
    }

    pub fn omap_get_vals(
        &self,
        oid: &str,
        start_after: Option<&str>,
        filter_prefix: Option<&str>,
        max_return: u64,
    ) -> Result<OmapPage> {
        let mut op = ReadOp::new();
        let vals = op.omap_get_vals(start_after, filter_prefix, max_return)?;
        op.operate(self, oid)?.take(vals)
    }

    pub fn omap_get_keys(
        &self,
        oid: &str,
        start_after: Option<&str>,
        max_return: u64,
    ) -> Result<OmapKeys> {
        let mut op = ReadOp::new();
        let keys = op.omap_get_keys(start_after, max_return)?;
        op.operate(self, oid)?.take(keys)
    }

    pub fn omap_get_vals_by_keys<K: AsRef<[u8]>>(&self, oid: &str, keys: &[K]) -> Result<OmapPage> {
        let mut op = ReadOp::new();
        let vals = op.omap_get_vals_by_keys(keys);
        op.operate(self, oid)?.take(vals)
    }

    /// `rados_getxattr` reports only `-ERANGE` when the buffer is too small; it never reports
    /// the needed size (`librados_c.cc:1965-1968`). The buffer therefore doubles until the
    /// value fits.
    pub fn getxattr(&self, oid: &str, name: &str) -> Result<Vec<u8>> {
        let c_oid = CString::new(oid)?;
        let c_name = CString::new(name)?;
        let mut buf = vec![0u8; 256];
        loop {
            // SAFETY: both strings are NUL-terminated and buf is writable for buf.len()
            // bytes, all for the duration of the call.
            let ret = unsafe {
                ffi::rados_getxattr(
                    self.raw(),
                    c_oid.as_ptr(),
                    c_name.as_ptr(),
                    buf.as_mut_ptr().cast::<c_char>(),
                    buf.len(),
                )
            };
            if -ret == libc::ERANGE {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            check_err(ret)?;
            buf.truncate(ret as usize);
            return Ok(buf);
        }
    }

    /// Takes the `cls_lock` named `name` on `oid`. `duration` of `None` means no expiry.
    /// `flags` is a mask of `LOCK_FLAG_MAY_RENEW` and `LOCK_FLAG_MUST_RENEW`.
    ///
    /// The errno reaches the caller through `check_err`: another holder gives
    /// `RadosError::Rados(EBUSY)`, the same `(client, cookie)` taken again without a renew
    /// flag gives `AlreadyExists`, and `LOCK_FLAG_MUST_RENEW` on an unheld lock gives
    /// `NotFound` (`cls_lock.cc:145-217`).
    pub fn lock_exclusive(
        &self,
        oid: &str,
        name: &str,
        cookie: &str,
        desc: &str,
        duration: Option<Duration>,
        flags: u8,
    ) -> Result<()> {
        let c_oid = CString::new(oid)?;
        let c_name = CString::new(name)?;
        let c_cookie = CString::new(cookie)?;
        let c_desc = CString::new(desc)?;
        let mut tv = duration.map(|d| ffi::timeval {
            tv_sec: d.as_secs() as ffi::__time_t,
            tv_usec: d.subsec_micros() as ffi::__suseconds_t,
        });
        // SAFETY: the four strings are NUL-terminated and alive for this call, and duration is
        // null or points at the local timeval, which librados only reads.
        let ret = unsafe {
            ffi::rados_lock_exclusive(
                self.raw(),
                c_oid.as_ptr(),
                c_name.as_ptr(),
                c_cookie.as_ptr(),
                c_desc.as_ptr(),
                tv.as_mut().map_or(ptr::null_mut(), |t| t as *mut _),
                flags,
            )
        };
        check_err(ret)
    }

    pub fn unlock(&self, oid: &str, name: &str, cookie: &str) -> Result<()> {
        let c_oid = CString::new(oid)?;
        let c_name = CString::new(name)?;
        let c_cookie = CString::new(cookie)?;
        // SAFETY: the three strings are NUL-terminated and alive for this call.
        let ret = unsafe {
            ffi::rados_unlock(
                self.raw(),
                c_oid.as_ptr(),
                c_name.as_ptr(),
                c_cookie.as_ptr(),
            )
        };
        check_err(ret)
    }

    pub fn break_lock(&self, oid: &str, name: &str, client: &str, cookie: &str) -> Result<()> {
        let c_oid = CString::new(oid)?;
        let c_name = CString::new(name)?;
        let c_client = CString::new(client)?;
        let c_cookie = CString::new(cookie)?;
        // SAFETY: the four strings are NUL-terminated and alive for this call.
        let ret = unsafe {
            ffi::rados_break_lock(
                self.raw(),
                c_oid.as_ptr(),
                c_name.as_ptr(),
                c_client.as_ptr(),
                c_cookie.as_ptr(),
            )
        };
        check_err(ret)
    }

    /// `rados_list_lockers` writes the clients, cookies and addresses as NUL-terminated
    /// strings packed into three caller buffers, sets each `*_len` to the total it needed and
    /// returns `-ERANGE` without writing anything when any buffer was too small
    /// (`librados_c.cc:3535-3552`). On success the return value is the number of lockers and
    /// each `*_len` is the number of bytes used.
    pub fn list_lockers(&self, oid: &str, name: &str) -> Result<Lockers> {
        let c_oid = CString::new(oid)?;
        let c_name = CString::new(name)?;
        let mut sizes = [64usize; 4];
        loop {
            let mut exclusive: c_int = 0;
            let mut tag = vec![0u8; sizes[0]];
            let mut clients = vec![0u8; sizes[1]];
            let mut cookies = vec![0u8; sizes[2]];
            let mut addrs = vec![0u8; sizes[3]];
            let mut used = sizes;
            // SAFETY: the four buffers are writable for the lengths handed alongside them and
            // both strings are NUL-terminated, all for the duration of the call.
            let ret = unsafe {
                ffi::rados_list_lockers(
                    self.raw(),
                    c_oid.as_ptr(),
                    c_name.as_ptr(),
                    &mut exclusive,
                    tag.as_mut_ptr().cast::<c_char>(),
                    &mut used[0],
                    clients.as_mut_ptr().cast::<c_char>(),
                    &mut used[1],
                    cookies.as_mut_ptr().cast::<c_char>(),
                    &mut used[2],
                    addrs.as_mut_ptr().cast::<c_char>(),
                    &mut used[3],
                )
            };
            if -ret == libc::ERANGE as isize {
                sizes = used;
                continue;
            }
            check_err(ret as c_int)?;

            // Each buffer holds `len` bytes ending in the last string's NUL, so dropping
            // that byte leaves exactly one field per locker.
            let strings = |buf: &[u8], len: usize| -> Vec<String> {
                match len {
                    0 => Vec::new(),
                    _ => buf[..len - 1]
                        .split(|&b| b == 0)
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect(),
                }
            };
            return Ok(Lockers {
                exclusive: exclusive != 0,
                tag: String::from_utf8_lossy(&tag[..used[0] - 1]).into_owned(),
                lockers: strings(&clients, used[1])
                    .into_iter()
                    .zip(strings(&cookies, used[2]))
                    .zip(strings(&addrs, used[3]))
                    .map(|((client, cookie), addr)| Locker {
                        client,
                        cookie,
                        addr,
                    })
                    .collect(),
            });
        }
    }

    /// Registers a watch on `oid` with a timeout of 0, which the OSD reads as its own
    /// `osd_client_watch_timeout` (30s by default) rather than as "no timeout"
    /// (`PrimaryLogPG.cc:7214-7216`). librados renews the watch with a periodic ping
    /// (`Objecter.cc:747`), so the watch goes away when the returned `Watch` is dropped, when
    /// the client is blocklisted, or when the client stops reaching the OSD for that long.
    /// The caller re-registers after `on_error`.
    ///
    /// Both callbacks run on a librados thread. `on_notify` returns the bytes to reply with:
    /// the wrapper acks every notification, because a watcher that never acks keeps the
    /// notifier waiting for its whole timeout.
    pub fn watch(
        &self,
        oid: &str,
        on_notify: Box<dyn FnMut(Notification) -> Vec<u8> + Send>,
        on_error: Box<dyn FnMut(i32) + Send>,
    ) -> Result<Watch> {
        let state = Box::into_raw(Box::new(WatchState {
            on_notify,
            on_error,
            io: self.clone(),
            oid: CString::new(oid)?,
        }));
        let mut handle: u64 = 0;
        // SAFETY: the oid is NUL-terminated and alive for this call, and state is a live
        // WatchState that outlives every callback (see Watch::drop).
        let ret = unsafe {
            ffi::rados_watch3(
                self.raw(),
                (*state).oid.as_ptr(),
                &mut handle,
                Some(watch_notify_trampoline),
                Some(watch_error_trampoline),
                0,
                state.cast::<c_void>(),
            )
        };
        if let Err(err) = check_err(ret) {
            // SAFETY: rados_watch3 failed, so it registered no callback and the box is
            // reclaimed here.
            drop(unsafe { Box::from_raw(state) });
            return Err(err);
        }
        Ok(Watch {
            handle,
            io: self.clone(),
            state,
        })
    }

    /// Notifies every watcher of `oid` and waits for their acks or `timeout`.
    ///
    /// `rados_notify2` takes the timeout in milliseconds but divides it by 1000
    /// (`IoCtxImpl.cc:1849-1851`), so it is rounded up to whole seconds here: a sub-second
    /// value would otherwise reach the OSD as 0, which the OSD reads as its own
    /// `osd_default_notify_timeout` (30s by default, `PrimaryLogPG.cc:6838-6839`).
    /// `Duration::ZERO` keeps that meaning of 0 deliberately, except that librados
    /// substitutes the client's `notify_timeout` config value before the op is sent.
    ///
    /// A watcher that does not ack in time makes `rados_notify2` return `-ETIMEDOUT` while
    /// still filling the reply buffer ("we do this regardless of what error code we return",
    /// `IoCtxImpl.cc:71-73`). That case is decoded like any other and returned with
    /// `timeouts` populated, not raised as an error.
    pub fn notify(&self, oid: &str, payload: &[u8], timeout: Duration) -> Result<NotifyResponse> {
        let c_oid = CString::new(oid)?;
        let timeout_ms = timeout.as_millis().div_ceil(1000) as u64 * 1000;
        let mut reply: *mut c_char = ptr::null_mut();
        let mut reply_len: size_t = 0;
        // SAFETY: c_oid is NUL-terminated, payload is readable for payload.len() bytes and
        // librados copies it, and both out slots are valid for the call.
        let ret = unsafe {
            ffi::rados_notify2(
                self.raw(),
                c_oid.as_ptr(),
                payload.as_ptr().cast::<c_char>(),
                payload.len() as c_int,
                timeout_ms,
                &mut reply,
                &mut reply_len,
            )
        };
        if -ret != libc::ETIMEDOUT {
            check_err(ret)?;
        }
        let response = decode_notify_response(reply, reply_len);
        // SAFETY: reply is null or the buffer rados_notify2 allocated for this call.
        unsafe { ffi::rados_buffer_free(reply) };
        response
    }
}

/// The holders of one `cls_lock`, as `rados_list_lockers` reports them.
#[derive(Debug)]
pub struct Lockers {
    pub exclusive: bool,
    pub tag: String,
    pub lockers: Vec<Locker>,
}

#[derive(Debug)]
pub struct Locker {
    pub client: String,
    pub cookie: String,
    /// The `entity_addr_t` string the OSD sees, of the shape `v1:IP:port/nonce`. `cls_lock`
    /// stamps every stored locker address `TYPE_LEGACY` regardless of the messenger the
    /// locking client used (`cls_lock.cc:228-233`), so the prefix is always `v1:`. This is
    /// the form `Rados::blocklist_add` accepts.
    pub addr: String,
}

/// One notification delivered to a watcher.
#[derive(Debug)]
pub struct Notification {
    pub notify_id: u64,
    pub notifier_id: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub struct NotifyResponse {
    pub acks: Vec<NotifyAck>,
    pub timeouts: Vec<NotifyTimeout>,
}

/// One watcher's reply. Despite the name librados gives the field, `notifier_id` is the gid
/// of the watcher that acked: the OSD keys the reply map by the watcher's `(gid, cookie)`
/// (`Watch.cc:172`).
#[derive(Debug)]
pub struct NotifyAck {
    pub notifier_id: u64,
    pub cookie: u64,
    pub payload: Vec<u8>,
}

/// A watcher that did not ack in time, identified the same way as `NotifyAck`.
#[derive(Debug)]
pub struct NotifyTimeout {
    pub notifier_id: u64,
    pub cookie: u64,
}

/// The callbacks and the context the trampolines need, kept behind a stable address for as
/// long as librados may call them.
struct WatchState {
    on_notify: Box<dyn FnMut(Notification) -> Vec<u8> + Send>,
    on_error: Box<dyn FnMut(i32) + Send>,
    io: IoCtx,
    oid: CString,
}

/// A registered watch. Dropping it unregisters the watch and releases the callbacks.
///
/// The `IoCtx` is held here rather than read back out of the `WatchState`, so that `drop` can
/// name the handles `rados_unwatch2` and `rados_watch_flush` need without touching the
/// allocation a running callback still borrows.
pub struct Watch {
    handle: u64,
    io: IoCtx,
    state: *mut WatchState,
}

// SAFETY: the only non-Send field is the pointer to the WatchState, whose contents are Send;
// librados serializes the callbacks on its finisher strand, so the state is never touched by
// two threads at once.
unsafe impl Send for Watch {}

impl Drop for Watch {
    fn drop(&mut self) {
        // SAFETY: rados_unwatch2 cancels the linger op and waits for the OSD
        // (IoCtxImpl.cc:1785-1806), after which Objecter::handle_watch_notify no longer finds
        // the cookie and dispatches nothing new. It does not wait for a callback already
        // queued on the finisher strand, so rados_watch_flush follows: it defers a handler
        // onto that same strand and blocks until it runs (RadosClient.cc:379-386,
        // Objecter.h:2641-2649), which the strand can only do after every earlier callback has
        // returned. Only then is the box created in IoCtx::watch reclaimed, exactly once and
        // with no callback left holding a reference into it.
        unsafe {
            ffi::rados_unwatch2(self.io.raw(), self.handle);
            ffi::rados_watch_flush(self.io.cluster());
            drop(Box::from_raw(self.state));
        }
    }
}

/// The `rados_watchcb2_t` trampoline. It must not unwind into librados, so a panic in
/// `on_notify` is caught and acked with an empty reply.
unsafe extern "C" fn watch_notify_trampoline(
    arg: *mut c_void,
    notify_id: u64,
    handle: u64,
    notifier_id: u64,
    data: *mut c_void,
    data_len: size_t,
) {
    // SAFETY: arg is the WatchState registered with rados_watch3, alive until the Watch is
    // dropped, and librados serializes these calls on its finisher strand. data is readable
    // for data_len bytes for the duration of the call.
    let state = unsafe { &mut *arg.cast::<WatchState>() };
    let payload = if data.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), data_len) }.to_vec()
    };
    let notification = Notification {
        notify_id,
        notifier_id,
        payload,
    };
    let reply =
        catch_unwind(AssertUnwindSafe(|| (state.on_notify)(notification))).unwrap_or_default();
    // SAFETY: the ioctx and oid live in the WatchState, and reply is readable for its length.
    unsafe {
        ffi::rados_notify_ack(
            state.io.raw(),
            state.oid.as_ptr(),
            notify_id,
            handle,
            reply.as_ptr().cast::<c_char>(),
            reply.len() as c_int,
        );
    }
}

/// The `rados_watcherrcb_t` trampoline, with the same no-unwind rule as the notify one.
unsafe extern "C" fn watch_error_trampoline(arg: *mut c_void, _cookie: u64, err: c_int) {
    // SAFETY: as in watch_notify_trampoline.
    let state = unsafe { &mut *arg.cast::<WatchState>() };
    let _ = catch_unwind(AssertUnwindSafe(|| (state.on_error)(err)));
}

/// `rados_decode_notify_response` leaves an array pointer null when its count is zero, and
/// `slice::from_raw_parts` rejects a null pointer even for an empty slice.
///
/// # Safety
///
/// `ptr` is null, or readable for `len` elements for the lifetime of the returned slice.
unsafe fn as_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

fn decode_notify_response(reply: *mut c_char, reply_len: size_t) -> Result<NotifyResponse> {
    if reply.is_null() || reply_len == 0 {
        return Ok(NotifyResponse {
            acks: Vec::new(),
            timeouts: Vec::new(),
        });
    }
    let mut acks: *mut ffi::notify_ack_t = ptr::null_mut();
    let mut nr_acks: size_t = 0;
    let mut timeouts: *mut ffi::notify_timeout_t = ptr::null_mut();
    let mut nr_timeouts: size_t = 0;
    // SAFETY: reply is readable for reply_len bytes and the four out slots are valid.
    let ret = unsafe {
        ffi::rados_decode_notify_response(
            reply,
            reply_len,
            &mut acks,
            &mut nr_acks,
            &mut timeouts,
            &mut nr_timeouts,
        )
    };
    check_err(ret)?;
    // SAFETY: on success the two arrays hold nr_acks and nr_timeouts elements, each ack
    // payload is readable for its recorded length, and everything is copied out before
    // rados_free_notify_response releases them.
    let response = unsafe {
        NotifyResponse {
            acks: as_slice(acks, nr_acks)
                .iter()
                .map(|ack| NotifyAck {
                    notifier_id: ack.notifier_id,
                    cookie: ack.cookie,
                    payload: if ack.payload.is_null() {
                        Vec::new()
                    } else {
                        std::slice::from_raw_parts(
                            ack.payload.cast::<u8>(),
                            ack.payload_len as usize,
                        )
                        .to_vec()
                    },
                })
                .collect(),
            timeouts: as_slice(timeouts, nr_timeouts)
                .iter()
                .map(|t| NotifyTimeout {
                    notifier_id: t.notifier_id,
                    cookie: t.cookie,
                })
                .collect(),
        }
    };
    // SAFETY: both arrays came from rados_decode_notify_response and are freed exactly once.
    unsafe { ffi::rados_free_notify_response(acks, nr_acks, timeouts) };
    Ok(response)
}
