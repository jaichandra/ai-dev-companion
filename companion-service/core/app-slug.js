// The short lowercase name the product's files and jobs go by: the install folder (~/<slug>), the state
// folder (~/.<slug>), the launchd label (com.<slug>.companion-service), the review-session folder and the
// id on a Jira remote link. A distribution sets branding.appSlug; changing it moves where an installed copy
// looks for its data, so it is fixed once something is installed.
const environment = require("../environment.js");

const DEFAULT_APP_SLUG = "ai-dev-companion";

const APP_SLUG = (environment.branding && environment.branding.appSlug) || DEFAULT_APP_SLUG;

module.exports = { APP_SLUG, DEFAULT_APP_SLUG };
