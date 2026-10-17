// Channel transports to an agent. A channel is reached either through the WSS gateway
// (the server opens the agent channel, so the browser speaks raw channel bytes) or over
// a WebRTC DataChannel (the browser itself sends the ChannelOpen ticket, then raw bytes).
//
// Shell and Files channels are length-prefixed JSON frames; RDP is a raw byte stream that
// iron-remote-desktop drives over its own WebSocket (see rtc-shim.js for the WebRTC case).

import { api, wsBase } from './api.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

export function frame(objOrBytes) {
  const body = objOrBytes instanceof Uint8Array ? objOrBytes : enc.encode(JSON.stringify(objOrBytes));
  const out = new Uint8Array(4 + body.length);
  new DataView(out.buffer).setUint32(0, body.length);
  out.set(body, 4);
  return out;
}

export class FrameReader {
  #buf = new Uint8Array(0);
  push(bytes) {
    const merged = new Uint8Array(this.#buf.length + bytes.length);
    merged.set(this.#buf);
    merged.set(bytes, this.#buf.length);
    this.#buf = merged;
  }
  /** Next complete frame as parsed JSON, or null. */
  next() {
    if (this.#buf.length < 4) return null;
    const len = new DataView(this.#buf.buffer, this.#buf.byteOffset).getUint32(0);
    if (this.#buf.length < 4 + len) return null;
    const body = this.#buf.slice(4, 4 + len);
    this.#buf = this.#buf.slice(4 + len);
    return JSON.parse(dec.decode(body));
  }
}

/** Opens a WebRTC DataChannel to the agent and completes the ChannelOpen handshake.
 *  Resolves to a raw byte duplex: { send(bytes), onBytes(cb), onClose(cb), close() }. */
export async function openWebRtc({ sessionId, ticket, iceServers }) {
  const pc = new RTCPeerConnection({ iceServers: iceServers?.length ? [{ urls: iceServers }] : [] });
  const dc = pc.createDataChannel('irmm');
  dc.binaryType = 'arraybuffer';

  const open = new Promise((resolve, reject) => {
    dc.onopen = resolve;
    dc.onerror = () => reject(new Error('data channel error'));
  });
  await pc.setLocalDescription(await pc.createOffer());
  await iceGatheringComplete(pc);
  const { sdp } = await api.post(`/api/sessions/${sessionId}/rtc`, { sdp: pc.localDescription.sdp });
  await pc.setRemoteDescription({ type: 'answer', sdp });
  await open;

  const reader = new FrameReader();
  let pending = [];
  let onBytes = (b) => pending.push(b);
  let onClose = () => {};
  let accepted = false;
  const ready = new Promise((resolve, reject) => {
    dc.onmessage = (e) => {
      const bytes = new Uint8Array(e.data);
      if (accepted) {
        onBytes(bytes);
        return;
      }
      reader.push(bytes);
      const reply = reader.next();
      if (!reply) return;
      accepted = true;
      if (reply.type !== 'accepted') {
        reject(new Error(reply.reason || 'channel denied'));
        return;
      }
      resolve();
    };
  });
  dc.onclose = () => onClose();
  dc.send(frame({ ticket }));
  await ready;

  return {
    send: (bytes) => dc.send(bytes),
    onBytes: (cb) => {
      onBytes = cb;
      for (const b of pending) cb(b);
      pending = [];
    },
    onClose: (cb) => {
      onClose = cb;
    },
    close: () => {
      try {
        dc.close();
      } finally {
        pc.close();
      }
    },
  };
}

function iceGatheringComplete(pc) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      if (pc.iceGatheringState === 'complete') {
        pc.removeEventListener('icegatheringstatechange', done);
        resolve();
      }
    };
    pc.addEventListener('icegatheringstatechange', done);
    setTimeout(resolve, 3000); // fall back to whatever candidates we have
  });
}

/** Opens a raw byte duplex over the WSS gateway (the server already opened the channel). */
export function openGateway({ device, ticket }) {
  const ws = new WebSocket(`${wsBase()}/api/gw/${device}?ticket=${encodeURIComponent(ticket)}`);
  ws.binaryType = 'arraybuffer';
  let onBytes = () => {};
  let onClose = () => {};
  ws.onmessage = (e) => onBytes(new Uint8Array(e.data));
  ws.onclose = () => onClose();
  const opened = new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = () => reject(new Error('gateway connection failed'));
  });
  return {
    opened,
    send: (bytes) => ws.send(bytes),
    onBytes: (cb) => {
      onBytes = cb;
    },
    onClose: (cb) => {
      onClose = cb;
    },
    close: () => ws.close(),
  };
}

/** Creates a remote session and opens a framed (Shell/Files) channel over the chosen transport. */
export async function openFramedChannel({ device, kind, monitor = 0, transport }) {
  const session = await api.post(`/api/devices/${device}/sessions`, { kind, monitor });
  const raw =
    transport === 'webrtc'
      ? await openWebRtc({ sessionId: session.session_id, ticket: session.ticket, iceServers: session.ice_servers })
      : openGateway({ device, ticket: session.ticket });
  if (raw.opened) await raw.opened;

  const reader = new FrameReader();
  let onMsg = () => {};
  raw.onBytes((bytes) => {
    reader.push(bytes);
    for (let m = reader.next(); m; m = reader.next()) onMsg(m);
  });
  return {
    session,
    write: (obj) => raw.send(frame(obj)),
    onMessage: (cb) => {
      onMsg = cb;
    },
    onClose: (cb) => raw.onClose(cb),
    close: () => raw.close(),
  };
}
