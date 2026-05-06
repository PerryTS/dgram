declare module "dgram" {
  /** Opaque branded handle returned by `createSocket`. */
  export type SocketHandle = number & { readonly __dgramSocket: unique symbol };

  /** UDP socket address family. */
  export type SocketType = "udp4" | "udp6";

  /**
   * Local address info returned by `address`.
   */
  export interface AddressInfo {
    address: string;
    family: "IPv4" | "IPv6";
    port: number;
  }

  /**
   * Remote-message metadata returned alongside `recv`'s payload.
   */
  export interface RemoteInfo {
    address: string;
    family: "IPv4" | "IPv6";
    port: number;
    size: number;
  }

  /**
   * The shape `recv` resolves with: the payload bytes plus the
   * peer's address.
   */
  export interface IncomingMessage {
    msg: Uint8Array;
    rinfo: RemoteInfo;
  }

  /**
   * Create an unbound UDP socket of the given family.
   * Synchronous — returns an opaque handle.
   */
  export function createSocket(type: SocketType): SocketHandle;

  /**
   * Bind the socket to a port and (optional) local address.
   * Defaults to `0.0.0.0` for `udp4`, `::` for `udp6`.
   * Pass port 0 to let the kernel assign a free port (read it
   * back via `address`). Resolves once the bind completes.
   */
  export function bind(
    socket: SocketHandle,
    port: number,
    address?: string,
  ): Promise<void>;

  /**
   * Send a UTF-8 string payload to a remote endpoint. Resolves
   * with the number of bytes written.
   */
  export function send(
    socket: SocketHandle,
    msg: string,
    port: number,
    address: string,
  ): Promise<number>;

  /**
   * Binary-safe variant of `send`. Bytes are sent verbatim.
   */
  export function sendBuffer(
    socket: SocketHandle,
    buffer: Uint8Array | Buffer,
    port: number,
    address: string,
  ): Promise<number>;

  /**
   * Await the next incoming datagram. The Promise resolves with
   * `{ msg, rinfo }` once a packet arrives. `maxBytes` caps the
   * read buffer (bytes beyond the cap are silently truncated by
   * the kernel — UDP semantics).
   *
   * Concurrent `recv` calls on the same socket are serialized
   * internally; if you want parallel readers, use multiple sockets.
   *
   * If you're not actively `await`ing `recv`, packets the kernel
   * receives are buffered up to its receive queue and then
   * dropped. That's standard UDP behavior — there is no guaranteed
   * delivery.
   */
  export function recv(
    socket: SocketHandle,
    maxBytes: number,
  ): Promise<IncomingMessage>;

  /**
   * Close the socket. Pending `recv` calls reject with a closed-
   * socket error. Idempotent.
   */
  export function close(socket: SocketHandle): Promise<void>;

  /**
   * Read the local socket's bound address. Returns `null` if the
   * socket hasn't been bound yet.
   */
  export function address(socket: SocketHandle): AddressInfo | null;

  /**
   * Toggle the SO_BROADCAST flag on the underlying socket. Returns
   * `true` on success.
   */
  export function setBroadcast(
    socket: SocketHandle,
    flag: boolean,
  ): boolean;

  /**
   * Join a multicast group on the given multicast address. The
   * `interfaceAddress` is the local interface to bind the
   * membership to (empty string = let the kernel pick).
   * Returns `true` on success.
   */
  export function addMembership(
    socket: SocketHandle,
    multicastAddress: string,
    interfaceAddress?: string,
  ): boolean;

  /**
   * Leave a multicast group previously joined with `addMembership`.
   * Returns `true` on success.
   */
  export function dropMembership(
    socket: SocketHandle,
    multicastAddress: string,
    interfaceAddress?: string,
  ): boolean;
}
