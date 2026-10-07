// The Host-header check, as express middleware for the whole app. A page
// that rebinds its own DNS name to 127.0.0.1 still sends its own name in
// Host, so requiring 127.0.0.1:<port> / localhost:<port> stops that class of
// attack before any route (or the shared-secret check) runs.
const mcpAuth = require("./mcp-auth.js");

function createHostGuard(getPort) {
  return function hostGuard(req, res, next) {
    if (mcpAuth.hostAllowed(req.headers.host, getPort())) {
      next();
      return;
    }
    res.status(403).json({
      error: "This service only answers requests addressed to 127.0.0.1 or localhost on its own port.",
    });
  };
}

module.exports = { createHostGuard };
