# minimal-env: GitHub, Jira and Jenkins

An example distribution profile for the framework: `companion-service/environment.js` names GitHub as the git host
(github.com or GitHub Enterprise), Jira Server/Data Center for tickets and Jenkins for builds. It is the whole of a
distribution that needs no code of its own: the framework supplies every feature.

## Try it

From the framework's root:

```
npm run test:example      # assembles framework + this profile, builds it, and runs github-profile.test.js in it
```

## Use it as a starting point

1. Create a repo for your distribution that depends on this one (`"ai-dev-companion": "<path or git url>"`).
2. Copy `companion-service/environment.js` into a `companion/` overlay folder, change the hosts, update source and
   branding.
3. Assemble with `scripts/assemble.js` (`frameworkDir` = this repo, `rootDir` = yours, `overlays` = your folder),
   install from the result, and run the framework's tests in it to check your profile.

## Using GitHub

GitHub's browser login doesn't work for its API, so the companion uses a **personal access token** only. In ✨ →
⚙ Settings → Code and accounts → GitHub, paste a token (a fine-grained token with read and write access to *Pull
requests* and *Contents* on the repositories you work on, or a classic token with the `repo` scope). For GitHub
Enterprise Server, enter its web address as the base URL (not the API address).

What works: Resolve conflicts, Address review comments (resolved and outdated review threads are skipped), Review PR
in your editor, the digest, ticket workspaces and Ticket to PR. What differs from Bitbucket:

- Pull requests from **forks** are not offered or processed (the companion works in a clone of the target repository).
- GitHub has no default reviewers, so Create PR opens the pull request without adding any.
- Only inline review comments are addressed, not comments in the PR's conversation tab.
- Mergeability is worked out lazily by GitHub; a PR whose conflicts aren't known yet isn't listed until it is.
- The extension runs only on pull request pages (`github.com/<owner>/<repo>/pull/<n>`), is given no github.com
  cookies, and talks to GitHub only through the companion.
