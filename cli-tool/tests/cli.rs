use assert_cmd::Command;
use predicates::prelude::predicate;
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every test runs hit with HOME pointed at an empty temp directory, so a
/// real ~/.hit.toml or a real saved request can never leak into a test.
fn hit(home: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("hit").unwrap();
    command.env("HOME", home);
    command
}

#[test]
fn no_subcommand_prints_usage() {
    let home = tempdir().unwrap();
    hit(home.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage:"));
}

#[test]
fn a_broken_header_fails_before_the_network() {
    let home = tempdir().unwrap();
    hit(home.path())
        .args(["get", "http://127.0.0.1:9/nowhere", "-H", "no colon here"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --header"));
}

#[tokio::test]
async fn get_reports_the_status_line() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello from the mock"))
        .mount(&server)
        .await;

    let home = tempdir().unwrap();
    hit(home.path())
        .args(["get", &server.uri()])
        .assert()
        .success()
        .stdout(predicate::str::contains("200 OK"))
        .stdout(predicate::str::contains("hello from the mock"));
}

#[tokio::test]
async fn run_replays_a_saved_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/weather"))
        .respond_with(ResponseTemplate::new(200).set_body_string("18C, overcast"))
        .mount(&server)
        .await;

    let home = tempdir().unwrap();
    let url = format!("{}/weather", server.uri());
    hit(home.path())
        .args(["save", "weather", &url])
        .assert()
        .success()
        .stdout(predicate::str::contains("saved weather"));
    hit(home.path())
        .args(["run", "weather"])
        .assert()
        .success()
        .stdout(predicate::str::contains("18C, overcast"));
}

#[tokio::test]
async fn a_batch_reports_every_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/feed"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fresh"))
        .mount(&server)
        .await;

    let home = tempdir().unwrap();
    let url = format!("{}/feed", server.uri());
    hit(home.path())
        .args(["save", "feed-a", &url])
        .assert()
        .success();
    hit(home.path())
        .args(["save", "feed-b", &url])
        .assert()
        .success();

    hit(home.path())
        .args(["run", "feed-a", "feed-b"])
        .assert()
        .success()
        .stdout(predicate::str::contains("feed-a"))
        .stdout(predicate::str::contains("feed-b"))
        .stdout(predicate::str::contains("200 OK"));
}
