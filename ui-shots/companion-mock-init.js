/*
 * Browser-side mock for the companion phone page
 * (src-tauri/src/companion/client/index.html) inside the screenshot rig.
 *
 * Injected by ui-shots/shoot.mjs via context.addInitScript({ path }) so it
 * runs before the page's inline script. The page is plain HTML + JS served
 * by the Mac's companion server; it opens a WebSocket to /ws, reads the
 * pairing token from the URL hash, and captures microphone audio. The rig:
 *   - replaces WebSocket with a controllable fake (window.__companion)
 *   - leaves getUserMedia real; the browser launches with Chromium's fake
 *     media device flags so audio capture and the level meter run the page's
 *     own capture graph end to end. Scenario companion-mic-error overrides
 *     getUserMedia to reject, rendering the mic-error card.
 *   - ?voxshot=<scenario> selects the behavior (same convention as the
 *     Tauri mock in tauri-mock-init.js).
 */
(() => {
  if (!window.location.pathname.includes("/companion/")) return;
  if (window.__companionMock) return;

  const params = new URLSearchParams(window.location.search);
  const scenario = params.get("voxshot") || "companion-ready";

  // ---- fake WebSocket ----
  const sockets = [];
  const sent = [];
  // The Connecting state lives between connect() and onopen; the default
  // 25ms open fires too fast to frame, so the companion-connecting
  // scenario simply never opens the socket (a phone waiting on a dead
  // LAN route) and the pill stays on "Connecting" for the capture.
  const neverOpens = scenario === "companion-connecting";
  class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.binaryType = "blob";
      this.bufferedAmount = 0;
      this.onopen = null;
      this.onmessage = null;
      this.onclose = null;
      this.onerror = null;
      this.extensions = "";
      this.protocol = "";
      sockets.push(this);
      // Real sockets take at least one round trip to open; the delay keeps
      // the Connecting pill renderable and the open edge asynchronous.
      if (!neverOpens) {
        setTimeout(() => {
          if (this.readyState !== 0) return;
          this.readyState = 1;
          if (this.onopen) this.onopen({ type: "open" });
        }, 25);
      }
    }
    send(data) {
      if (this.readyState !== 1) {
        throw new Error("WebSocket is not open");
      }
      sent.push(data);
    }
    close(code, reason) {
      if (this.readyState === 2 || this.readyState === 3) return;
      this.readyState = 3;
      if (this.onclose) {
        this.onclose({
          type: "close",
          code: code === undefined ? 1000 : code,
          reason: reason || "",
          wasClean: true,
        });
      }
    }
  }
  window.WebSocket = FakeWebSocket;

  // ---- microphone stubs ----
  if (scenario === "companion-mic-error") {
    const deny = () =>
      Promise.reject(
        new DOMException(
          "Permission denied",
          navigator.mediaDevices && navigator.mediaDevices.getUserMedia
            ? "NotAllowedError"
            : "NotAllowedError",
        ),
      );
    if (navigator.mediaDevices) {
      navigator.mediaDevices.getUserMedia = deny;
    } else {
      Object.defineProperty(navigator, "mediaDevices", {
        value: { getUserMedia: deny },
        configurable: true,
      });
    }
  }
  if (scenario === "companion-recording") {
    // Headless Chromium's fake media device delivers real (tone) samples
    // through ScriptProcessorNode but silence into AudioWorklet inputs,
    // so the page's primary capture path would meter 0% forever. The page
    // ships its own fallback branch for engines without AudioWorklet
    // (scriptProcessorFallback, same 16 kHz resample + 40 ms chunking);
    // hiding the worklet selects that branch so the meter fills from the
    // page's own level code over real fake-device samples. The accessor
    // lives on BaseAudioContext.prototype (AudioContext inherits it).
    for (const Ctor of [window.AudioContext, window.webkitAudioContext]) {
      if (Ctor && Ctor.prototype) {
        delete Ctor.prototype.audioWorklet;
        const base = Object.getPrototypeOf(Ctor.prototype);
        if (base && "audioWorklet" in base) delete base.audioWorklet;
      }
    }
  }

  // ---- rig control surface ----
  const latest = () => sockets[sockets.length - 1] || null;
  window.__companion = {
    scenario,
    sockets,
    sent,
    latest,
    /** Deliver one server frame to the live socket. */
    message(obj) {
      const sock = latest();
      if (!sock || sock.readyState !== 1) {
        throw new Error("no open fake websocket");
      }
      sock.onmessage({ data: JSON.stringify(obj), type: "message" });
    },
    /** Server dropped the connection (fires the page's onclose path). */
    drop() {
      const sock = latest();
      if (!sock) throw new Error("no fake websocket");
      sock.readyState = 3;
      if (sock.onclose) {
        sock.onclose({
          type: "close",
          code: 1006,
          reason: "",
          wasClean: false,
        });
      }
    },
  };
  window.__companionMock = true;
})();
