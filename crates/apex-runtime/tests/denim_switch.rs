//! Task 8.5 R41: the switch at the fork (`ops/host/denim-switch.sh`), run
//! against a stub `apex denim-detect` and a stub `systemctl`, in a scratch
//! tree: it writes both drop-ins and restarts the canary only on a Denim
//! answer, records what it saw, and does nothing twice.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CANARY: &str = "ExecStart=/opt/apex/bin/apex canary --config ops/canary.denim.yaml";
const EVAL: &str = "ExecStart=/opt/apex/bin/apex armed-eval --config ops/shadow.denim.yaml";

fn ops(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/host").join(file)
}

/// A detect line as `apex denim-detect` prints it, with ten blocks.
fn detect(verdict: &str) -> String {
    let blocks: Vec<serde_json::Value> = (0..10u64)
        .map(|i| serde_json::json!({"number": 53_556_200 + i, "timestamp": 1_793_901_700 + i / 5, "timestampMs": 1_793_901_700_000 + 200 * i}))
        .collect();
    serde_json::json!({"source": "mainnet.base.org", "chain_id": 8453, "blocks": blocks, "verdict": verdict, "reason": null}).to_string()
}

struct Switch {
    dir: tempfile::TempDir,
}

impl Switch {
    /// `answers`: what each detect round prints and exits with, in turn; the
    /// last repeats.
    fn new(answers: &[(i32, String)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("answers"), answers.iter().map(|(c, l)| format!("{c} {l}\n")).collect::<String>()).unwrap();
        let stub = |name: &str, body: &str| {
            let p = d.join(name);
            std::fs::write(&p, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
            Command::new("chmod").arg("+x").arg(&p).status().unwrap();
        };
        stub(
            "apex",
            r#"dir=$(dirname "$0"); n=$(( $(cat "$dir/n" 2>/dev/null || echo 0) + 1 )); echo $n > "$dir/n"
line=$(sed -n "${n}p" "$dir/answers"); [[ -n "$line" ]] || line=$(tail -1 "$dir/answers")
echo "$*" >> "$dir/apex.log"; echo "${line#* }"; exit "${line%% *}""#,
        );
        stub("systemctl", r#"echo "$*" >> "$(dirname "$0")/systemctl.log""#);
        std::fs::create_dir(d.join("etc")).unwrap();
        Self { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn run(&self, extra: &[&str]) -> Output {
        let d = self.dir.path();
        let mut c = Command::new("bash");
        c.arg(ops("denim-switch.sh"))
            .args(["--apex", d.join("apex").to_str().unwrap(), "--systemctl", d.join("systemctl").to_str().unwrap()])
            .args(["--etc", d.join("etc").to_str().unwrap(), "--record", d.join("record.jsonl").to_str().unwrap()])
            .args(["--as-user", "", "--every-s", "0", "--give-up-at", "4000000000"])
            .args(extra);
        c.output().unwrap()
    }

    fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.path(p)).unwrap_or_default()
    }

    fn records(&self) -> Vec<serde_json::Value> {
        self.read("record.jsonl").lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn drop_in(&self, unit: &str) -> String {
        self.read(&format!("etc/{unit}.service.d/denim.conf"))
    }
}

/// The empty `ExecStart=` clears the unit's own before the Denim one.
fn assert_drop_in(text: &str, exec: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.iter().position(|l| *l == exec).unwrap_or_else(|| panic!("no {exec} in {text}"));
    assert_eq!(lines[at - 1], "ExecStart=", "{text}");
    assert!(lines.contains(&"[Service]"), "{text}");
}

#[test]
fn the_switch_writes_both_drop_ins_and_restarts_on_denim() {
    let s = Switch::new(&[(1, detect("not_yet")), (1, detect("not_yet")), (0, detect("denim"))]);
    let out = s.run(&[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_drop_in(&s.drop_in("apex-canary"), CANARY);
    assert_drop_in(&s.drop_in("apex-armed-eval"), EVAL);
    assert_eq!(s.read("systemctl.log"), "daemon-reload\nrestart apex-canary\n");
    let r = s.records();
    assert_eq!(r.len(), 1, "not-yet rounds write nothing: {r:?}");
    assert_eq!(r[0]["event"], "switched");
    assert_eq!(r[0]["detect"]["blocks"].as_array().unwrap().len(), 10);
    assert!(r[0]["restarted_at"].is_string());
    assert!(s.read("apex.log").lines().all(|l| l.starts_with("denim-detect ") && l.contains("--rpc https://mainnet.base.org")));
}

#[test]
fn a_dry_run_writes_nothing() {
    let s = Switch::new(&[(0, detect("denim"))]);
    let out = s.run(&["--dry-run"]);
    assert!(out.status.success());
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(printed.contains(CANARY) && printed.contains(EVAL) && printed.contains("would restart apex-canary"), "{printed}");
    assert_eq!((s.drop_in("apex-canary"), s.read("systemctl.log"), s.read("record.jsonl")), (String::new(), String::new(), String::new()));
    let not_yet = Switch::new(&[(1, detect("not_yet"))]);
    assert_eq!(not_yet.run(&["--dry-run"]).status.code(), Some(1), "one round, and not yet");
}

#[test]
fn nothing_changes_when_base_postpones() {
    let s = Switch::new(&[(1, detect("not_yet"))]);
    let out = s.run(&["--give-up-at", "1"]);
    assert_eq!(out.status.code(), Some(1));
    let r = s.records();
    assert_eq!((r.len(), r[0]["event"].as_str()), (1, Some("gave_up")));
    assert_eq!((s.drop_in("apex-canary"), s.read("systemctl.log")), (String::new(), String::new()));
}

#[test]
fn an_anomaly_is_recorded_and_not_switched() {
    let s = Switch::new(&[(2, detect("anomaly")), (2, detect("anomaly")), (0, detect("denim"))]);
    assert!(s.run(&[]).status.success());
    let events: Vec<String> = s.records().iter().map(|r| r["event"].as_str().unwrap().to_string()).collect();
    assert_eq!(events, vec!["anomaly", "switched"], "the same anomaly twice is one line");
}

#[test]
fn a_second_run_finds_the_drop_ins_and_does_nothing() {
    let s = Switch::new(&[(0, detect("denim"))]);
    assert!(s.run(&[]).status.success());
    let again = s.run(&[]);
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("already switched"));
    assert_eq!(s.read("systemctl.log").lines().count(), 2, "restarted once");
    assert_eq!(s.read("apex.log").lines().count(), 1, "detected once");
}

#[test]
fn a_half_written_switch_is_completed_not_skipped() {
    let s = Switch::new(&[(0, detect("denim"))]);
    std::fs::create_dir_all(s.path("etc/apex-canary.service.d")).unwrap();
    std::fs::write(s.path("etc/apex-canary.service.d/denim.conf"), format!("[Service]\nExecStart=\n{CANARY}\n")).unwrap();
    assert!(s.run(&[]).status.success());
    assert_drop_in(&s.drop_in("apex-armed-eval"), EVAL);
    assert_eq!(s.read("systemctl.log"), "daemon-reload\nrestart apex-canary\n");
}

#[test]
fn the_timer_fires_at_17_59_utc_on_the_fork_day() {
    let timer = std::fs::read_to_string(ops("apex-denim-switch.timer")).unwrap();
    assert!(timer.lines().any(|l| l == "OnCalendar=2026-11-05 17:59:00 UTC"), "{timer}");
    assert!(timer.lines().any(|l| l == "Persistent=true"));
    let service = std::fs::read_to_string(ops("apex-denim-switch.service")).unwrap();
    assert!(service.lines().any(|l| l == "ExecStart=/opt/apex/ops/host/denim-switch.sh"), "{service}");
    assert!(service.lines().any(|l| l == "Type=oneshot"));
}
