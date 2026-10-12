// Companion network-override parsing for the settings panel.
//
// Mirrors the Rust validation (validate_companion_network_override and
// ip::is_valid_advertised_host) so a bad value never leaves the panel:
// an advertised host must be an IPv4 literal or a plain hostname (no
// scheme, port, or slashes), and the port must be an integer from 1 to
// 65535. Empty strings mean "unset" (the automatic pre-v1.6.0 behavior)
// and are always valid; a partially typed value is invalid and stays a
// draft until it parses.

export type OverrideField =
  | { kind: "unset" }
  | { kind: "set"; value: string }
  | { kind: "invalid" };

/** Same shape for the port, whose committed value is a number. */
export type PortField =
  | { kind: "unset" }
  | { kind: "set"; value: number }
  | { kind: "invalid" };

const IPV4_RE = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

function isIpv4Literal(s: string): boolean {
  const match = s.match(IPV4_RE);
  if (!match) return false;
  // Each octet 0-255 with no leading zeros, matching Rust's strict
  // Ipv4Addr parser ("192.168.001.1" is refused there too).
  return match.slice(1).every((part) => {
    const n = Number.parseInt(part, 10);
    return n >= 0 && n <= 255 && String(n) === part;
  });
}

const HOST_LABEL_RE = /^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$/;

/** Is this a plain hostname (dot-separated labels, no scheme/port/slash)? */
export function isValidAdvertisedHost(raw: string): boolean {
  const s = raw.trim();
  if (!s || s.length > 253) return false;
  if (isIpv4Literal(s)) return true;
  return s
    .split(".")
    .every((label) => label.length <= 63 && HOST_LABEL_RE.test(label));
}

/** Parse the advertised-host field: unset, a valid host, or invalid. */
export function parseAdvertisedHost(raw: string): OverrideField {
  const s = raw.trim();
  if (!s) return { kind: "unset" };
  return isValidAdvertisedHost(s)
    ? { kind: "set", value: s }
    : { kind: "invalid" };
}

/** Parse the port field: unset, an integer 1..=65535, or invalid. */
export function parseCompanionPort(raw: string): PortField {
  const s = raw.trim();
  if (!s) return { kind: "unset" };
  if (!/^\d+$/.test(s)) return { kind: "invalid" };
  const value = Number.parseInt(s, 10);
  if (!Number.isSafeInteger(value) || value < 1 || value > 65535) {
    return { kind: "invalid" };
  }
  return { kind: "set", value };
}

/** Both halves of the manual TLS pair or neither (the backend's
 * both-or-neither rule; a lone path never persists). */
export function tlsPairShape(
  cert: string,
  key: string,
): "both" | "neither" | "half" {
  const c = cert.trim() !== "";
  const k = key.trim() !== "";
  if (c && k) return "both";
  if (!c && !k) return "neither";
  return "half";
}
