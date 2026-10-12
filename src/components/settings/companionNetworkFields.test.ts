import assert from "node:assert/strict";
import {
  isValidAdvertisedHost,
  parseAdvertisedHost,
  parseCompanionPort,
  tlsPairShape,
} from "./companionNetworkFields";

// The backend rejects these shapes (validate_companion_network_override),
// so the UI must never commit them; every rule below mirrors a Rust-side
// rejection pinned in shortcut::tests::companion_network_override_validation.

// Advertised host: IPv4 literals and plain hostnames pass.
assert.equal(isValidAdvertisedHost("192.168.1.42"), true);
assert.equal(isValidAdvertisedHost("100.101.7.24"), true);
assert.equal(isValidAdvertisedHost("localhost"), true);
assert.equal(isValidAdvertisedHost("mac.tail1234.ts.net"), true);
assert.equal(isValidAdvertisedHost("  padded.host  "), true, "trimmed first");

// Scheme, port, spaces, underscores, and boundary hyphens are refused.
assert.equal(isValidAdvertisedHost("https://mac.tailnet.ts.net"), false);
assert.equal(isValidAdvertisedHost("mac.tailnet.ts.net:4177"), false);
assert.equal(isValidAdvertisedHost("mac tailnet"), false);
assert.equal(isValidAdvertisedHost("-leading.example"), false);
assert.equal(isValidAdvertisedHost("bad_underscore.example"), false);
assert.equal(isValidAdvertisedHost(""), false);

// Numeric non-IPs (a leading-zero or out-of-range octet) are not strict
// IPv4, but they ARE charset-valid hostnames, and the Rust validator
// accepts them on that same fallback: the mirror must agree with the
// backend, not invent a stricter rule.
assert.equal(isValidAdvertisedHost("192.168.001.1"), true);
assert.equal(isValidAdvertisedHost("256.1.1.1"), true);

// The field parse distinguishes unset (automatic) from a value.
assert.deepEqual(parseAdvertisedHost(""), { kind: "unset" });
assert.deepEqual(parseAdvertisedHost("   "), { kind: "unset" });
assert.deepEqual(parseAdvertisedHost("192.168.1.42"), {
  kind: "set",
  value: "192.168.1.42",
});
assert.deepEqual(parseAdvertisedHost("  mac.tail1234.ts.net "), {
  kind: "set",
  value: "mac.tail1234.ts.net",
});
assert.deepEqual(parseAdvertisedHost("not a host"), { kind: "invalid" });

// Port: integers 1..=65535, empty means unset, nothing else commits.
assert.deepEqual(parseCompanionPort(""), { kind: "unset" });
assert.deepEqual(parseCompanionPort("4177"), { kind: "set", value: 4177 });
assert.deepEqual(parseCompanionPort(" 8443 "), { kind: "set", value: 8443 });
assert.deepEqual(parseCompanionPort("65535"), { kind: "set", value: 65535 });
assert.deepEqual(parseCompanionPort("0"), { kind: "invalid" });
assert.deepEqual(parseCompanionPort("65536"), { kind: "invalid" });
assert.deepEqual(parseCompanionPort("41x7"), { kind: "invalid" });
assert.deepEqual(parseCompanionPort("1e3"), { kind: "invalid" });

// TLS pair shape: both or neither, a lone path is "half" and never
// commits (change_companion_tls_setting rejects it before writing).
assert.equal(tlsPairShape("", ""), "neither");
assert.equal(tlsPairShape("  ", " "), "neither");
assert.equal(tlsPairShape("/c.pem", "/k.pem"), "both");
assert.equal(tlsPairShape("/c.pem", ""), "half");
assert.equal(tlsPairShape("", "/k.pem"), "half");

console.log("companionNetworkFields: all assertions passed");
