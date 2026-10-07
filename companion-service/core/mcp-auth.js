// The /mcp route's own trust boundary (core/mcp.ts): a bearer token of its
// own (`mcp.token` in credentials.enc), never the extension's
// sharedSecret, so a leaked ~/.claude.json (where Claude Code keeps the
// token it was registered with) can drive the MCP tools but none of the
// extension routes. Plain JS, not TypeScript — same reason as
// core/prereqs.js: mcp-auth.test.js runs it directly, and install/setup
// can share generateMcpToken with zero build step.
const crypto = require("crypto");

const TOKEN_BYTES = 32;

/** A fresh MCP token: 32 random bytes as base64url (43 chars, no padding),
 * safe to paste into a JSON config or an HTTP header as-is. `randomBytes`
 * is injectable for tests. */
function generateMcpToken(randomBytes = crypto.randomBytes) {
  return randomBytes(TOKEN_BYTES).toString("base64url");
}

/**
 * Whether an Authorization header carries exactly `Bearer <expectedToken>`
 * (scheme case-insensitive, exactly one space). Compared in constant time
 * with timingSafeEqual; that throws on unequal lengths, so the length is
 * checked first (leaking only the length, which is fixed anyway). Never
 * throws, whatever the header; false whenever no token is expected, so an
 * unset token can never be "matched" by an empty one.
 */
function bearerMatches(authorizationHeader, expectedToken) {
  if (!expectedToken || typeof expectedToken !== "string") return false;
  if (typeof authorizationHeader !== "string") return false;
  const match = /^Bearer ([^ ]+)$/i.exec(authorizationHeader);
  if (!match) return false;
  const given = Buffer.from(match[1], "utf8");
  const expected = Buffer.from(expectedToken, "utf8");
  if (given.length !== expected.length) return false;
  return crypto.timingSafeEqual(given, expected);
}

/** DNS-rebinding guard: the service only listens on 127.0.0.1, but a web
 * page can still point its own hostname at that address — its requests
 * then carry that hostname in Host. Only the two names a local MCP client
 * actually uses, on this service's own port, are let through. */
function hostAllowed(hostHeader, port) {
  if (typeof hostHeader !== "string") return false;
  return hostHeader === `127.0.0.1:${port}` || hostHeader === `localhost:${port}`;
}

module.exports = {
  generateMcpToken,
  bearerMatches,
  hostAllowed,
};
