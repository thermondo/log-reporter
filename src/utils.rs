use uuid::Uuid;

use crate::log_parser::{
    parse_offer_extension_number, parse_offer_number, parse_project_reference, parse_sfid,
};

/// generate a route-name from a URL path.
/// Replaces elements in the URL that are
/// - positive integers
/// - UUIDs
/// - Salesforce IDs
/// - thermondo project references
/// - thermondo offer & offer-extension numbers
pub(crate) fn route_from_path(path: &str) -> String {
    let elements: Vec<_> = path
        .split('/')
        .map(|el| {
            if el.parse::<u64>().is_ok() {
                "{number}"
            } else if Uuid::try_parse(el).is_ok() {
                "{uuid}"
            } else if parse_sfid(el).is_ok() {
                "{sfid}"
            } else if parse_project_reference(el).is_ok() {
                "{project_reference}"
            } else if parse_offer_number(el).is_ok() {
                "{offer_number}"
            } else if parse_offer_extension_number(el).is_ok() {
                "{offer_extension_number}"
            } else {
                el
            }
        })
        .collect();
    elements.join("/")
}
