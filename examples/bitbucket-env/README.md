# bitbucket-env

A second example profile (Bitbucket Server/Data Center, Jira and Jenkins on other hosts). It is not a template to copy:
`npm run test:profiles` assembles the framework with it and runs the whole framework test list, so a test that assumes
the placeholder's GitHub profile (instead of calling `useGitHubProfile()` or reading `environment.js`) fails here, as
it would for a distribution with its own profile.
