//! Native bindings for Node `dgram` (UDP datagram sockets) — closes #492.
//!
//! Wraps `tokio::net::UdpSocket` behind a Promise-based surface that
//! mirrors Node's `dgram` module shape: `createSocket`, `bind`,
//! `send` / `sendBuffer`, `recv`, `close`, `address`, plus broadcast
//! and multicast-membership toggles.
//!
//! # Why Promise-only (no `on('message', cb)`)
//!
//! Node's idiomatic dgram API is event-driven (`socket.on('message',
//! handler)`), but external Perry bindings can't register their own
//! `process_pending` pump with `perry-stdlib`'s event-loop dispatcher
//! today. Rather than block this binding on perry-stdlib changes,
//! the MVP exposes `recv(socket, maxBytes): Promise<IncomingMessage>`
//! — users write a `while (true) { const m = await recv(...) }` loop
//! that's idiomatic in modern TS and trivially wraps into an
//! `EventEmitter` if needed. An event-driven surface is a v0.2
//! followup once perry-ffi grows a per-wrapper pump-registration
//! helper.
//!
//! # Status
//!
//! - v0.1.0: createSocket / bind / send / sendBuffer / recv / close
//!   / address / setBroadcast / addMembership / dropMembership.
//!   Covers the IPv4 + IPv6 surface for unicast clients, unicast
//!   servers, and multicast group-receive workloads.
//!
//! # Followups
//!
//! - Event-driven `on('message', cb)` API (needs per-wrapper pump
//!   registration in perry-stdlib).
//! - `setMulticastTTL` / `setMulticastInterface` / `setTTL` socket
//!   options. The `socket2`-based knobs work in tokio via
//!   `UdpSocket::from_std` round-trip; left out of v0.1 to keep the
//!   surface tight.
//! - `connect(host, port)` for connected-UDP semantics (lets the
//!   kernel filter packets to a single peer and enables `send`
//!   without an address).

use perry_ffi::{
    alloc_buffer, alloc_string, build_object_shape, js_object_alloc_with_shape, js_object_set_field,
    read_buffer_bytes, BufferHeader, JsPromise, JsValue, Promise, StringHeader,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::net::UdpSocket;
use tokio::sync::Mutex as TokioMutex;

// ─── Handle storage ──────────────────────────────────────────────────────────
//
// Each socket carries:
// - `socket: Option<Arc<UdpSocket>>` — None until bind succeeds. Cloned
//   into the spawn_blocking closures for send / recv so the underlying
//   fd is shared without locking.
// - `recv_lock: Arc<TokioMutex<()>>` — held across each recv() Promise
//   so concurrent recv awaits on the same socket serialize cleanly.
//   (recv_from is &self on tokio, but two parallel recvs would race and
//   each consume one packet — surprising for users; the lock makes
//   the queue-style behaviour predictable.)
// - `ipv6: bool` — the family chosen at createSocket.

struct SocketState {
    socket: Option<Arc<UdpSocket>>,
    recv_lock: Arc<TokioMutex<()>>,
    ipv6: bool,
}

mod statics {
    use super::*;

    pub fn sockets() -> &'static Mutex<HashMap<i64, SocketState>> {
        static S: OnceLock<Mutex<HashMap<i64, SocketState>>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn next_id() -> &'static Mutex<i64> {
        static N: OnceLock<Mutex<i64>> = OnceLock::new();
        N.get_or_init(|| Mutex::new(1))
    }
}

fn next_id() -> i64 {
    let mut g = statics::next_id().lock().unwrap();
    let id = *g;
    *g += 1;
    id
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Read a string from a perry-runtime `*StringHeader` pointer passed
/// through the FFI boundary as i64. Pointers below 0x1000 are treated
/// as null (cheap sanity gate).
unsafe fn read_str(ptr: *const StringHeader) -> Option<String> {
    if (ptr as usize) < 0x1000 {
        return None;
    }
    let len = (*ptr).byte_len as usize;
    let data_ptr = (ptr as *const u8).add(std::mem::size_of::<StringHeader>());
    let bytes = std::slice::from_raw_parts(data_ptr, len);
    std::str::from_utf8(bytes).ok().map(|s| s.to_string())
}

/// Default bind address for the given family.
fn default_bind_addr(ipv6: bool) -> &'static str {
    if ipv6 {
        "::"
    } else {
        "0.0.0.0"
    }
}

// ─── FFI: createSocket ───────────────────────────────────────────────────────

/// `dgram.createSocket(type)` — register an unbound UDP socket and
/// return its handle. Sync. No I/O yet.
///
/// # Safety
///
/// `type_ptr` must be null or a `StringHeader` allocated by
/// perry-runtime.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_create_socket(type_ptr: *const StringHeader) -> i64 {
    let type_str = read_str(type_ptr).unwrap_or_else(|| "udp4".to_string());
    let ipv6 = type_str == "udp6";
    let id = next_id();
    statics::sockets().lock().unwrap().insert(
        id,
        SocketState {
            socket: None,
            recv_lock: Arc::new(TokioMutex::new(())),
            ipv6,
        },
    );
    id
}

// ─── FFI: bind ───────────────────────────────────────────────────────────────

/// `dgram.bind(socket, port, address?)` — bind the underlying UDP
/// socket. Defaults to `0.0.0.0` (`udp4`) or `::` (`udp6`) when
/// `address` is null/empty. Resolves with `undefined`.
///
/// # Safety
///
/// `addr_ptr` must be null or a `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_bind(
    handle: i64,
    port: f64,
    addr_ptr: *const StringHeader,
) -> *mut Promise {
    let promise = JsPromise::new();
    let raw = promise.as_raw();

    let ipv6 = match statics::sockets().lock().unwrap().get(&handle) {
        Some(s) => {
            if s.socket.is_some() {
                promise.reject_string("dgram bind: socket already bound");
                return raw;
            }
            s.ipv6
        }
        None => {
            promise.reject_string("dgram bind: invalid socket handle");
            return raw;
        }
    };

    let addr = read_str(addr_ptr).unwrap_or_default();
    let bind_addr = if addr.is_empty() {
        default_bind_addr(ipv6).to_string()
    } else {
        addr
    };
    let port_u16 = port as u16;
    // IPv6 literal addresses must be bracketed in `host:port` form.
    let target = if ipv6 && !bind_addr.starts_with('[') && bind_addr.contains(':') {
        format!("[{}]:{}", bind_addr, port_u16)
    } else {
        format!("{}:{}", bind_addr, port_u16)
    };

    perry_ffi::spawn_blocking(move || {
        let result =
            tokio::runtime::Handle::current().block_on(async { UdpSocket::bind(&target).await });
        match result {
            Ok(sock) => {
                let arc = Arc::new(sock);
                if let Some(s) = statics::sockets().lock().unwrap().get_mut(&handle) {
                    s.socket = Some(arc);
                    promise.resolve_undefined();
                } else {
                    promise.reject_string("dgram bind: socket vanished mid-bind");
                }
            }
            Err(e) => promise.reject_string(&format!("dgram bind: {}", e)),
        }
    });
    raw
}

// ─── FFI: send / sendBuffer ──────────────────────────────────────────────────

/// `dgram.send(socket, msg, port, address)` — send a UTF-8 string
/// payload. Resolves with the byte count actually written.
///
/// # Safety
///
/// `msg_ptr` and `addr_ptr` must each be null or a `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_send(
    handle: i64,
    msg_ptr: *const StringHeader,
    port: f64,
    addr_ptr: *const StringHeader,
) -> *mut Promise {
    let bytes = match read_str(msg_ptr) {
        Some(s) => s.into_bytes(),
        None => {
            let p = JsPromise::new();
            let raw = p.as_raw();
            p.reject_string("dgram send: invalid msg");
            return raw;
        }
    };
    send_inner(handle, bytes, port, addr_ptr, "send")
}

/// `dgram.sendBuffer(socket, buf, port, address)` — binary variant
/// of `send`. The bytes are forwarded verbatim, no UTF-8 validation.
///
/// # Safety
///
/// `buf_ptr` must be null or a perry-runtime `BufferHeader`.
/// `addr_ptr` must be null or a `StringHeader`.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_send_buffer(
    handle: i64,
    buf_ptr: *const BufferHeader,
    port: f64,
    addr_ptr: *const StringHeader,
) -> *mut Promise {
    let bytes = match read_buffer_bytes(buf_ptr) {
        Some(b) => b.to_vec(),
        None => {
            let p = JsPromise::new();
            let raw = p.as_raw();
            p.reject_string("dgram sendBuffer: invalid buffer");
            return raw;
        }
    };
    send_inner(handle, bytes, port, addr_ptr, "sendBuffer")
}

/// Shared send body — pulls the socket Arc out of the registry on
/// the calling thread, then dispatches the actual `send_to` onto a
/// blocking-pool thread that drives the tokio runtime.
unsafe fn send_inner(
    handle: i64,
    bytes: Vec<u8>,
    port: f64,
    addr_ptr: *const StringHeader,
    op: &'static str,
) -> *mut Promise {
    let promise = JsPromise::new();
    let raw = promise.as_raw();

    let addr = match read_str(addr_ptr) {
        Some(a) if !a.is_empty() => a,
        _ => {
            promise.reject_string(&format!("dgram {}: invalid address", op));
            return raw;
        }
    };
    let port_u16 = port as u16;
    let dst = if addr.contains(':') && !addr.starts_with('[') {
        format!("[{}]:{}", addr, port_u16)
    } else {
        format!("{}:{}", addr, port_u16)
    };

    let sock = match statics::sockets()
        .lock()
        .unwrap()
        .get(&handle)
        .and_then(|s| s.socket.clone())
    {
        Some(s) => s,
        None => {
            promise.reject_string(&format!("dgram {}: socket not bound", op));
            return raw;
        }
    };

    perry_ffi::spawn_blocking(move || {
        let result = tokio::runtime::Handle::current()
            .block_on(async move { sock.send_to(&bytes, &dst).await });
        match result {
            Ok(n) => promise.resolve(JsValue::from_number(n as f64)),
            Err(e) => promise.reject_string(&format!("dgram {}: {}", op, e)),
        }
    });
    raw
}

// ─── FFI: recv ───────────────────────────────────────────────────────────────

/// `dgram.recv(socket, maxBytes)` — await the next incoming datagram.
/// Resolves with `{ msg: Uint8Array, rinfo: { address, family, port,
/// size } }`. Concurrent recvs on the same socket serialize via an
/// internal tokio mutex.
#[no_mangle]
pub extern "C" fn js_dgram_recv(handle: i64, max_bytes: f64) -> *mut Promise {
    let promise = JsPromise::new();
    let raw = promise.as_raw();

    // Cap maxBytes at 65,536 (UDP datagram practical max). Anything
    // smaller than 1 falls back to 1 — caller asked for a buffer.
    let cap = max_bytes as usize;
    let cap = cap.clamp(1, 65_536);

    let (sock, recv_lock) = {
        let g = statics::sockets().lock().unwrap();
        match g.get(&handle) {
            Some(s) => match &s.socket {
                Some(arc) => (arc.clone(), s.recv_lock.clone()),
                None => {
                    promise.reject_string("dgram recv: socket not bound");
                    return raw;
                }
            },
            None => {
                promise.reject_string("dgram recv: invalid socket handle");
                return raw;
            }
        }
    };

    perry_ffi::spawn_blocking(move || {
        let outcome = tokio::runtime::Handle::current().block_on(async move {
            let _guard = recv_lock.lock().await;
            let mut buf = vec![0u8; cap];
            let res = sock.recv_from(&mut buf).await;
            match res {
                Ok((n, addr)) => Ok((buf[..n].to_vec(), addr)),
                Err(e) => Err(e.to_string()),
            }
        });
        match outcome {
            Ok((bytes, addr)) => {
                let (ip, family, port) = match addr {
                    SocketAddr::V4(v) => (v.ip().to_string(), "IPv4", v.port()),
                    SocketAddr::V6(v) => (v.ip().to_string(), "IPv6", v.port()),
                };
                unsafe {
                    let buf_hdr = alloc_buffer(&bytes);
                    if buf_hdr.is_null() {
                        promise.reject_string("dgram recv: buffer allocation failed");
                        return;
                    }
                    let msg_value = JsValue::from_object_ptr(buf_hdr);

                    let rinfo_keys = ["address", "family", "port", "size"];
                    let (rinfo_packed, rinfo_shape) = build_object_shape(&rinfo_keys);
                    let rinfo = js_object_alloc_with_shape(
                        rinfo_shape,
                        rinfo_keys.len() as u32,
                        rinfo_packed.as_ptr(),
                        rinfo_packed.len() as u32,
                    );
                    js_object_set_field(
                        rinfo,
                        0,
                        JsValue::from_string_ptr(alloc_string(&ip).as_raw()),
                    );
                    js_object_set_field(
                        rinfo,
                        1,
                        JsValue::from_string_ptr(alloc_string(family).as_raw()),
                    );
                    js_object_set_field(rinfo, 2, JsValue::from_number(port as f64));
                    js_object_set_field(rinfo, 3, JsValue::from_number(bytes.len() as f64));

                    let outer_keys = ["msg", "rinfo"];
                    let (outer_packed, outer_shape) = build_object_shape(&outer_keys);
                    let outer = js_object_alloc_with_shape(
                        outer_shape,
                        outer_keys.len() as u32,
                        outer_packed.as_ptr(),
                        outer_packed.len() as u32,
                    );
                    js_object_set_field(outer, 0, msg_value);
                    js_object_set_field(outer, 1, JsValue::from_object_ptr(rinfo));
                    promise.resolve(JsValue::from_object_ptr(outer));
                }
            }
            Err(msg) => promise.reject_string(&format!("dgram recv: {}", msg)),
        }
    });

    raw
}

// ─── FFI: close ──────────────────────────────────────────────────────────────

/// `dgram.close(socket)` — drop the socket from the registry. The
/// underlying fd closes when the last `Arc<UdpSocket>` goes out of
/// scope (which may be after a pending `recv`'s spawn_blocking
/// closure observes the closed fd and errors). Idempotent.
#[no_mangle]
pub extern "C" fn js_dgram_close(handle: i64) -> *mut Promise {
    let promise = JsPromise::new();
    let raw = promise.as_raw();
    let _ = statics::sockets().lock().unwrap().remove(&handle);
    promise.resolve_undefined();
    raw
}

// ─── FFI: address ────────────────────────────────────────────────────────────

/// `dgram.address(socket)` — sync read of the local bound address.
/// Returns `null` if the socket isn't bound.
#[no_mangle]
pub extern "C" fn js_dgram_address(handle: i64) -> JsValue {
    let sock_opt = {
        let g = statics::sockets().lock().unwrap();
        g.get(&handle).and_then(|s| s.socket.clone())
    };
    let sock = match sock_opt {
        Some(s) => s,
        None => return JsValue::NULL,
    };
    let local = match sock.local_addr() {
        Ok(a) => a,
        Err(_) => return JsValue::NULL,
    };
    let (ip, family, port) = match local {
        SocketAddr::V4(v) => (v.ip().to_string(), "IPv4", v.port()),
        SocketAddr::V6(v) => (v.ip().to_string(), "IPv6", v.port()),
    };

    unsafe {
        let keys = ["address", "family", "port"];
        let (packed, shape_id) = build_object_shape(&keys);
        let obj = js_object_alloc_with_shape(
            shape_id,
            keys.len() as u32,
            packed.as_ptr(),
            packed.len() as u32,
        );
        js_object_set_field(
            obj,
            0,
            JsValue::from_string_ptr(alloc_string(&ip).as_raw()),
        );
        js_object_set_field(
            obj,
            1,
            JsValue::from_string_ptr(alloc_string(family).as_raw()),
        );
        js_object_set_field(obj, 2, JsValue::from_number(port as f64));
        JsValue::from_object_ptr(obj)
    }
}

// ─── FFI: setBroadcast ───────────────────────────────────────────────────────

/// `dgram.setBroadcast(socket, flag)` — toggle SO_BROADCAST. Returns
/// `1` on success, `0` if the socket isn't bound or the syscall
/// failed. The codegen-side maps this to a JS boolean.
#[no_mangle]
pub extern "C" fn js_dgram_set_broadcast(handle: i64, flag: f64) -> i64 {
    let sock_opt = {
        let g = statics::sockets().lock().unwrap();
        g.get(&handle).and_then(|s| s.socket.clone())
    };
    let sock = match sock_opt {
        Some(s) => s,
        None => return 0,
    };
    if sock.set_broadcast(flag != 0.0).is_ok() {
        1
    } else {
        0
    }
}

// ─── FFI: addMembership / dropMembership ─────────────────────────────────────

/// `dgram.addMembership(socket, multicastAddress, interfaceAddress?)`
/// — join a multicast group. Empty `interfaceAddress` lets the
/// kernel pick a suitable interface. Returns `1` on success.
///
/// # Safety
///
/// String pointers must be null or valid `StringHeader`s.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_add_membership(
    handle: i64,
    multi_ptr: *const StringHeader,
    iface_ptr: *const StringHeader,
) -> i64 {
    membership_inner(handle, multi_ptr, iface_ptr, true)
}

/// `dgram.dropMembership` — leave a previously-joined multicast
/// group. Mirrors `addMembership`.
///
/// # Safety
///
/// String pointers must be null or valid `StringHeader`s.
#[no_mangle]
pub unsafe extern "C" fn js_dgram_drop_membership(
    handle: i64,
    multi_ptr: *const StringHeader,
    iface_ptr: *const StringHeader,
) -> i64 {
    membership_inner(handle, multi_ptr, iface_ptr, false)
}

unsafe fn membership_inner(
    handle: i64,
    multi_ptr: *const StringHeader,
    iface_ptr: *const StringHeader,
    join: bool,
) -> i64 {
    let multi = match read_str(multi_ptr) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let iface = read_str(iface_ptr).unwrap_or_default();

    let (sock, ipv6) = {
        let g = statics::sockets().lock().unwrap();
        match g.get(&handle) {
            Some(s) => match &s.socket {
                Some(arc) => (arc.clone(), s.ipv6),
                None => return 0,
            },
            None => return 0,
        }
    };

    if ipv6 {
        let multi_addr = match multi.parse::<std::net::Ipv6Addr>() {
            Ok(a) => a,
            Err(_) => return 0,
        };
        // Iface index 0 = let the kernel pick. We don't yet parse
        // textual interface names → indices; v0.1 accepts only "0"
        // (or empty) for IPv6 multicast.
        let iface_idx: u32 = iface.parse().unwrap_or(0);
        let res = if join {
            sock.join_multicast_v6(&multi_addr, iface_idx)
        } else {
            sock.leave_multicast_v6(&multi_addr, iface_idx)
        };
        if res.is_ok() {
            1
        } else {
            0
        }
    } else {
        let multi_addr = match multi.parse::<std::net::Ipv4Addr>() {
            Ok(a) => a,
            Err(_) => return 0,
        };
        let iface_addr = if iface.is_empty() {
            std::net::Ipv4Addr::UNSPECIFIED
        } else {
            match iface.parse() {
                Ok(a) => a,
                Err(_) => return 0,
            }
        };
        let res = if join {
            sock.join_multicast_v4(multi_addr, iface_addr)
        } else {
            sock.leave_multicast_v4(multi_addr, iface_addr)
        };
        if res.is_ok() {
            1
        } else {
            0
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

// Tests deliberately avoid the Promise-returning entry points
// (`js_dgram_close` / `bind` / `send` / `recv`) — `perry-ffi`'s
// `JsPromise::new` is provided by perry-stdlib at final-link time,
// which isn't present in `cargo test`. The sync entry points cover
// enough of the registry / option-parsing logic to catch the easy
// regressions; the meaningful integration coverage comes from
// end-to-end TS smoke against a real `perry` binary.
#[cfg(test)]
mod tests {
    use super::*;

    /// `createSocket` returns a positive handle and registers a slot.
    /// `address` on a never-bound socket returns NULL.
    #[test]
    fn create_socket_returns_positive_handle() {
        let id4 = unsafe { js_dgram_create_socket(std::ptr::null()) };
        assert!(id4 > 0);
        let addr = js_dgram_address(id4);
        assert!(addr.is_null(), "unbound socket address should be NULL");
    }

    /// Two consecutive `createSocket` calls return distinct handles —
    /// no accidental id reuse.
    #[test]
    fn handle_ids_are_unique() {
        let a = unsafe { js_dgram_create_socket(std::ptr::null()) };
        let b = unsafe { js_dgram_create_socket(std::ptr::null()) };
        assert_ne!(a, b);
    }

    /// `setBroadcast` on a never-bound socket returns 0 (no socket fd
    /// to flip). The success-path is covered by integration tests
    /// against a real bound socket.
    #[test]
    fn set_broadcast_unbound_returns_zero() {
        let id = unsafe { js_dgram_create_socket(std::ptr::null()) };
        assert_eq!(js_dgram_set_broadcast(id, 1.0), 0);
    }

    /// `addMembership` on a never-bound socket returns 0.
    #[test]
    fn add_membership_unbound_returns_zero() {
        let id = unsafe { js_dgram_create_socket(std::ptr::null()) };
        let r = unsafe { js_dgram_add_membership(id, std::ptr::null(), std::ptr::null()) };
        assert_eq!(r, 0);
    }

    /// `setBroadcast` on an unknown handle (never registered)
    /// returns 0 without panicking.
    #[test]
    fn set_broadcast_unknown_handle() {
        assert_eq!(js_dgram_set_broadcast(99_999, 1.0), 0);
    }

    /// `address` on an unknown handle returns NULL.
    #[test]
    fn address_unknown_handle_returns_null() {
        let v = js_dgram_address(99_999);
        assert!(v.is_null());
    }

    /// `default_bind_addr` picks the right wildcard for each family.
    #[test]
    fn default_bind_addr_per_family() {
        assert_eq!(default_bind_addr(false), "0.0.0.0");
        assert_eq!(default_bind_addr(true), "::");
    }
}
