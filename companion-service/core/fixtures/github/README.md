Hand-written GitHub API responses (REST and GraphQL), shaped after GitHub's documented fields, for the
GitHub provider's tests. They are not recordings: no GitHub server was available when they were written, so the
first run against a real account should be compared with them. Cases they cover: a conflicted PR (`mergeable:
false`, `mergeable_state: "dirty"`), a PR whose mergeability is still being computed (`null`), a fork PR, a PR whose
fork was deleted (`head.repo: null`), a merged PR; review threads that are open, resolved, outdated, on the removed
side, unanswered, empty and duplicated across pages; check runs and commit statuses in every state; and a GraphQL
search result with approvals, requested changes, a team review request and junk entries.
