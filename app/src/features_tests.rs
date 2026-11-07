use super::*;

#[test]
fn herdr_control_mode_is_enabled_at_startup() {
    assert!(enabled_features().contains(&FeatureFlag::HerdrControlMode));
}
