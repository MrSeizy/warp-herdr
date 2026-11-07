use super::*;

#[test]
#[ignore = "CORE-3768 - need to clean up PREVIEW_FLAGS, but this is a temporary fix for the cluttered changelog"]
fn test_all_preview_flags_have_a_description() {
    for flag in PREVIEW_FLAGS {
        assert!(
            flag.flag_description()
                .is_some_and(|description| !description.is_empty()),
            "Missing description for preview-enabled flag {flag:?}"
        );
    }
}

#[test]
fn factory_deferred_repositories_are_dogfood_only_by_default() {
    assert!(DOGFOOD_FLAGS.contains(&FeatureFlag::FactoryDeferredRepositories));
    for flags in [DEBUG_FLAGS, LOCAL_FLAGS, PREVIEW_FLAGS, RELEASE_FLAGS] {
        assert!(!flags.contains(&FeatureFlag::FactoryDeferredRepositories));
    }
}

#[test]
fn local_child_harnesses_are_local_only_by_default() {
    assert!(LOCAL_FLAGS.contains(&FeatureFlag::LocalClaudeCodexChildHarnesses));
    assert!(!DEBUG_FLAGS.contains(&FeatureFlag::LocalClaudeCodexChildHarnesses));
    assert!(!DOGFOOD_FLAGS.contains(&FeatureFlag::LocalClaudeCodexChildHarnesses));
}

/// Herdr control mode is dogfood-gated, but its development loop runs on the
/// OSS/local binary (script/run without warp-channel-config), where dogfood
/// flags are off. The Debug menu's runtime toggle is the sanctioned way to
/// exercise it there — dropping it from this list silently hides the
/// attach/detach commands from every dev build.
#[test]
fn herdr_control_mode_is_runtime_toggleable_for_dev_builds() {
    assert!(RUNTIME_FEATURE_FLAGS.contains(&FeatureFlag::HerdrControlMode));
    assert!(DOGFOOD_FLAGS.contains(&FeatureFlag::HerdrControlMode));
}

#[test]
fn account_first_onboarding_is_dogfood_only() {
    assert!(!LOCAL_FLAGS.contains(&FeatureFlag::AccountFirstOnboarding));
    assert!(!DEBUG_FLAGS.contains(&FeatureFlag::AccountFirstOnboarding));
    assert!(DOGFOOD_FLAGS.contains(&FeatureFlag::AccountFirstOnboarding));
    assert!(!PREVIEW_FLAGS.contains(&FeatureFlag::AccountFirstOnboarding));
    assert!(!RELEASE_FLAGS.contains(&FeatureFlag::AccountFirstOnboarding));
}
