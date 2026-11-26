//! `magequery doctor --format github|sarif`: findings as CI annotations, so
//! they appear on the pull request lines that cause them.
//!
//! Paths are relative to the working directory, where CI runs from (the
//! repository root), so they still point at the right file when the Magento
//! root is a subdirectory of the repository.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A repository with the Magento root in `shop/`, and two findings: an error
/// with a file and line (E2001, a preference to a missing class), and a
/// warning with neither (E1002, a module missing from config.php).
fn repo(name: &str) -> PathBuf {
    let repo = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&repo);
    let shop = repo.join("shop");
    write(&shop, "app/etc/config.php", "<?php\nreturn ['modules' => ['Acme_Probe' => 1]];\n");
    write(
        &shop,
        "app/code/Acme/Probe/etc/module.xml",
        "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Probe\"/></config>\n",
    );
    write(
        &shop,
        "app/code/Acme/Probe/Model/ApiInterface.php",
        "<?php\nnamespace Acme\\Probe\\Model;\n\ninterface ApiInterface\n{\n}\n",
    );
    write(
        &shop,
        "app/code/Acme/Probe/etc/di.xml",
        "<?xml version=\"1.0\"?>\n<config>\n\
         <preference for=\"Acme\\Probe\\Model\\ApiInterface\" type=\"Acme\\Probe\\Model\\Gone\"/>\n</config>\n",
    );
    write(
        &shop,
        "app/code/Acme/Loose/etc/module.xml",
        "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Loose\"/></config>\n",
    );
    write(
        &shop,
        "app/code/Acme/Loose/registration.php",
        "<?php\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\
         \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE, 'Acme_Loose', __DIR__);\n",
    );
    repo
}

/// `magequery --root shop doctor`, run from the repository root.
fn doctor(repo: &Path, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_magequery"))
        .current_dir(repo)
        .args(["--root", "shop", "doctor", "--no-baseline"])
        .args(extra)
        .output()
        .expect("run magequery")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn github_prints_one_workflow_command_per_finding() {
    let repo = repo("mq-doctor-format-github");
    let out = doctor(&repo, &["--format", "github"]);
    assert_eq!(out.status.code(), Some(1), "an error still fails the run:\n{}", stdout(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().filter(|l| l.starts_with("::")).collect();
    assert_eq!(lines.len(), 2, "{text}");
    let error = lines.iter().find(|l| l.starts_with("::error ")).expect("the error");
    assert!(
        error.starts_with("::error file=shop/app/code/Acme/Probe/etc/di.xml,line=3,title=E2001 "),
        "{error}"
    );
    let warning = lines.iter().find(|l| l.starts_with("::warning ")).expect("the warning");
    assert!(warning.starts_with("::warning title=E1002 "), "no file, so no file=:\n{warning}");
    let summary = String::from_utf8_lossy(&out.stderr);
    assert!(summary.contains("1 error(s), 1 warning(s)"), "the log keeps the count:\n{summary}");
}

#[test]
fn sarif_is_a_sarif_2_1_0_log() {
    let repo = repo("mq-doctor-format-sarif");
    let out = doctor(&repo, &["--format", "sarif"]);
    assert_eq!(out.status.code(), Some(1));
    let log: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    assert_eq!(log["version"], "2.1.0");
    let run = &log["runs"][0];
    assert_eq!(run["tool"]["driver"]["name"], "magequery");
    let results = run["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let error = results.iter().find(|r| r["ruleId"] == "E2001").expect("E2001");
    assert_eq!(error["level"], "error");
    let location = &error["locations"][0]["physicalLocation"];
    assert_eq!(location["artifactLocation"]["uri"], "shop/app/code/Acme/Probe/etc/di.xml");
    assert_eq!(location["region"]["startLine"], 3);
    let warning = results.iter().find(|r| r["ruleId"] == "E1002").expect("E1002");
    assert_eq!(warning["level"], "warning");
    assert!(warning.get("locations").is_none_or(|l| l.as_array().unwrap().is_empty()));
    // Each rule a result names is described, so code scanning can show it.
    let rules = run["tool"]["driver"]["rules"].as_array().unwrap();
    for id in ["E2001", "E1002"] {
        let rule = rules.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("rule {id}"));
        assert!(rule["shortDescription"]["text"].as_str().is_some_and(|t| !t.is_empty()));
    }
}

#[test]
fn json_is_still_the_flag_it_was() {
    let repo = repo("mq-doctor-format-json");
    let flag = stdout(&doctor(&repo, &["--json"]));
    let format = stdout(&doctor(&repo, &["--format", "json"]));
    assert_eq!(flag, format);
    assert_eq!(doctor(&repo, &["--json", "--format", "sarif"]).status.code(), Some(2), "they conflict");
}

#[test]
fn a_stale_baseline_entry_is_annotated_on_the_baseline() {
    let repo = repo("mq-doctor-format-stale");
    write(
        &repo,
        "shop/magequery-baseline.json",
        "{\n  \"version\": 1,\n  \"ignore\": [\n    {\n      \"code\": \"E3001\",\n      \"message\": \"gone\"\n    }\n  ]\n}\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_magequery"))
        .current_dir(&repo)
        .args(["--root", "shop", "doctor", "--format", "github"])
        .output()
        .expect("run magequery");
    let text = stdout(&out);
    let stale = text
        .lines()
        .find(|l| l.contains("title=baseline"))
        .unwrap_or_else(|| panic!("{text}{}", String::from_utf8_lossy(&out.stderr)));
    assert!(stale.starts_with("::error file=shop/magequery-baseline.json,line=4,"), "{stale}");
}

#[test]
fn gitlab_is_a_code_quality_report() {
    let repo = repo("mq-doctor-format-gitlab");
    let out = doctor(&repo, &["--format", "gitlab"]);
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid JSON");
    let issues = report.as_array().expect("an array of issues");
    assert_eq!(issues.len(), 2);
    let error = issues.iter().find(|i| i["check_name"] == "E2001").expect("E2001");
    assert_eq!(error["severity"], "major");
    assert_eq!(error["location"]["path"], "shop/app/code/Acme/Probe/etc/di.xml");
    assert_eq!(error["location"]["lines"]["begin"], 3);
    assert!(error["description"].as_str().is_some_and(|d| d.contains("Acme\\Probe\\Model\\Gone")));
    // GitLab needs a path: a finding without a file sits on config.php, where
    // the module set it is about is declared.
    let warning = issues.iter().find(|i| i["check_name"] == "E1002").expect("E1002");
    assert_eq!(warning["severity"], "minor");
    assert_eq!(warning["location"]["path"], "shop/app/etc/config.php");
    assert_eq!(warning["location"]["lines"]["begin"], 1);
    // Fingerprints tell GitLab which issues are new in a merge request: unique,
    // and the same on the next run.
    let prints: Vec<&str> = issues.iter().map(|i| i["fingerprint"].as_str().unwrap()).collect();
    assert_ne!(prints[0], prints[1]);
    let again: serde_json::Value = serde_json::from_str(&stdout(&doctor(&repo, &["--format", "gitlab"]))).unwrap();
    let again: Vec<&str> = again.as_array().unwrap().iter().map(|i| i["fingerprint"].as_str().unwrap()).collect();
    assert_eq!(prints, again);
}

/// `doctor` with no `--format`, in the environment given.
fn doctor_in(repo: &Path, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magequery"));
    cmd.current_dir(repo).args(["--root", "shop", "doctor", "--no-baseline"]);
    for var in ["GITHUB_ACTIONS", "GITLAB_CI", "CI"] {
        cmd.env_remove(var);
    }
    cmd.envs(env.iter().copied());
    cmd.output().expect("run magequery")
}

#[test]
fn github_actions_gets_annotations_without_asking() {
    let repo = repo("mq-doctor-format-auto-github");
    let text = stdout(&doctor_in(&repo, &[("GITHUB_ACTIONS", "true")]));
    assert!(text.lines().any(|l| l.starts_with("::error file=shop/")), "{text}");
}

#[test]
fn elsewhere_the_default_stays_text() {
    let repo = repo("mq-doctor-format-auto-text");
    for env in [&[][..], &[("GITLAB_CI", "true")][..]] {
        let text = stdout(&doctor_in(&repo, env));
        assert!(text.contains("error[E2001]"), "{env:?}:\n{text}");
        assert!(!text.contains("::error"), "{env:?}:\n{text}");
    }
    // An explicit text format wins over the detection.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magequery"));
    cmd.current_dir(&repo).env("GITHUB_ACTIONS", "true");
    let out = cmd.args(["--root", "shop", "doctor", "--no-baseline", "--format", "text"]).output().unwrap();
    assert!(stdout(&out).contains("error[E2001]"));
}
