// Links that came from a service (a PR or ticket URL) end up in the inbox and
// are opened on a click. Only https links on the configured Bitbucket or Jira
// host are kept; anything else becomes null. Pure.

/** `url` when it is https, has no credentials and its host is one of `baseUrls`' hosts; else null. */
function linkOnHosts(url, baseUrls) {
  if (typeof url !== "string" || !url) return null;
  let u;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  if (u.protocol !== "https:" || u.username || u.password) return null;
  for (const base of baseUrls || []) {
    try {
      if (new URL(base).host.toLowerCase() === u.host.toLowerCase()) return u.toString();
    } catch {
      // an unusable configured base URL allows nothing
    }
  }
  return null;
}

module.exports = { linkOnHosts };
