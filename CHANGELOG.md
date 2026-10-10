# Changelog

## Unreleased

Fixed: Command Mode bound to a lone modifier key (for example left
command) now engages after a short 150 ms hold instead of the 400 ms
default, and the overlay command badge lights the moment the hold fires,
before any live text updates.

- The hold window is per binding: every other lone-modifier binding keeps
  the 400 ms default.
- Releasing the key early or pressing any other key inside the window
  still cancels the activation, so accidental Cmd+letter chords never
  trigger command mode.

Companion devices: use a phone or tablet on the same Wi-Fi as a microphone
and push-to-talk trigger, with your Mac as the engine. Off by default in
Settings, Advanced, Experimental.

- Pair by QR code. The phone opens a single-file web page served by your
  Mac, with the talk button localized in all 26 app languages. No app to
  install.
- Audio takes the same recorder path as the built-in microphone, so the
  overlay, live text, voice commands, post-processing, and paste behave the
  same. A phone badge on the overlay marks a companion session.
- The phone and keyboard triggers share one recording session and arbitrate
  like the two keyboard bindings do, with a busy notice on both surfaces.
- TLS with a self-signed certificate generated once and shown by
  fingerprint in Settings; a 128-bit pairing token bound to your network;
  per-IP rate limiting on failed pairing; a 2x realtime input cap; and a 15
  minute session cap with auto-finalize.
- If the phone disconnects mid-dictation, everything captured up to the
  drop is transcribed and pasted, with a notice on the Mac and the phone.
- macOS only this cycle; the off path leaves no listener and no threads.
