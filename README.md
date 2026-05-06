# dgram

Native bindings for Node's [`dgram`](https://nodejs.org/api/dgram.html) module — UDP datagram sockets — for the [Perry TypeScript-to-native compiler](https://github.com/PerryTS/perry).

Closes [PerryTS/perry#492](https://github.com/PerryTS/perry/issues/492).

## What this is

A Perry "native library" package: a Rust crate (built on top of `tokio::net::UdpSocket`) exporting `extern "C"` symbols that the Perry compiler links into your TypeScript program. From your TypeScript code you `import * as dgram from "dgram"` like any npm package; under the hood every method call resolves to a direct call into the bundled staticlib — no Node addon, no IPC, no JSON marshalling.

This package contains:

- `src/lib.rs` — the Rust crate that wraps `tokio::net::UdpSocket` and exposes `js_dgram_*` `extern "C"` symbols
- `src/index.d.ts` — the TypeScript surface (`dgram` module declaration) Perry resolves at compile time
- `Cargo.toml` — staticlib build config consumed by the Perry linker
- `package.json` — includes the `perry.nativeLibrary` manifest block

## Install

```sh
bun add github:PerryTS/dgram
# or
npm install github:PerryTS/dgram
```

The package's `package.json` declares a `perry.nativeLibrary` block (see the [manifest spec](https://github.com/PerryTS/perry/blob/main/docs/src/native-libraries/manifest-v1.md)) which Perry's compiler reads at link time to discover the staticlib + `extern "C"` symbols. No post-install build step — Perry compiles the Rust crate as part of your project's build.

## Quick start

A UDP echo server. The server binds, awaits each datagram via `recv`, and sends the same bytes back to the originating peer. The client binds an ephemeral port, sends a single message, awaits the reply, and exits.

### Server

```typescript
import * as dgram from "dgram";

const sock = dgram.createSocket("udp4");
await dgram.bind(sock, 8000);
console.log("listening on", dgram.address(sock));

while (true) {
  const { msg, rinfo } = await dgram.recv(sock, 65_536);
  console.log("recv from", rinfo.address, rinfo.port, "->", new TextDecoder().decode(msg));
  await dgram.sendBuffer(sock, msg, rinfo.port, rinfo.address);
}
```

### Client

```typescript
import * as dgram from "dgram";

const sock = dgram.createSocket("udp4");
await dgram.bind(sock, 0); // ephemeral port
await dgram.send(sock, "hello, server!", 8000, "127.0.0.1");

const { msg } = await dgram.recv(sock, 65_536);
console.log("server replied:", new TextDecoder().decode(msg));

await dgram.close(sock);
```

## Why a Promise-based `recv` instead of `socket.on('message', cb)`

Node's idiomatic `dgram` API is event-driven (`socket.on('message', handler)`). External Perry bindings can't register their own event-pump with `perry-stdlib`'s event-loop dispatcher today, so v0.1 exposes `recv(socket, maxBytes): Promise<{ msg, rinfo }>` instead. Most modern code is happier with the await-loop shape anyway, and an event-emitter wrapper is a few lines of TypeScript:

```typescript
import * as dgram from "dgram";
import { EventEmitter } from "events";

export function asEventSocket(s: dgram.SocketHandle, maxBytes = 65_536) {
  const emitter = new EventEmitter();
  let running = true;
  (async () => {
    while (running) {
      try {
        const { msg, rinfo } = await dgram.recv(s, maxBytes);
        emitter.emit("message", msg, rinfo);
      } catch (e) {
        emitter.emit("error", e);
        return;
      }
    }
  })();
  return { emitter, stop: () => { running = false; } };
}
```

A native `on('message', cb)` surface is a v0.2 followup once perry-ffi grows per-wrapper pump registration.

## API reference

### `createSocket(type)`

```typescript
type SocketType = "udp4" | "udp6";

function createSocket(type: SocketType): SocketHandle;
```

Allocate an unbound UDP socket. Synchronous. Returns an opaque branded handle. No I/O happens until `bind` is called.

### `bind(socket, port, address?)`

```typescript
function bind(socket: SocketHandle, port: number, address?: string): Promise<void>;
```

Bind the socket. Pass `0` as the port to let the kernel assign a free port (read it back via `address`). When `address` is omitted the socket binds to `0.0.0.0` (`udp4`) or `::` (`udp6`). Rejects if the socket is already bound or the bind syscall fails (port in use, permission denied, etc).

### `send(socket, msg, port, address)` and `sendBuffer(socket, buffer, port, address)`

```typescript
function send(s: SocketHandle, msg: string, port: number, address: string): Promise<number>;
function sendBuffer(s: SocketHandle, buf: Uint8Array | Buffer, port: number, address: string): Promise<number>;
```

Send a UTF-8 string or arbitrary bytes to a remote endpoint. Resolves with the byte count actually written (`send_to`'s return). For datagrams larger than the path MTU the kernel will either fragment (IPv4) or return EMSGSIZE — at the JS layer that surfaces as a rejection.

### `recv(socket, maxBytes)`

```typescript
interface RemoteInfo {
  address: string;
  family: "IPv4" | "IPv6";
  port: number;
  size: number;
}

interface IncomingMessage {
  msg: Uint8Array;
  rinfo: RemoteInfo;
}

function recv(s: SocketHandle, maxBytes: number): Promise<IncomingMessage>;
```

Await the next incoming datagram. `maxBytes` caps the read buffer (clamped to `[1, 65_536]` internally). Concurrent `recv` calls on the same socket are serialized — packets are delivered in receive order to whichever recv is first in line.

If you're not actively `await`ing `recv`, packets the kernel receives are buffered up to its receive queue and then dropped silently. That's standard UDP semantics.

### `close(socket)`

```typescript
function close(s: SocketHandle): Promise<void>;
```

Drop the socket. The underlying fd closes when no concurrent operation holds a reference; pending `recv` calls reject with a closed-socket error. Idempotent.

### `address(socket)`

```typescript
interface AddressInfo {
  address: string;
  family: "IPv4" | "IPv6";
  port: number;
}

function address(s: SocketHandle): AddressInfo | null;
```

Synchronous read of the local bound address. Returns `null` if the socket isn't bound.

### `setBroadcast(socket, flag)`

```typescript
function setBroadcast(s: SocketHandle, flag: boolean): boolean;
```

Toggle SO_BROADCAST on the underlying fd. Required before sending to broadcast addresses (`255.255.255.255`, subnet broadcasts). Returns `true` on success.

### `addMembership(socket, multicastAddress, interfaceAddress?)` and `dropMembership(...)`

```typescript
function addMembership(s: SocketHandle, multi: string, iface?: string): boolean;
function dropMembership(s: SocketHandle, multi: string, iface?: string): boolean;
```

Join / leave a multicast group. For IPv4, `interfaceAddress` is a textual IPv4 (e.g. `"192.168.1.10"`) — empty string lets the kernel pick. For IPv6, pass a numeric interface index as a string (`"0"` = let the kernel pick); textual interface name resolution is a v0.2 followup.

## Types

```typescript
type SocketHandle = number & { readonly __dgramSocket: unique symbol };
```

`SocketHandle` is an opaque branded number — never inspect it or do arithmetic on it.

## Error handling

Every async function rejects with an `Error` whose message is prefixed by the operation, e.g. `dgram bind: Address already in use (os error 48)`. Common rejection reasons:

- Invalid handle (`dgram <op>: invalid socket handle`) — you passed a handle that was never returned by this library, or one that was already consumed by `close`.
- Already bound (`dgram bind: socket already bound`) — `bind` was called twice on the same handle.
- Not bound (`dgram <send|recv>: socket not bound`) — you have to `bind` before sending or receiving.
- Bind failed — kernel-level errors flow through verbatim.

## Status & roadmap

What's there:

- `createSocket` / `bind` / `send` / `sendBuffer` / `recv` / `close` / `address`
- `setBroadcast` for broadcast clients
- `addMembership` / `dropMembership` for multicast group receivers

Known gaps, tracked in [`PerryTS/perry`](https://github.com/PerryTS/perry):

- Event-driven `socket.on('message', cb)` surface — v0.1 is `await recv()` only; needs per-wrapper pump registration in perry-ffi
- `setMulticastTTL` / `setMulticastInterface` / `setTTL` socket options
- `connect(host, port)` for connected-UDP semantics
- Textual interface-name → IPv6 interface-index resolution for `addMembership`

## Versioning

Pre-1.0. The `perry.nativeLibrary.abiVersion` (currently `0.5`) is a hard pin against Perry's perry-ffi ABI — bump it in lockstep with the Perry release that the bindings target.

## License

MIT — see [LICENSE](./LICENSE).
