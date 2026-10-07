// What the product is called in text the user reads (setup and doctor banners, the note on generated
// comments, the extension's name): the distribution's branding.productName, else the framework's own name.
const environment = require("../environment.js");

const DEFAULT_PRODUCT_NAME = "AI Dev Companion";

const PRODUCT_NAME = (environment.branding && environment.branding.productName) || DEFAULT_PRODUCT_NAME;

module.exports = { PRODUCT_NAME, DEFAULT_PRODUCT_NAME };
