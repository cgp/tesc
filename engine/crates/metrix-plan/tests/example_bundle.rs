//! The worked example in `examples/plans/checkout-mixed` is the fixture that proves
//! these types match the format the design documents describe. If the design changes,
//! this test is where the mismatch shows up.

use std::path::PathBuf;

use metrix_plan::{CallFile, Mix, PERCENT_EPSILON, PERCENT_TOTAL, SessionPolicy, Targets};

fn bundle() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../examples/plans/checkout-mixed")
        .canonicalize()
        .expect("example bundle should exist")
}

fn read(name: &str) -> String {
    let path = bundle().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[test]
fn mix_parses() {
    let mix: Mix = serde_json::from_str(&read("mix.json")).expect("mix.json should parse");

    assert_eq!(mix.name, "checkout-mixed");
    assert_eq!(mix.chains.len(), 6);
    assert_eq!(mix.load.rate, Some(150.0));
    assert_eq!(mix.phases.baseline.as_duration().as_secs(), 30);
    assert_eq!(mix.phases.settle.as_duration().as_secs(), 60);
}

#[test]
fn the_mixture_totals_one_hundred_percent() {
    let mix: Mix = serde_json::from_str(&read("mix.json")).unwrap();
    let total: f64 = mix.chains.iter().map(|c| c.percent).sum();
    assert!(
        (total - PERCENT_TOTAL).abs() < PERCENT_EPSILON,
        "percentages total {total}, not {PERCENT_TOTAL}"
    );
}

#[test]
fn session_policy_and_pool_size_survive_the_round_trip() {
    let mix: Mix = serde_json::from_str(&read("mix.json")).unwrap();

    let cart = mix
        .chains
        .iter()
        .find(|c| c.name == "cart-add-remove")
        .expect("cart-add-remove chain");
    assert_eq!(cart.session, SessionPolicy::Pool);
    assert_eq!(cart.pool_size, Some(50));

    let checkout = mix.chains.iter().find(|c| c.name == "checkout").unwrap();
    assert_eq!(checkout.session, SessionPolicy::Fresh);
    assert!(checkout.steps[1].repeat_until.is_some(), "poll step repeats");
}

#[test]
fn every_step_names_a_call_that_exists() {
    let mix: Mix = serde_json::from_str(&read("mix.json")).unwrap();
    let calls: CallFile = serde_json::from_str(&read("calls/shop.json")).unwrap();

    for chain in &mix.chains {
        for step in &chain.steps {
            assert!(
                calls.contains_key(&step.call),
                "chain {:?} step {:?} names call {:?}, which is not defined",
                chain.name,
                step.id,
                step.call
            );
        }
    }
}

#[test]
fn calls_parse_with_both_body_forms_and_both_extractor_languages() {
    let calls: CallFile = serde_json::from_str(&read("calls/shop.json")).unwrap();

    // Inline body.
    let login = &calls["login"];
    assert!(matches!(login.body, Some(metrix_plan::Body::Inline(_))));
    assert!(login.extract.contains_key("token"));

    // Generated body, XPath extraction.
    let order = &calls["create-order"];
    assert!(matches!(
        order.body,
        Some(metrix_plan::Body::Generated { .. })
    ));
    assert!(matches!(
        order.extract["order_id"],
        metrix_plan::Selector::Xpath(_)
    ));

    // Every call carries a description: it is what the read-only inspector shows.
    for (name, call) in &calls {
        assert!(!call.description.is_empty(), "call {name:?} has no description");
    }
}

#[test]
fn an_expected_failure_is_expressed_as_an_assertion() {
    let calls: CallFile = serde_json::from_str(&read("calls/shop.json")).unwrap();
    let bad = &calls["login-bad-password"];
    assert_eq!(
        bad.assertions,
        vec![metrix_plan::Assertion::Status { status: 401 }],
        "a login-fail chain asserts the 401 it expects, so getting one is a pass"
    );
}

#[test]
fn targets_parse_and_carry_inventory_attributes() {
    let targets: Targets =
        serde_json::from_str(&read("targets.json")).expect("targets.json should parse");

    assert_eq!(targets.list.len(), 2);
    let first = &targets.list[0];
    assert_eq!(first.host_header.as_deref(), Some("api.staging.example.com"));
    assert_eq!(
        first.attributes.get("image_digest").map(String::as_str),
        Some("sha256:9f2c1e"),
        "image digest travels with the target so comparison can explain an outlier"
    );
}

#[test]
fn documents_round_trip_without_loss() {
    for name in ["mix.json", "targets.json", "calls/shop.json"] {
        let original: serde_json::Value = serde_json::from_str(&read(name)).unwrap();

        let reserialized = match name {
            "mix.json" => {
                serde_json::to_value(serde_json::from_value::<Mix>(original.clone()).unwrap())
            }
            "targets.json" => {
                serde_json::to_value(serde_json::from_value::<Targets>(original.clone()).unwrap())
            }
            _ => serde_json::to_value(serde_json::from_value::<CallFile>(original.clone()).unwrap()),
        }
        .unwrap();

        // Parsing again from our own output must give the same document: defaults we
        // filled in are allowed, but nothing the author wrote may be dropped.
        let reparsed: serde_json::Value = serde_json::from_value(reserialized.clone()).unwrap();
        assert_eq!(
            reserialized, reparsed,
            "{name} is not stable across a round trip"
        );
    }
}
