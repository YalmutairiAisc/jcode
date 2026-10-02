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
fn loop_noul_questions_pass_request_validation_on_direct_providers() {
    let questions = json!({"meets": {"type": "noul", "instructions": "Is done_when met?",
        "criteria": {"true": "met", "false": "not met"}}})
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
        assert_eq!(body["questions"]["meets"]["type"], "noul");
        assert_eq!(body["model"], provider.model());
    }
}

/// Live: the loop's two Jev questions, through the caller's own Jev key: the
/// step review (one noul) and a file pick (one noul per candidate).
/// Run with `cargo test -p jcode-base --lib jev::loop_tests -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "calls the live Jev API with the configured BYOK key"]
async fn live_loop_review_and_file_pick() {
    let client = JevClient::for_loop().expect("a Jev BYOK key is configured");
    let review = json!({"meets": {"type": "noul",
        "instructions": "A helper attempted state.step. The loop ran its check and it passed. Does the change meet every part of state.done_when?",
        "criteria": {
            "true": "Yes: the change does what the step asks and done_when is met.",
            "false": "No: part of the step or done_when is missing, or the check does not exercise the change."
        }}})
    .as_object()
    .unwrap()
    .clone();
    let state = json!({
        "step": "Add a unit test for parse_port that rejects port 0",
        "done_when": "cargo test parse_port passes and a test asserts parse_port(\"0\") is an error",
        "check_command": "cargo test parse_port",
        "check_result": "passed in 2.1s",
        "check_output_end": "test result: ok. 12 passed; 0 failed",
        "changes": "=== changed src/port.rs ===\n+#[test]\n+fn parse_port_rejects_zero() {\n+    assert!(parse_port(\"0\").is_err());\n+}\n"
    });
    let value = client.evaluate(state, review).await.expect("live review");
    let meets = value["answers"]["meets"]["noul"].as_f64().expect("noul");
    eprintln!(
        "live review via {}: meets={meets} usage={}",
        client.provider_name(),
        value["usage"]
    );
    assert!((0.0..=1.0).contains(&meets));

    let pick = json!({
        "c0": {"type": "noul", "instructions": "Will someone doing state.task need to open the file state.candidates.c0?",
               "criteria": {"true": "The task needs this file.", "false": "The task does not need this file."}},
        "c1": {"type": "noul", "instructions": "Will someone doing state.task need to open the file state.candidates.c1?",
               "criteria": {"true": "The task needs this file.", "false": "The task does not need this file."}}
    })
    .as_object()
    .unwrap()
    .clone();
    let state = json!({
        "task": "Add a unit test for parse_port that rejects port 0",
        "candidates": {
            "c0": {"path": "src/port.rs", "lines": ["pub fn parse_port(text: &str) -> Result<u16> {"]},
            "c1": {"path": "web/styles/theme.css", "lines": [".navbar { color: red; }"]}
        }
    });
    let value = client.evaluate(state, pick).await.expect("live pick");
    let (needed, unrelated) = (
        value["answers"]["c0"]["noul"].as_f64().expect("noul"),
        value["answers"]["c1"]["noul"].as_f64().expect("noul"),
    );
    eprintln!(
        "live pick: port.rs={needed} theme.css={unrelated} usage={}",
        value["usage"]
    );
    assert!(
        needed > unrelated,
        "the needed file ranks above the unrelated one"
    );
}
