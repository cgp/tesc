mod support;
use metrix_engine::{Output, Plan, run_with_output};
use metrix_metrics::Record;
use metrix_mock::{Config, MockServer};
use serde_json::json;
use std::{fs, future::pending};

#[tokio::test]
async fn three_targets_have_separate_ordered_windows_and_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let mut targets = Vec::new();
    let mut tasks = Vec::new();
    for id in ["a", "b", "c"] {
        let server = MockServer::bind("127.0.0.1:0".parse().unwrap(), Config::default())
            .await
            .unwrap();
        targets.push(json!({"id": id, "address": server.local_addr().unwrap().to_string()}));
        tasks.push(tokio::spawn(server.run_until(pending())));
    }
    support::bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    support::write(
        dir.path(),
        "targets.json",
        &json!({"gap": "1s", "list": targets}),
    );
    let plan = Plan::load(dir.path()).unwrap();
    let path = dir.path().join("summary.ndjson");
    let output = Output::open(&plan, &path, None, 1.0, 0).unwrap();
    run_with_output(plan, pending(), &output).await.unwrap();
    assert!(!output.finish(0, None).failed);
    let records: Vec<Record> = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut finished = 0;
    let mut starts = Vec::new();
    for record in records {
        match record {
            Record::TargetStarted(r) => {
                assert_eq!(r.index as usize, starts.len() + 1);
                assert_eq!(r.total, 3);
                if !starts.is_empty() {
                    assert!(r.t_ms >= finished + 995);
                }
                starts.push(r.target_id);
            }
            Record::TargetFinished(r) => {
                assert!(r.completed);
                finished = r.t_ms;
            }
            Record::Summary(r) => assert_eq!(Some(&r.target_id), starts.last()),
            _ => {}
        }
    }
    assert_eq!(starts, ["a", "b", "c"]);
    for task in tasks {
        task.abort();
    }
}

#[test]
fn invalid_later_targets_are_rejected_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    support::bundle(dir.path(), "127.0.0.1:1".parse().unwrap(), "http1");
    support::edit(dir.path(), "targets.json", |doc| {
        doc["list"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"bad", "address":"missing-port"}))
    });
    assert!(
        Plan::load(dir.path())
            .err()
            .unwrap()
            .contains("/list/1/address")
    );
}
