//! Run against a cluster started by `hack/run-ceph.sh`: `cargo test -- --include-ignored`.

use librados::{CMPXATTR_OP_EQ, IoCtx, LOCK_FLAG_MUST_RENEW, Rados, RadosError, ReadOp, WriteOp};
use std::time::Duration;
use uuid::Uuid;

const CEPH_CONF: &str = "/tmp/ceph/ceph.conf";

fn connect() -> Rados {
    let rados = Rados::with_id("admin").expect("create cluster handle");
    rados.conf_read_file(CEPH_CONF).expect("read ceph.conf");
    rados.connect().expect("connect");
    rados
}

fn temp_pool(rados: &Rados) -> String {
    let name = format!("test_pool_{}", Uuid::new_v4().simple());
    rados.create_pool(&name).expect("create pool");
    name
}

fn with_pool(f: impl FnOnce(&IoCtx)) {
    let rados = connect();
    let pool = temp_pool(&rados);
    let ioctx = rados.create_ioctx(&pool).expect("create ioctx");
    f(&ioctx);
    drop(ioctx);
    rados.delete_pool(&pool).expect("delete pool");
}

#[test]
#[ignore]
fn object_io() {
    with_pool(|ioctx| {
        let cases: &[(&str, &[u8], &[u8])] = &[
            ("empty", b"", b""),
            ("text", b"Hello Ceph RADOS from Rust!", b" and again"),
            ("binary", b"\x00\x01\xff\xfe", b"\x00"),
        ];

        for (oid, body, extra) in cases {
            ioctx.write_full(oid, body).expect("write_full");
            ioctx.append(oid, extra).expect("append");

            let expected: Vec<u8> = [*body, *extra].concat();
            let mut buf = vec![0u8; expected.len() + 8];
            let read = ioctx.read(oid, &mut buf, 0).expect("read");
            assert_eq!(read, expected.len(), "{oid}");
            assert_eq!(&buf[..read], &expected[..], "{oid}");
            assert_eq!(
                ioctx.stat(oid).expect("stat").size,
                expected.len() as u64,
                "{oid}"
            );

            ioctx.truncate(oid, 0).expect("truncate");
            assert_eq!(ioctx.stat(oid).expect("stat").size, 0, "{oid}");

            ioctx.remove(oid).expect("remove");
            let err = ioctx.stat(oid).expect_err("stat of removed object");
            assert!(matches!(err, RadosError::NotFound), "{oid}: {err}");
        }
    });
}

#[test]
#[ignore]
fn omap_round_trip() {
    with_pool(|ioctx| {
        let oid = "omap_round_trip";
        let entries: &[(&[u8], &[u8])] = &[
            (b"plain", b"value"),
            (b"with\0nul", b"nul value"),
            (b"\xff\xfe non utf8", b"\x00\x01\x02"),
            (b"empty value", b""),
        ];
        ioctx.omap_set(oid, entries).expect("omap_set");

        let keys: Vec<&[u8]> = entries.iter().map(|(k, _)| *k).collect();
        let by_keys = ioctx
            .omap_get_vals_by_keys(oid, &keys)
            .expect("get_vals_by_keys");
        assert!(!by_keys.more);
        for (key, val) in entries {
            assert_eq!(by_keys.entries.get(*key).map(Vec::as_slice), Some(*val));
        }

        let page = ioctx.omap_get_vals(oid, None, None, 100).expect("get_vals");
        assert!(!page.more);
        assert_eq!(page.entries.len(), entries.len());
        for (key, val) in entries {
            assert_eq!(page.entries.get(*key).map(Vec::as_slice), Some(*val));
        }

        let listed = ioctx.omap_get_keys(oid, None, 100).expect("get_keys");
        assert!(!listed.more);
        assert_eq!(listed.keys.len(), entries.len());
        for (key, _) in entries {
            assert!(listed.keys.contains(*key), "{key:?}");
        }

        let binary_oid = "binary_omap_cursor";
        ioctx
            .omap_set(binary_oid, &[(b"\x80", b"a"), (b"\x81", b"b")])
            .expect("set binary keys");
        let first = ioctx
            .omap_get_vals(binary_oid, None, None, 1)
            .expect("first binary page");
        let cursor = first.entries.last_key_value().expect("binary cursor").0;
        let second = ioctx
            .omap_get_vals(binary_oid, Some(cursor), None, 1)
            .expect("second binary page");
        assert_eq!(
            second.entries.keys().next().map(Vec::as_slice),
            Some(&b"\x81"[..])
        );

        ioctx
            .omap_rm_keys(oid, &[b"with\0nul"])
            .expect("omap_rm_keys");
        let after = ioctx
            .omap_get_keys(oid, None, 100)
            .expect("get_keys after rm");
        assert_eq!(after.keys.len(), entries.len() - 1);
        assert!(!after.keys.contains(b"with\0nul".as_slice()));

        ioctx.omap_clear(oid).expect("omap_clear");
        assert!(
            ioctx
                .omap_get_keys(oid, None, 100)
                .expect("get_keys after clear")
                .keys
                .is_empty()
        );

        let err = ioctx
            .omap_get_vals("no_such_object", None, None, 100)
            .expect_err("read of absent object");
        assert!(matches!(err, RadosError::NotFound), "{err}");
    });
}

#[test]
#[ignore]
fn omap_page_is_marked_truncated() {
    with_pool(|ioctx| {
        let oid = "omap_pages";
        let total = 1500;
        let keys: Vec<String> = (0..total).map(|i| format!("key{i:05}")).collect();
        for batch in keys.chunks(300) {
            let entries: Vec<(&str, &[u8])> = batch
                .iter()
                .map(|k| (k.as_str(), b"v".as_slice()))
                .collect();
            ioctx.omap_set(oid, &entries).expect("omap_set");
        }

        let first = ioctx
            .omap_get_keys(oid, None, total as u64)
            .expect("get_keys");
        assert!(first.more, "the OSD caps a page below the requested count");
        assert!(
            first.keys.len() < total,
            "{} keys returned",
            first.keys.len()
        );

        let mut seen = first.keys.len();
        let mut last =
            String::from_utf8(first.keys.last().expect("non-empty page").clone()).unwrap();
        loop {
            let page = ioctx
                .omap_get_keys(oid, Some(last.as_bytes()), total as u64)
                .expect("get_keys page");
            seen += page.keys.len();
            if !page.more {
                break;
            }
            last = String::from_utf8(page.keys.last().expect("non-empty page").clone()).unwrap();
        }
        assert_eq!(seen, total);
    });
}

#[test]
#[ignore]
fn compound_ops() {
    with_pool(|ioctx| {
        let oid = "compound";
        let mut write = WriteOp::new();
        write.write_full(b"abcdefgh");
        write.append(b"ijkl");
        write.omap_set(&[(b"ck".as_slice(), b"cv".as_slice())]);
        write.operate(ioctx, oid).expect("write op");

        let mut read = ReadOp::new();
        let data = read.read(0, 64);
        let vals = read
            .omap_get_vals(None, None, 16)
            .expect("stack omap_get_vals");
        let mut results = read.operate(ioctx, oid).expect("read op");
        assert_eq!(results.take(data).expect("take data"), b"abcdefghijkl");
        let page = results.take(vals).expect("take omap");
        assert_eq!(
            page.entries.get(b"ck".as_slice()).map(Vec::as_slice),
            Some(b"cv".as_slice())
        );

        let mut absent = ReadOp::new();
        let _ = absent.read(0, 64);
        let err = absent.operate(ioctx, "no_such_object").unwrap_err();
        assert!(matches!(err, RadosError::NotFound), "{err}");
    });
}

#[test]
#[ignore]
fn pool_enumeration() {
    let rados = connect();
    let pool = temp_pool(&rados);
    assert!(rados.list_pools().expect("list_pools").contains(&pool));
    rados.delete_pool(&pool).expect("delete pool");
    assert!(!rados.list_pools().expect("list_pools").contains(&pool));
}

/// The ioctx keeps the cluster handle alive, so `rados_ioctx_destroy` still runs before
/// `rados_shutdown` when the last `Rados` is dropped first.
#[test]
#[ignore]
fn ioctx_outlives_rados() {
    let rados = connect();
    let pool = temp_pool(&rados);
    let ioctx = rados.create_ioctx(&pool).expect("create ioctx");
    ioctx.write_full("survivor", b"data").expect("write_full");

    drop(rados);

    assert_eq!(ioctx.stat("survivor").expect("stat after shutdown").size, 4);
    ioctx
        .omap_set("survivor", &[(b"k".as_slice(), b"v".as_slice())])
        .expect("omap_set");
    assert_eq!(
        ioctx
            .omap_get_keys("survivor", None, 10)
            .expect("get_keys")
            .keys
            .len(),
        1
    );
    drop(ioctx);

    let cleanup = connect();
    cleanup.delete_pool(&pool).expect("delete pool");
}

/// Reads one omap value, or `None` when the key is absent.
fn omap_value(ioctx: &IoCtx, oid: &str, key: &[u8]) -> Option<Vec<u8>> {
    ioctx
        .omap_get_vals_by_keys(oid, &[key])
        .expect("omap_get_vals_by_keys")
        .entries
        .remove(key)
}

#[test]
#[ignore]
fn omap_cmp_guards_the_write() {
    with_pool(|ioctx| {
        let oid = "omap_cmp";
        ioctx
            .omap_set(
                oid,
                &[
                    (b"epoch".as_slice(), b"1".as_slice()),
                    (b"rev".as_slice(), b"7".as_slice()),
                ],
            )
            .expect("omap_set");

        let mut op = WriteOp::new();
        op.omap_cmp(b"epoch", CMPXATTR_OP_EQ, b"1");
        op.omap_set(&[(b"rev".as_slice(), b"8".as_slice())]);
        op.operate_report(ioctx, oid).expect("matching guard");
        assert_eq!(omap_value(ioctx, oid, b"rev").as_deref(), Some(&b"8"[..]));

        // Two guards, one of them wrong: the write is dropped and the wrong one is named.
        let cases: &[(&str, &[u8], &[u8], usize)] = &[
            ("first fails", b"9", b"8", 0),
            ("second fails", b"1", b"9", 1),
        ];
        for (name, epoch, rev, index) in cases {
            let mut op = WriteOp::new();
            let handles = [
                op.omap_cmp(b"epoch", CMPXATTR_OP_EQ, epoch),
                op.omap_cmp(b"rev", CMPXATTR_OP_EQ, rev),
            ];
            op.omap_set(&[(b"rev".as_slice(), b"99".as_slice())]);
            let err = op.operate_report(ioctx, oid).expect_err(name);
            assert!(
                matches!(err.error, RadosError::Rados(e) if e == libc::ECANCELED),
                "{name}: {}",
                err.error
            );
            assert_eq!(err.failed_cmp, Some(handles[*index]), "{name}");
            assert_eq!(
                omap_value(ioctx, oid, b"rev").as_deref(),
                Some(&b"8"[..]),
                "{name}: the mutation must not have been applied"
            );
        }

        // A missing key compares equal to the empty value and differs from any other.
        let mut op = WriteOp::new();
        op.omap_cmp(b"absent", CMPXATTR_OP_EQ, b"");
        op.omap_set(&[(b"mark".as_slice(), b"set".as_slice())]);
        op.operate_report(ioctx, oid)
            .expect("missing key equals the empty value");
        assert_eq!(
            omap_value(ioctx, oid, b"mark").as_deref(),
            Some(&b"set"[..])
        );

        let mut op = WriteOp::new();
        let guard = op.omap_cmp(b"absent", CMPXATTR_OP_EQ, b"x");
        op.omap_set(&[(b"mark".as_slice(), b"again".as_slice())]);
        let err = op
            .operate_report(ioctx, oid)
            .expect_err("missing key differs from a non-empty value");
        assert_eq!(err.failed_cmp, Some(guard));
        assert_eq!(
            omap_value(ioctx, oid, b"mark").as_deref(),
            Some(&b"set"[..])
        );
    });
}

#[test]
#[ignore]
fn assert_version_pins_the_object() {
    with_pool(|ioctx| {
        let oid = "assert_version";
        ioctx.write_full(oid, b"v1").expect("write_full");
        let version = ioctx.last_version();
        assert!(version > 0, "last_version after a write");

        let mut op = WriteOp::new();
        op.assert_version(version);
        op.write_full(b"v2");
        op.operate(ioctx, oid)
            .expect("write at the current version");
        let version = ioctx.last_version();

        let cases: &[(&str, u64, i32)] = &[
            ("stale", version - 1, libc::ERANGE),
            ("ahead", version + 1, libc::EOVERFLOW),
        ];
        for (name, ver, errno) in cases {
            let mut op = WriteOp::new();
            op.assert_version(*ver);
            op.write_full(b"v3");
            let err = op.operate(ioctx, oid).expect_err(name);
            assert!(
                matches!(err, RadosError::Rados(e) if e == *errno),
                "{name}: {err}"
            );
        }

        let mut buf = vec![0u8; 8];
        let read = ioctx.read(oid, &mut buf, 0).expect("read");
        assert_eq!(&buf[..read], b"v2");

        // A stale assert_version on a read op fails the whole op, so no ReadResults exist to
        // take a step's data from.
        let mut read = ReadOp::new();
        read.assert_version(version - 1);
        let _ = read.read(0, 8);
        let err = read.operate(ioctx, oid).expect_err("stale read");
        assert!(
            matches!(err, RadosError::Rados(e) if e == libc::ERANGE),
            "{err}"
        );
    });
}

#[test]
#[ignore]
fn create_and_assert_exists() {
    with_pool(|ioctx| {
        let oid = "created";
        let mut op = WriteOp::new();
        op.create(true);
        op.write_full(b"first");
        op.operate(ioctx, oid).expect("exclusive create");

        let mut op = WriteOp::new();
        op.create(true);
        op.write_full(b"second");
        let err = op.operate(ioctx, oid).expect_err("second exclusive create");
        assert!(matches!(err, RadosError::AlreadyExists), "{err}");

        let mut buf = vec![0u8; 16];
        let read = ioctx.read(oid, &mut buf, 0).expect("read");
        assert_eq!(&buf[..read], b"first");

        let mut op = WriteOp::new();
        op.assert_exists();
        op.write_full(b"x");
        let err = op
            .operate(ioctx, "no_such_object")
            .expect_err("assert_exists on a missing object");
        assert!(matches!(err, RadosError::NotFound), "{err}");
    });
}

#[test]
#[ignore]
fn xattr_round_trip() {
    with_pool(|ioctx| {
        let oid = "xattrs";
        let big = vec![0xab_u8; 4096];
        let cases: &[(&str, &[u8])] = &[
            ("plain", b"value"),
            ("binary", b"\x00\x01\xff\x00"),
            ("empty", b""),
            ("big", &big),
        ];
        for (name, value) in cases {
            let mut op = WriteOp::new();
            op.setxattr(name, value).expect("setxattr");
            op.operate(ioctx, oid).expect("write op");
            assert_eq!(
                &ioctx.getxattr(oid, name).expect("getxattr"),
                value,
                "{name}"
            );
        }

        let mut op = WriteOp::new();
        op.cmpxattr("plain", CMPXATTR_OP_EQ, b"wrong")
            .expect("cmpxattr");
        op.write_full(b"must not land");
        let err = op.operate(ioctx, oid).expect_err("cmpxattr mismatch");
        assert!(
            matches!(err, RadosError::Rados(e) if e == libc::ECANCELED),
            "{err}"
        );
        assert_eq!(ioctx.stat(oid).expect("stat").size, 0);
    });
}

#[test]
#[ignore]
fn omap_rm_range_is_half_open() {
    with_pool(|ioctx| {
        let oid = "rm_range";
        let entries: Vec<(&[u8], &[u8])> = vec![
            (b"a", b"1"),
            (b"b", b"2"),
            (b"c", b"3"),
            (b"d", b"4"),
            (b"e", b"5"),
        ];
        ioctx.omap_set(oid, &entries).expect("omap_set");

        let mut op = WriteOp::new();
        op.omap_rm_range(b"b", b"d");
        op.operate(ioctx, oid).expect("omap_rm_range");

        let keys = ioctx.omap_get_keys(oid, None, 100).expect("omap_get_keys");
        let remaining: Vec<&[u8]> = keys.keys.iter().map(Vec::as_slice).collect();
        assert_eq!(remaining, vec![&b"a"[..], &b"d"[..], &b"e"[..]]);
    });
}

/// A second connection to the same pool, standing in for another client.
fn second_client(ioctx: &IoCtx) -> (Rados, IoCtx) {
    let rados = connect();
    let other = rados.create_ioctx(ioctx.pool_name()).expect("create ioctx");
    (rados, other)
}

#[test]
#[ignore]
fn exclusive_locks() {
    with_pool(|ioctx| {
        let (oid, lease) = ("locked", "lease");
        let ttl = Some(Duration::from_secs(30));
        ioctx
            .lock_exclusive(oid, lease, "cookie-a", "holder A", ttl, 0)
            .expect("acquire");

        let cases: &[(&str, &str, &str, u8, RadosError)] = &[
            (
                "another cookie",
                lease,
                "cookie-b",
                0,
                RadosError::Rados(libc::EBUSY),
            ),
            (
                "same cookie",
                lease,
                "cookie-a",
                0,
                RadosError::AlreadyExists,
            ),
            (
                "renew an unheld lock",
                "other",
                "cookie-a",
                LOCK_FLAG_MUST_RENEW,
                RadosError::NotFound,
            ),
        ];
        for (name, lock, cookie, flags, expected) in cases {
            let err = ioctx
                .lock_exclusive(oid, lock, cookie, "", ttl, *flags)
                .expect_err(name);
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(expected),
                "{name}: {err}"
            );
            if let (RadosError::Rados(got), RadosError::Rados(want)) = (&err, expected) {
                assert_eq!(got, want, "{name}");
            }
        }

        let lockers = ioctx.list_lockers(oid, lease).expect("list_lockers");
        assert!(lockers.exclusive);
        assert_eq!(lockers.lockers.len(), 1);
        let locker = &lockers.lockers[0];
        assert_eq!(locker.cookie, "cookie-a");
        assert!(locker.client.starts_with("client."), "{}", locker.client);
        // The addr is an entity_addr_t string, shaped v1:<host>:<port>/<nonce>. cls_lock
        // sets every stored locker address to TYPE_LEGACY before writing it, whatever
        // messenger the client used (cls_lock.cc:228-233), so the prefix is never v2:. What
        // blocklist_add and `profile simple-rados-client-with-blocklist` require is the
        // `[^/]+/[0-9]+` tail (MonCap.cc:312-321).
        let rest = locker.addr.strip_prefix("v1:").expect(&locker.addr);
        let (host, nonce) = rest.rsplit_once('/').expect(&locker.addr);
        assert!(!host.is_empty() && !host.contains('/'), "{}", locker.addr);
        assert!(
            !nonce.is_empty() && nonce.bytes().all(|b| b.is_ascii_digit()),
            "{}",
            locker.addr
        );

        let (_other_rados, other) = second_client(ioctx);
        other
            .break_lock(oid, lease, &locker.client, "cookie-a")
            .expect("break_lock");
        assert!(
            other
                .list_lockers(oid, lease)
                .expect("list_lockers after break")
                .lockers
                .is_empty()
        );

        ioctx
            .lock_exclusive(oid, lease, "cookie-a", "", ttl, 0)
            .expect("re-acquire");
        ioctx.unlock(oid, lease, "cookie-a").expect("unlock");
        assert!(
            ioctx
                .list_lockers(oid, lease)
                .expect("list_lockers after unlock")
                .lockers
                .is_empty()
        );

        // cls_lock expires the lock on the OSD's clock.
        ioctx
            .lock_exclusive(oid, lease, "cookie-a", "", Some(Duration::from_secs(1)), 0)
            .expect("one second lock");
        std::thread::sleep(Duration::from_secs(2));
        ioctx
            .lock_exclusive(oid, lease, "cookie-b", "", ttl, 0)
            .expect("acquire after expiry");
        ioctx.unlock(oid, lease, "cookie-b").expect("unlock");

        // MUST_RENEW erases the locker and re-inserts it with `now + duration`
        // (cls_lock.cc:195, 224-225), so the renewing call's own duration is what sets the
        // new expiry. Renewing a 30s lock down to 1s therefore expires it: this fails both
        // if the renew is rejected and if its duration never reaches cls_lock.
        ioctx
            .lock_exclusive(oid, lease, "cookie-a", "", ttl, 0)
            .expect("lock to renew");
        ioctx
            .lock_exclusive(
                oid,
                lease,
                "cookie-a",
                "",
                Some(Duration::from_secs(1)),
                LOCK_FLAG_MUST_RENEW,
            )
            .expect("renew");
        std::thread::sleep(Duration::from_secs(2));
        ioctx
            .lock_exclusive(oid, lease, "cookie-b", "", ttl, 0)
            .expect("acquire after the renewed lease expired");
        ioctx
            .unlock(oid, lease, "cookie-b")
            .expect("unlock renewed");

        // list_lockers starts with 64-byte buffers and grows them from the lengths librados
        // reports on -ERANGE. A cookie past that size is what makes it take the retry.
        let long_cookie = "c".repeat(100);
        ioctx
            .lock_exclusive(oid, lease, &long_cookie, "", ttl, 0)
            .expect("lock with a long cookie");
        let lockers = ioctx
            .list_lockers(oid, lease)
            .expect("list_lockers after the retry");
        assert_eq!(lockers.lockers.len(), 1);
        assert_eq!(lockers.lockers[0].cookie, long_cookie);
        ioctx.unlock(oid, lease, &long_cookie).expect("unlock");
    });
}

#[test]
#[ignore]
fn watch_and_notify() {
    use std::sync::{Arc, Mutex};

    with_pool(|ioctx| {
        let oid = "watched";
        ioctx.write_full(oid, b"x").expect("write_full");
        let (_notifier_rados, notifier) = second_client(ioctx);

        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let watch = ioctx
            .watch(
                oid,
                Box::new(move |n| {
                    recorded.lock().expect("lock").push(n.payload);
                    b"ack-1".to_vec()
                }),
                Box::new(|_| {}),
            )
            .expect("watch");

        let response = notifier
            .notify(oid, b"ping", Duration::from_secs(2))
            .expect("notify");
        assert!(response.timeouts.is_empty(), "{response:?}");
        assert_eq!(response.acks.len(), 1, "{response:?}");
        assert_eq!(response.acks[0].payload, b"ack-1");
        assert!(response.acks[0].notifier_id != 0 && response.acks[0].cookie != 0);
        assert_eq!(*seen.lock().expect("lock"), vec![b"ping".to_vec()]);

        let second = ioctx
            .watch(oid, Box::new(|_| b"ack-2".to_vec()), Box::new(|_| {}))
            .expect("second watch");
        let response = notifier
            .notify(oid, b"ping", Duration::from_secs(2))
            .expect("notify two watchers");
        assert!(response.timeouts.is_empty(), "{response:?}");
        let mut payloads: Vec<Vec<u8>> = response.acks.iter().map(|a| a.payload.clone()).collect();
        payloads.sort();
        assert_eq!(payloads, vec![b"ack-1".to_vec(), b"ack-2".to_vec()]);
        drop(second);

        drop(watch);
        let response = notifier
            .notify(oid, b"ping", Duration::from_secs(2))
            .expect("notify with no watchers");
        assert!(response.acks.is_empty(), "{response:?}");
        assert!(response.timeouts.is_empty(), "{response:?}");

        // A watcher that acks after the notify timeout is reported as a timeout.
        let slow = ioctx
            .watch(
                oid,
                Box::new(|_| {
                    std::thread::sleep(Duration::from_secs(4));
                    b"late".to_vec()
                }),
                Box::new(|_| {}),
            )
            .expect("slow watch");
        let response = notifier
            .notify(oid, b"ping", Duration::from_secs(2))
            .expect("notify a slow watcher");
        assert!(response.acks.is_empty(), "{response:?}");
        assert_eq!(response.timeouts.len(), 1, "{response:?}");

        // A sub-second timeout is rounded up to one second. Passed through unrounded it
        // reaches the OSD as 0, which means osd_default_notify_timeout (30s) there.
        let started = std::time::Instant::now();
        let response = notifier
            .notify(oid, b"ping", Duration::from_millis(500))
            .expect("notify with a sub-second timeout");
        assert_eq!(response.timeouts.len(), 1, "{response:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        drop(slow);
    });
}

/// EBLOCKLISTED is 108 (`rados.h:513`); libc has no name for it.
const EBLOCKLISTED: i32 = 108;

#[test]
#[ignore]
fn blocklist_fences_the_old_writer() {
    use std::sync::{Arc, Mutex};

    let keeper = connect();
    let pool = temp_pool(&keeper);
    let watcher = keeper.create_ioctx(&pool).expect("create ioctx B");

    // A gets its own cluster handle: blocklisting it must not disturb the other tests.
    let fenced = connect();
    let fenced_ctx = fenced.create_ioctx(&pool).expect("create ioctx A");
    let (oid, lease) = ("fenced", "lease");
    fenced_ctx.write_full(oid, b"a").expect("A writes");
    fenced_ctx
        .lock_exclusive(oid, lease, "cookie-a", "", Some(Duration::from_secs(60)), 0)
        .expect("A locks");

    let errors: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = errors.clone();
    let watch = fenced_ctx
        .watch(
            oid,
            Box::new(|_| Vec::new()),
            Box::new(move |err| recorded.lock().expect("lock").push(err)),
        )
        .expect("A watches");

    let lockers = watcher.list_lockers(oid, lease).expect("B lists lockers");
    let addr = lockers.lockers[0].addr.clone();
    keeper
        .blocklist_add(&addr, Duration::from_secs(60))
        .expect("B blocklists A");
    keeper
        .wait_for_latest_osdmap()
        .expect("B waits for the new osdmap");

    // What makes the next write fail deterministically is A's own osdmap epoch: the op is
    // stamped with it, the OSD holds an op from a newer epoch until it has that map
    // (OSD.cc:11331-11335), and it then rejects a blocklisted source (PrimaryLogPG.cc:2139-2143).
    // Without this wait A may still be stamping ops with the epoch before the blocklist, and
    // whether they are rejected depends on how fast the OSD picked the new map up.
    fenced
        .wait_for_latest_osdmap()
        .expect("A waits for the new osdmap");
    let err = fenced_ctx.write_full(oid, b"a2").expect_err("A is fenced");
    assert!(
        matches!(err, RadosError::Rados(e) if e == EBLOCKLISTED),
        "{err}"
    );

    // Taking the new osdmap, the OSD drops the watchers of every blocklisted client
    // (check_blocklisted_obc_watchers -> handle_watch_timeout) and sends each one a
    // CEPH_WATCH_EVENT_DISCONNECT (Watch.cc:449-455). Objecter turns that into ENOTCONN and
    // hands it to the error callback (Objecter.cc:1002-1009), which is how a watcher learns
    // it has to re-register.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let err = loop {
        if let Some(err) = errors.lock().expect("lock").first().copied() {
            break err;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "A's on_error was never called"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(err, -libc::ENOTCONN, "watch error");

    drop(watch);
    drop(fenced_ctx);
    drop(fenced);
    keeper.delete_pool(&pool).expect("delete pool");
}
