// SSE-based PTY transport — alternative to WebSocket for mobile
// browsers behind proxies that don't support WS upgrade.
//
// Output:  EventSource → SSE stream from GET /api/v1/sessions/:id/pty
// Input:   fetch POST → /api/v1/sessions/:id/pty/input
//
// The connect token (?ct=) is built identically to the WS path.

import { _b64u, type CryptoPrivateKey } from "./crypto/ed25519";
import { canonicalBytes, bodyHashHex } from "./crypto/canonical";
import type { PairingRecord } from "./storage/idb";
import type { PtyConnection, FrameKind } from "./pty-ws";

async function buildCt(args: {
  deviceId: string;
  privateKey: CryptoPrivateKey;
  capabilityTokenB64: string;
  sessionId: string;
  path: string;
}): Promise<string> {
  const ts = new Date().toISOString();
  const nonceBytes = new Uint8Array(16);
  crypto.getRandomValues(nonceBytes);
  const nonce = _b64u.encode(nonceBytes);
  const emptyHash = await bodyHashHex(new Uint8Array(0));
  const canonical = canonicalBytes({
    method: "GET",
    path: args.path,
    query: "",
    ts,
    nonce,
    bodySha256Hex: emptyHash,
    deviceId: args.deviceId,
    protocolVersion: 1,
  });
  const { sign } = await import("./crypto/ed25519");
  const sig = await sign(args.privateKey, canonical);
  const bundle = {
    v: 1,
    device_id: args.deviceId,
    ts,
    nonce,
    sig: _b64u.encode(sig),
    capability_token: args.capabilityTokenB64,
  };
  return _b64u.encode(new TextEncoder().encode(JSON.stringify(bundle)));
}

export interface ConnectSseOptions {
  pairing: PairingRecord;
  sessionId: string;
  onDebug?: (msg: string) => void;
}

export async function connectPtySse(
  opts: ConnectSseOptions,
): Promise<PtyConnection> {
  const dbg = opts.onDebug ?? (() => {});
  const hostBase = opts.pairing.hostUrl.replace(/\/$/, "");
  const path = `/api/v1/sessions/${opts.sessionId}/pty`;

  // Import the device private key.
  const { importPrivateKeyJwk } = await import("./crypto/ed25519");
  const privateKey = await importPrivateKeyJwk(opts.pairing.privateKeyJwk);

  // Build connect token for SSE output endpoint.
  const ct = await buildCt({
    deviceId: opts.pairing.deviceId,
    privateKey,
    capabilityTokenB64: opts.pairing.capabilityTokenB64,
    sessionId: opts.sessionId,
    path,
  });

  const sseUrl = `${hostBase}${path}?ct=${encodeURIComponent(ct)}`;
  dbg(`sse: opening ${sseUrl}`);

  const es = new EventSource(sseUrl);
  let closed = false;
  let outboundSeq = 0;
  const outputHandlers = new Set<(bytes: Uint8Array) => void>();
  const closeHandlers = new Set<
    (info: { code: number; reason: string }) => void
  >();

  const fireClose = (code: number, reason: string) => {
    if (closed) return;
    closed = true;
    es.close();
    for (const h of closeHandlers) {
      try { h({ code, reason }); } catch { /* swallow */ }
    }
  };

  es.onopen = () => {
    dbg("sse: connected");
  };

  es.onmessage = (evt) => {
    if (closed) return;
    try {
      const bytes = _b64u.decode(evt.data);
      for (const h of outputHandlers) {
        try { h(bytes); } catch { /* swallow */ }
      }
    } catch {
      dbg(`sse: bad frame: ${evt.data.slice(0, 80)}`);
    }
  };

  es.addEventListener("snapshot", ((evt: MessageEvent) => {
    if (closed) return;
    try {
      const bytes = _b64u.decode(evt.data);
      for (const h of outputHandlers) {
        try { h(bytes); } catch { /* swallow */ }
      }
    } catch {
      dbg(`sse: bad snapshot`);
    }
  }) as EventListener);

  es.onerror = () => {
    if (closed) return;
    dbg("sse: error, closing");
    fireClose(1006, "sse_error");
  };

  // Build input function that POSTs keystrokes.
  const inputPath = `/api/v1/sessions/${opts.sessionId}/pty/input`;
  const sendInput = async (bytes: Uint8Array): Promise<void> => {
    if (closed) throw new Error("connection_closed");
    // Build a fresh connect token per input POST (nonce required).
    const inputCt = await buildCt({
      deviceId: opts.pairing.deviceId,
      privateKey,
      capabilityTokenB64: opts.pairing.capabilityTokenB64,
      sessionId: opts.sessionId,
      path: inputPath,
    });
    const url = `${hostBase}${inputPath}?ct=${encodeURIComponent(inputCt)}`;
    const b64 = _b64u.encode(bytes);
    const res = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ input: b64 }),
    });
    if (!res.ok) {
      dbg(`sse: input POST failed: ${res.status}`);
      throw new Error(`input_failed:${res.status}`);
    }
  };

  return {
    sendInput,
    sendControl: async (_payload: object) => {
      // Control frames (resize) not yet wired for SSE.
    },
    onOutput(h) {
      outputHandlers.add(h);
      return () => outputHandlers.delete(h);
    },
    onControl(_h) {
      return () => {};
    },
    onClose(h) {
      closeHandlers.add(h);
      return () => closeHandlers.delete(h);
    },
    async ping() {
      // no-op for SSE
    },
    close() {
      fireClose(1000, "client_close");
    },
  };
}
