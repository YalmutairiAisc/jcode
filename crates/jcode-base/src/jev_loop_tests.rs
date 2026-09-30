//! Loop-purpose routing tests, kept beside `jev.rs` so that file stays within
//! its size budget.

use super::*;

#[test]
fn loop_routes_to_byok_keys_only_and_never_to_the_jcode_gateway() {
    let selector = JevPurpose::Loop
        .selector_with(
            |key| {
                assert_eq!(key, LOOP_PROVIDER_ENV);
                Err(std::env::VarError::NotPresent)
            },
            || panic!("loop must ignore memory configuration"),
        )
        .unwrap();
    assert_eq!(selector, "auto");
    // Typesafe serves Jev directly, so it wins auto routing.
    assert_eq!(
        resolve_loop_with("auto", |_, _| Some("present".into()))
            .unwrap()
            .0,
        JevProvider::TypeSafe
    );
    for (available, expected) in [
        ("OPENROUTER_API_KEY", JevProvider::OpenRouter),
        ("AIMLAPI_API_KEY", JevProvider::Aimlapi),
    ] {
        let (provider, _) = resolve_loop_with("auto", |env, _| {
            assert_ne!(env, "JCODE_API_KEY", "loop must never load the Jcode key");
            (env == available).then(|| "test-key".into())
        })
        .unwrap();
        assert_eq!(provider, expected);
    }
    // A Jcode subscription credential alone cannot make the loop route work.
    let error = resolve_loop_with("auto", |env, _| {
        (env == "JCODE_API_KEY").then(|| "subscription-key".into())
    })
    .unwrap_err();
    assert!(error.to_string().contains("TYPESAFE_API_KEY"));
    for selector in ["jcode", "subscription", "jcode-subscription", "bogus", ""] {
        assert!(
            resolve_loop_with(selector, |_, _| panic!(
                "invalid loop route must not load keys"
            ))
            .is_err(),
            "{selector}"
        );
    }
    let (provider, _) = resolve_loop_with("typesafe", |env, file| {
        assert_eq!((env, file), ("TYPESAFE_API_KEY", "typesafe.env"));
        Some("typesafe-test-key".into())
    })
    .unwrap();
    assert_eq!(provider, JevProvider::TypeSafe);
}

#[test]
fn loop_choice_questions_pass_request_validation_on_direct_providers() {
    let questions = json!({"q": {"type": "choice", "instructions": "What next?",
        "criteria": {"done": "met", "retry": "fixable", "escalate": "stuck"}}})
    .as_object()
    .unwrap()
    .clone();
    for provider in [
        JevProvider::TypeSafe,
        JevProvider::OpenRouter,
        JevProvider::Aimlapi,
    ] {
        let body = request_body_for(
            JevPurpose::Loop,
            provider,
            json!({"step": "x", "check_passed": true}),
            &questions,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["questions"]["q"]["type"], "choice");
        assert_eq!(body["model"], provider.model());
    }
}

/// Live: one real `step_outcome` choice fork through the caller's own Jev key.
/// Run with `cargo test -p jcode-base --lib jev::loop_tests -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls the live Jev API with the configured BYOK key"]
async fn live_loop_choice_fork() {
    let client = JevClient::for_loop().expect("a Jev BYOK key is configured");
    let questions = json!({"q": {"type": "choice",
        "instructions": "A helper just attempted one step of a coding task and reported back. What should happen next?",
        "criteria": {
            "done": "The step's goal is met and its check passed.",
            "retry": "The attempt failed for a small, fixable reason that another attempt is likely to fix.",
            "escalate": "The attempt failed in a way that needs a different approach or a decision."
        }}})
    .as_object()
    .unwrap()
    .clone();
    let state = json!({
        "step": "Add a unit test for parse_port",
        "done_when": "cargo test parse_port passes",
        "attempt_number": 1,
        "helper_summary": "Added test_parse_port_rejects_zero; all 12 tests pass.",
        "check_command": "cargo test parse_port",
        "check_passed": true,
        "check_output_end": "test result: ok. 12 passed; 0 failed",
        "blocker": "",
        "previous_attempt_error_end": ""
    });
    let value = client.evaluate(state, questions).await.expect("live fork");
    let answer = &value["answers"]["q"];
    eprintln!(
        "live loop fork via {}: choice={} confidence={} probabilities={} usage={}",
        client.provider_name(),
        answer["choice"],
        answer["confidence"],
        answer["probabilities"],
        value["usage"]
    );
    assert_eq!(answer["type"], "choice");
    assert!(matches!(
        answer["choice"].as_str(),
        Some("done" | "retry" | "escalate")
    ));
}
