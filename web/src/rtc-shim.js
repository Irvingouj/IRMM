// iron-remote-desktop (WASM) opens its RDP transport with the global `WebSocket`.
// For the gateway it gets a real wss:// URL and we pass through. For WebRTC it gets
// `rtc://<key>`; we intercept that, back it with a DataChannel (transport.openWebRtc),
// and expose a WebSocket-compatible object so the RDP client is unchanged.

import { openWebRtc } from './transport.js';

const pending = new Map();

/** Registers a WebRTC session for an `rtc://<key>` proxy address. */
export function registerRtc(key, config) {
  pending.set(key, config);
}

const Real = window.WebSocket;

class RtcSocket extends EventTarget {
  CONNECTING = 0;
  OPEN = 1;
  CLOSING = 2;
  CLOSED = 3;
  readyState = 0;
  binaryType = 'arraybuffer';

  constructor(url) {
    super();
    const key = url.slice('rtc://'.length);
    const config = pending.get(key);
    pending.delete(key);
    if (!config) {
      queueMicrotask(() => this.#fail('unknown rtc session'));
      return;
    }
    openWebRtc(config)
      .then((duplex) => {
        this.#duplex = duplex;
        duplex.onBytes((bytes) => this.dispatchEvent(new MessageEvent('message', { data: bytes.buffer })));
        duplex.onClose(() => {
          this.readyState = this.CLOSED;
          this.dispatchEvent(new CloseEvent('close'));
        });
        this.readyState = this.OPEN;
        this.dispatchEvent(new Event('open'));
      })
      .catch((e) => this.#fail(e.message));
  }

  #duplex = null;

  #fail(_msg) {
    this.readyState = this.CLOSED;
    this.dispatchEvent(new Event('error'));
    this.dispatchEvent(new CloseEvent('close'));
  }

  set onopen(fn) {
    this.#bind('open', fn);
  }
  set onmessage(fn) {
    this.#bind('message', fn);
  }
  set onclose(fn) {
    this.#bind('close', fn);
  }
  set onerror(fn) {
    this.#bind('error', fn);
  }
  #handlers = {};
  #bind(type, fn) {
    if (this.#handlers[type]) this.removeEventListener(type, this.#handlers[type]);
    this.#handlers[type] = fn;
    if (fn) this.addEventListener(type, fn);
  }

  #sendChain = Promise.resolve();
  send(data) {
    // iron-remote-desktop sends binary as a Blob; read it (async) but preserve order.
    let toBytes;
    if (typeof data === 'string') toBytes = Promise.resolve(new TextEncoder().encode(data));
    else if (data instanceof Blob) toBytes = data.arrayBuffer().then((b) => new Uint8Array(b));
    else if (data instanceof ArrayBuffer) toBytes = Promise.resolve(new Uint8Array(data));
    else toBytes = Promise.resolve(new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
    this.#sendChain = this.#sendChain.then(() => toBytes).then((bytes) => this.#duplex?.send(bytes));
  }
  close() {
    this.readyState = this.CLOSING;
    this.#duplex?.close();
  }
}

function PatchedWebSocket(url, protocols) {
  if (typeof url === 'string' && url.startsWith('rtc://')) return new RtcSocket(url);
  return protocols !== undefined ? new Real(url, protocols) : new Real(url);
}
PatchedWebSocket.prototype = Real.prototype;
PatchedWebSocket.CONNECTING = 0;
PatchedWebSocket.OPEN = 1;
PatchedWebSocket.CLOSING = 2;
PatchedWebSocket.CLOSED = 3;
window.WebSocket = PatchedWebSocket;
