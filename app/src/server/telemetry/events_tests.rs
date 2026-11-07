use serde_json::json;
use warp_core::telemetry::TelemetryEventDesc;

use super::TelemetryEvent;

#[derive(Debug)]
enum TelemetryEventPropertyError {
    // The variant data is never directly read, but it's used for error formatting if the test
    // below fails.
    EmptyName(#[expect(dead_code)] Box<dyn TelemetryEventDesc>),
    EmptyDescription(#[expect(dead_code)] Box<dyn TelemetryEventDesc>),
}

#[test]
fn herdr_session_attached_payload_contains_only_tab_and_pane_counts() {
    let event = TelemetryEvent::HerdrSessionAttached {
        tab_count: 3,
        pane_count: 5,
    };

    assert_eq!(
        event.payload(),
        Some(json!({
            "tab_count": 3,
            "pane_count": 5,
        }))
    );
    assert!(!event.contains_ugc());
    assert_eq!(event.name(), "Herdr.SessionAttached");
}

/// Checks that all telemetry events have a non-empty name and description.
///
/// The name and description are intended to be user-facing and are used to populate
/// our [exhaustive telemetry table](https://docs.warp.dev/support-and-community/privacy-and-security/privacy#exhaustive-telemetry-table).
#[test]
#[cfg(not(target_family = "wasm"))]
fn telemetry_events_have_nonempty_name_and_description() -> Result<(), TelemetryEventPropertyError>
{
    for event in warp_core::telemetry::all_events() {
        if event.name().is_empty() {
            return Err(TelemetryEventPropertyError::EmptyName(event));
        }
        if event.description().is_empty() {
            return Err(TelemetryEventPropertyError::EmptyDescription(event));
        }
    }
    Ok(())
}
