import { useEffect, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";
import { getPairing, type PairingRecord } from "../lib/storage/idb";
import { connectPty, type PtyConnection } from "../lib/pty-ws";
import { connectPtySse } from "../lib/pty-sse";
import { listSessions } from "../lib/sessions";

type Status = "loading" | "connecting" | "connected" | "disconnected" | "error";

export default function Terminal() {
  const { hostId, sessionId } = useParams();
  const navigate = useNavigate();
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [status, setStatus] = useState<Status>("loading");
  const [errorMsg, setErrorMsg] = useState<string>("");
  const [retryNonce, setRetryNonce] = useState(0);
  const [debugLog, setDebugLog] = useState<string[]>([]);
  const debugLogRef = useRef<string[]>([]);
  const appendDebug = (msg: string) => {
    const stamped = `[${new Date().toISOString().slice(11, 19)}] ${msg}`;
    debugLogRef.current = [...debugLogRef.current, stamped].slice(-30);
    setDebugLog(debugLogRef.current);
  };

  useEffect(() => {
    if (!hostId || !sessionId) return;
    let cancelled = false;
    let xterm: XTerm | null = null;
    let fit: FitAddon | null = null;
    let connection: PtyConnection | null = null;
    let onResize: (() => void) | null = null;

    (async () => {
      setStatus("loading");
      let pairing: PairingRecord | undefined;
      try {
        pairing = await getPairing(hostId);
      } catch (e) {
        if (cancelled) return;
        setErrorMsg(`Failed to load pairing: ${errStr(e)}`);
        setStatus("error");
        return;
      }
      if (!pairing) {
        if (!cancelled) navigate("/pair");
        return;
      }

      if (!containerRef.current || cancelled) return;
      const isMobile =
        typeof navigator !== "undefined" &&
        /Mobi|Android|iPhone|iPad/i.test(navigator.userAgent);
      xterm = new XTerm({
        cursorBlink: true,
        fontFamily:
          'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        fontSize: isMobile ? 12 : 13,
        scrollback: 5000,
        smoothScrollDuration: 0,
        fastScrollSensitivity: 5,
        scrollSensitivity: 3,
        allowTransparency: true,
        theme: { background: "#0a0a0a" },
      });
      fit = new FitAddon();
      xterm.loadAddon(fit);
      xterm.open(containerRef.current);
      // WebGL for smooth GPU-accelerated rendering on mobile.
      try {
        const wgl = new WebglAddon();
        xterm.loadAddon(wgl);
        wgl.onContextLoss(() => wgl.dispose());
      } catch {
        /* WebGL not available */
      }
      try {
        fit.fit();
      } catch {
        /* jsdom can't measure; ignore */
      }

      setStatus("connecting");
      appendDebug("connectPty start");
      const isMobile =
        typeof navigator !== "undefined" &&
        /Mobi|Android|iPhone|iPad/i.test(navigator.userAgent);
      try {
        connection = await connectPty({
          pairing,
          sessionId,
          onDebug: appendDebug,
        });
      } catch (e) {
        appendDebug(`connectPty WS failed: ${errStr(e)}`);
        // Fall back to SSE transport on mobile or when WS is blocked.
        if (isMobile || `${e}`.includes("ws_error")) {
          appendDebug("trying SSE fallback...");
          try {
            connection = await connectPtySse({
              pairing,
              sessionId,
              onDebug: appendDebug,
            });
            appendDebug("SSE connected");
          } catch (e2) {
            appendDebug(`SSE also failed: ${errStr(e2)}`);
            if (cancelled) return;
            setErrorMsg(`Failed to connect: ${errStr(e2)}`);
            setStatus("error");
            return;
          }
        } else {
          if (cancelled) return;
          setErrorMsg(`Failed to connect: ${errStr(e)}`);
          setStatus("error");
          return;
        }
      }
      appendDebug("connectPty resolved");
      if (cancelled) {
        connection.close();
        return;
      }
      setStatus("connected");

      const enc = new TextEncoder();
      xterm.onData((data) => {
        if (!connection) return;
        void connection.sendInput(enc.encode(data)).catch(() => {
          /* swallow; close handler will surface */
        });
      });

      // Buffer output until the initial size-control frame lands.
      // Without this, bytes render at default 80×24, then xterm.resize
      // shifts positions and old content ghost-renders on mobile.
      let sized = false;
      const outputBuf: Uint8Array[] = [];
      const flushOutput = () => {
        for (const chunk of outputBuf.splice(0)) {
          xterm!.write(chunk);
        }
      };
      connection.onOutput((bytes) => {
        if (!xterm) return;
        if (!sized) {
          outputBuf.push(bytes);
        } else {
          xterm.write(bytes);
        }
      });

      connection.onControl((payload) => {
        if (
          !xterm ||
          typeof payload !== "object" ||
          payload === null ||
          (payload as { type?: unknown }).type !== "size"
        ) {
          return;
        }
        const p = payload as { rows?: number; cols?: number };
        const laptopRows = typeof p.rows === "number" ? p.rows : 0;
        const laptopCols = typeof p.cols === "number" ? p.cols : 0;
        if (laptopRows <= 0 || laptopCols <= 0) return;

        // First size frame — flush buffered output now that dimensions
        // are known, so the parser renders at the correct size.
        if (!sized) {
          sized = true;
          flushOutput();
        }

        const phoneRows = xterm.rows;
        const phoneCols = xterm.cols;
        appendDebug(
          `size msg laptop=${laptopRows}x${laptopCols} phone=${phoneRows}x${phoneCols}`,
        );

        const TOO_NARROW = 80;
        if (phoneCols >= TOO_NARROW) {
          xterm.resize(laptopCols, laptopRows);
        } else if (connection) {
          appendDebug(`request laptop shrink to ${phoneRows}x${phoneCols} (phone < 80 cols)`);
          void connection
            .sendControl({ type: "resize", rows: phoneRows, cols: phoneCols })
            .catch((err) => appendDebug(`sendControl resize failed: ${errStr(err)}`));
        }
      });

      connection.onClose((info) => {
        if (cancelled) return;
        setErrorMsg(`Connection closed (${info.code}${info.reason ? `: ${info.reason}` : ""})`);
        setStatus("disconnected");
        // Auto-reconnect on transient close (app switch, DERP flap, idle timeout).
        // Normal codes (1000, 1001) mean intentional close — don't auto-retry.
        if (
          info.code !== 1000 &&
          info.code !== 1001 &&
          !cancelled
        ) {
          const delay = info.code === 1006 ? 1500 : 3000;
          setTimeout(() => {
            if (!cancelled) setRetryNonce((n) => n + 1);
          }, delay);
        }
        if (
          info.code === 1006 ||
          info.code === 1011 ||
          info.code === 4500
        ) {
          void listSessions(pairing!)
            .then((sessions) => {
              if (cancelled) return;
              const stillAlive = sessions.some(
                (s) => s.id === sessionId && s.alive,
              );
              if (!stillAlive && hostId) {
                navigate(`/host/${encodeURIComponent(hostId)}`, {
                  replace: true,
                });
              }
            })
            .catch(() => {
              /* host unreachable — stay on disconnected screen */
            });
        }
      });

      onResize = () => {
        if (!fit || !connection || !xterm) return;
        try {
          fit.fit();
        } catch {
          return;
        }
        const { cols, rows } = xterm;
        void connection
          .sendControl({ type: "resize", cols, rows })
          .catch(() => {
            /* swallow */
          });
      };
      window.addEventListener("resize", onResize);
    })();

    return () => {
      cancelled = true;
      if (onResize) window.removeEventListener("resize", onResize);
      if (connection) connection.close();
      if (xterm) xterm.dispose();
    };
  }, [hostId, sessionId, navigate, retryNonce]);

  return (
    <section className="max-w-5xl mx-auto space-y-3">
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          {hostId ? (
            <Link
              to={`/host/${encodeURIComponent(hostId)}`}
              data-testid="terminal-back-button"
              aria-label="Back to sessions"
              className="inline-flex items-center gap-1 px-2 py-1 rounded border border-neutral-700 text-xs text-neutral-200 hover:bg-neutral-800"
            >
              ← Sessions
            </Link>
          ) : null}
          <h1 className="text-2xl font-semibold">Terminal</h1>
        </div>
        <div className="flex items-center gap-3 text-xs">
          <StatusBadge status={status} />
          <span className="font-mono text-neutral-500">
            host: {hostId} · session: {sessionId}
          </span>
        </div>
      </div>

      {status === "error" || status === "disconnected" ? (
        <div
          role="alert"
          className="rounded border border-red-700 bg-red-900/30 p-3 text-sm text-red-200 flex items-center justify-between gap-3"
        >
          <span>{errorMsg || "Disconnected."}</span>
          <button
            type="button"
            onClick={() => setRetryNonce((n) => n + 1)}
            className="px-3 py-1 rounded bg-red-700 hover:bg-red-600 text-xs font-semibold"
          >
            Retry
          </button>
        </div>
      ) : null}

      <div
        ref={containerRef}
        data-testid="xterm-container"
        className="rounded border border-neutral-800 bg-black p-2"
        style={{
          height: "min(75vh, 100dvh - 180px)",
          touchAction: "none",
          overscrollBehavior: "contain",
          WebkitUserSelect: "none",
          userSelect: "none",
        }}
      />

      {debugLog.length > 0 ? (
        <details
          open={status !== "connected"}
          className="rounded border border-neutral-800 bg-neutral-950 text-[11px] font-mono"
        >
          <summary className="px-2 py-1 cursor-pointer text-neutral-300">
            debug ({debugLog.length} events)
          </summary>
          <pre className="px-2 py-1 max-h-48 overflow-auto text-neutral-400 whitespace-pre-wrap break-all">
            {debugLog.join("\n")}
          </pre>
        </details>
      ) : null}
    </section>
  );
}

function StatusBadge({ status }: { status: Status }) {
  const label =
    status === "loading"
      ? "loading"
      : status === "connecting"
      ? "connecting"
      : status === "connected"
      ? "connected"
      : status === "disconnected"
      ? "disconnected"
      : "error";
  const cls =
    status === "connected"
      ? "bg-emerald-900/50 text-emerald-200 border-emerald-800"
      : status === "connecting" || status === "loading"
      ? "bg-neutral-800 text-neutral-200 border-neutral-700"
      : "bg-red-900/40 text-red-200 border-red-800";
  return (
    <span
      data-testid="conn-status"
      className={`px-2 py-0.5 rounded border text-[11px] uppercase tracking-wide ${cls}`}
    >
      {label}
    </span>
  );
}

function errStr(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
