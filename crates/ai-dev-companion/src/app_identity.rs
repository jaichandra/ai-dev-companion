//! The product's names (ports of `core/app-slug.js` and `core/product-name.js`).

use crate::environment;

pub const DEFAULT_APP_SLUG: &str = "ai-dev-companion";
pub const DEFAULT_PRODUCT_NAME: &str = "AI Dev Companion";

/// The short lowercase name the product's files and jobs go by: the install
/// folder (`~/<slug>`), the state folder (`~/.<slug>`), the launchd label
/// (`com.<slug>.companion-service`), the review-session folder and the id on a
/// Jira remote link. A distribution sets `branding.appSlug`; changing it moves
/// where an installed copy looks for its data, so it is fixed once something
/// is installed.
pub fn app_slug() -> &'static str {
    environment::current().branding_text("appSlug").unwrap_or(DEFAULT_APP_SLUG)
}

/// What the product is called in text the user reads (setup and doctor
/// banners, the note on generated comments, the extension's name): the
/// distribution's `branding.productName`, else the framework's own name.
pub fn product_name() -> &'static str {
    environment::current().branding_text("productName").unwrap_or(DEFAULT_PRODUCT_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_framework_placeholder_uses_the_default_names() {
        assert_eq!((app_slug(), product_name()), (DEFAULT_APP_SLUG, DEFAULT_PRODUCT_NAME));
        assert_eq!(DEFAULT_APP_SLUG, "ai-dev-companion");
        assert_eq!(DEFAULT_PRODUCT_NAME, "AI Dev Companion");
    }
}
