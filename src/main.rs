//! Trindle crash reports: sends the crash and error notes Trindle's programs leave, once an hour.

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const API: &str = "https://buildmorphic.com/api/reports";
const REPOSITORIES: &[&str] = &[
    "trindle", "trindle-toolkit", "trindle-flow", "trindle-scan", "trindle-talk", "trindle-voice",
    "trindle-pocketkey", "trindle-snippets", "trindle-icons", "trindle-screen-reader",
    "trindle-crash-reports", "trindle-downloader", "trindle-translations",
];
const MAIN: &str = "trindle";
const PROTO: &str = if cfg!(test) { "=http,https" } else { "=https" };
const START_ON: bool = true;
const EVERY: u64 = 55 * 60;
const TRIES: u64 = 24;
const KEEP_SENT: usize = 50;
const NOTES_MAX: u64 = 1 << 20;
const WAITING_MAX: usize = 200;
const FRAMES: usize = 32;
const SHORT_LONGEST: usize = 40;
const FN_LONGEST: usize = 200;
const SIGNALS: &[&str] = &["SIGSEGV", "SIGABRT", "SIGBUS", "SIGILL", "SIGFPE", "SIGTRAP", "SIGSYS", "panic", "out-of-memory"];
const MEMORY: &[u64] = &[1, 2, 4, 8, 16, 32, 64, 128];

struct Places {
    switches: PathBuf,
    notes: PathBuf,
    taking: PathBuf,
    waiting: PathBuf,
    sent: PathBuf,
    last: PathBuf,
    lock: PathBuf,
    settings: PathBuf,
}

impl Places {
    fn under(home: &Path) -> Places {
        let state = home.join(".local/state/trindle");
        let own = state.join("crash-reports");
        Places {
            switches: home.join(".config/trindle/crash-reports.json"),
            notes: state.join("crash-notes.jsonl"),
            taking: state.join("crash-notes.taking"),
            waiting: own.join("waiting.json"),
            sent: own.join("sent.jsonl"),
            last: own.join("last-sent"),
            lock: own.join("lock"),
            settings: home.join(".config/trindle/trindle.conf"),
        }
    }
}

#[derive(Clone, Copy)]
struct Switches {
    crashes: bool,
    errors: bool,
    computer: bool,
}

impl Switches {
    fn read(path: &Path) -> Switches {
        let v: Value = fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let on = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(START_ON);
        Switches { crashes: on("crashes"), errors: on("errors"), computer: on("computer") }
    }

    fn allows(&self, kind: &str) -> bool {
        match kind {
            "crash" => self.crashes,
            "error" => self.errors,
            _ => false,
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("send") | None => {
            let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else { std::process::exit(1) };
            if unsafe { getuid() } == 0 {
                return;
            }
            let at = |slug: &str| format!("{API}/{slug}");
            let places = Places::under(Path::new(&home));
            let Some(_one_at_a_time) = lock(&places) else { return };
            run(&places, now(), &|slug| accepting(&at(slug)), &|slug, body| post(&at(slug), body));
        }
        _ => {
            eprintln!("trindle-crash-reports send: take Trindle's crash notes and send the ones switched on");
            std::process::exit(2);
        }
    }
}

extern "C" {
    fn getuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
}

fn lock(at: &Places) -> Option<fs::File> {
    let _ = fs::create_dir_all(at.lock.parent()?);
    let file = fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&at.lock).ok()?;
    const EXCLUSIVE_NO_WAIT: i32 = 2 | 4;
    (unsafe { flock(file.as_raw_fd(), EXCLUSIVE_NO_WAIT) } == 0).then_some(file)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn run(at: &Places, time: u64, taking: &dyn Fn(&str) -> bool, send: &dyn Fn(&str, &str) -> u16) {
    let switches = Switches::read(&at.switches);
    let mut waiting = read_waiting(&at.waiting);
    for note in take_notes(at) {
        let from = note["kind"].as_str().unwrap_or("");
        if let Some(report) = report_from(&note).filter(|_| switches.allows(from)) {
            add(&mut waiting, from, report);
        }
    }
    waiting.retain(|w| switches.allows(w["from"].as_str().unwrap_or("")));
    let last = fs::read_to_string(&at.last).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    if !waiting.is_empty() && time.saturating_sub(last) >= EVERY {
        let extra = extras(at, &switches);
        let mut open: HashMap<String, bool> = HashMap::new();
        let mut stopped = false;
        let mut sent = Vec::new();
        waiting.retain_mut(|w| {
            if stopped {
                return true;
            }
            let slug = w["report"]["project"].as_str().unwrap_or(MAIN).to_string();
            if *open.entry(slug.clone()).or_insert_with(|| taking(&slug)) {
                let report = body(w, &extra);
                let answer = send(&slug, &report.to_string());
                if matches!(answer, 201 | 400 | 403 | 413) {
                    sent.push(json!({ "when": time, "answer": answer, "report": report }));
                    return false;
                }
                stopped = matches!(answer, 0 | 429 | 500..=599);
            }
            let tries = w["tries"].as_u64().unwrap_or(0) + 1;
            w["tries"] = tries.into();
            tries < TRIES
        });
        keep_sent(&at.sent, sent);
        write(&at.last, time.to_string().as_bytes());
    }
    if write(&at.waiting, Value::Array(waiting).to_string().as_bytes()) {
        let _ = fs::remove_file(&at.taking);
    }
}

fn take_notes(at: &Places) -> Vec<Value> {
    if !at.taking.exists() && fs::rename(&at.notes, &at.taking).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    let _ = fs::File::open(&at.taking).and_then(|f| f.take(NOTES_MAX).read_to_end(&mut bytes));
    String::from_utf8_lossy(&bytes).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn report_from(note: &Value) -> Option<Value> {
    let kind = note["kind"].as_str()?;
    let what = note["what"].as_str()?;
    let program = note["program"].as_str().filter(|p| is_program(p))?;
    let version = note["version"].as_str().filter(|v| is_version(v))?;
    match kind {
        "crash" if SIGNALS.contains(&what) => {}
        "error" if is_code(what) => {}
        _ => return None,
    }
    let prefix = if program == "trindle-compose" { "trindle_compose::" } else { "trindle::" };
    let file = note["file"].as_str().filter(|f| is_own_file(f));
    let line = note["line"].as_u64().filter(|l| *l > 0);
    let mut frames: Vec<String> = Vec::new();
    for name in note["frames"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !is_rust_path(name) {
            continue;
        }
        let name = clean_fn(&format!("{prefix}{name}"));
        if name.is_empty() {
            continue;
        }
        frames.push(match (frames.is_empty(), file, line) {
            (true, Some(f), Some(l)) => format!("{name} ({f}:{l})"),
            _ => name,
        });
        if frames.len() == FRAMES {
            break;
        }
    }
    let mut crash = Map::new();
    crash.insert("program".into(), program.into());
    crash.insert("signal".into(), what.into());
    if !frames.is_empty() {
        crash.insert("trace".into(), frames.join("\n").into());
    }
    Some(json!({ "kind": "crash", "project": slug_for(program), "version": version, "crash": crash }))
}

fn slug_for(program: &str) -> &'static str {
    REPOSITORIES.iter().find(|r| **r == program).unwrap_or(&MAIN)
}

fn is_program(p: &str) -> bool {
    let mut chars = p.chars();
    p.len() <= 64
        && (p == "trindle" || p.starts_with("trindle-"))
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._+-".contains(c))
}

fn is_version(v: &str) -> bool {
    (1..=SHORT_LONGEST).contains(&v.len()) && v.chars().all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c))
}

fn is_code(c: &str) -> bool {
    (1..=SHORT_LONGEST).contains(&c.len()) && c.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_own_file(f: &str) -> bool {
    f.starts_with("src/")
        && f.len() <= 200
        && !f.split('/').any(|part| part.is_empty() || part == "..")
        && f.chars().all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c))
}

fn is_rust_path(name: &str) -> bool {
    let bare = name.replace(" as ", "").replace(", ", ",").replace("<impl ", "<");
    name.len() <= FN_LONGEST
        && bare.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || "_<{&*[(".contains(c))
        && bare.chars().all(|c| c.is_ascii_alphanumeric() || "_:<>(),&*[];{}".contains(c))
}

fn clean_fn(name: &str) -> String {
    let kept: String = name.chars().filter(|c| !c.is_control() && !"/\\@~\"'`%".contains(*c)).collect();
    let spaced = kept.replace(" as ", "\u{1}").replace(", ", "\u{2}").replace(' ', "_");
    let back = spaced.replace('\u{1}', " as ").replace('\u{2}', ", ");
    back.chars().take(FN_LONGEST).collect::<String>().trim_end().to_string()
}

fn read_waiting(path: &Path) -> Vec<Value> {
    let all: Vec<Value> = fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    all.into_iter().filter(|w| w["report"]["crash"].is_object() && w["from"].is_string()).take(WAITING_MAX).collect()
}

fn add(waiting: &mut Vec<Value>, from: &str, report: Value) {
    if waiting.len() < WAITING_MAX && !waiting.iter().any(|w| w["report"] == report) {
        waiting.push(json!({ "from": from, "report": report, "tries": 0 }));
    }
}

fn body(waiting: &Value, extra: &Map<String, Value>) -> Value {
    let mut body = waiting["report"].clone();
    body.as_object_mut().into_iter().for_each(|b| b.extend(extra.clone()));
    body
}

fn extras(at: &Places, switches: &Switches) -> Map<String, Value> {
    let mut extra = Map::new();
    if let Some(text) = switches.computer.then(computer).and_then(|c| hardware_text(&c)) {
        extra.insert("hardware".into(), text.into());
    }
    let username = saved(&at.settings, "report_username").map(|n| n.to_lowercase()).filter(|n| is_username(n));
    let github = saved(&at.settings, "report_github").filter(|n| is_github(n));
    for (field, name) in [("username", username), ("github", github)] {
        if let Some(name) = name {
            extra.insert(field.into(), name.into());
        }
    }
    extra
}

fn saved(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let found = text.lines().filter(|l| !l.trim_start().starts_with('#')).filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim().to_string()).next_back();
    found.filter(|v| !v.is_empty())
}

fn is_username(n: &str) -> bool {
    (2..=31).contains(&n.len()) && !n.starts_with('-') && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_github(n: &str) -> bool {
    (1..=39).contains(&n.len()) && !n.starts_with('-') && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn accepting(address: &str) -> bool {
    let (answer, said) = request(&format!("{address}/status"), None);
    answer == 200 && serde_json::from_str::<Value>(&said).ok().and_then(|v| v["accepting"].as_bool()) == Some(true)
}

fn post(address: &str, body: &str) -> u16 {
    request(address, Some(body)).0
}

fn request(url: &str, body: Option<&str>) -> (u16, String) {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "--proto", PROTO, "--noproxy", "127.0.0.1", "--max-time", "20", "-A", "trindle-crash-reports",
              "-H", "Accept: application/json", "-w", "\n%{http_code}"]);
    if body.is_some() {
        cmd.args(["-H", "Content-Type: application/json", "--data-binary", "@-"]);
    }
    let child = cmd.arg(url).stdin(if body.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let Ok(mut child) = child else { return (0, String::new()) };
    if let (Some(mut input), Some(body)) = (child.stdin.take(), body) {
        let _ = input.write_all(body.as_bytes());
    }
    let out = child.wait_with_output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let (said, code) = out.rsplit_once('\n').unwrap_or(("", &out));
    (code.trim().parse().unwrap_or(0), said.to_string())
}

fn keep_sent(path: &Path, new: Vec<Value>) {
    if new.is_empty() {
        return;
    }
    let old = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().map(str::to_string).collect();
    lines.extend(new.iter().map(Value::to_string));
    let from = lines.len().saturating_sub(KEEP_SENT);
    write(path, (lines[from..].join("\n") + "\n").as_bytes());
}

fn write(path: &Path, bytes: &[u8]) -> bool {
    let Some(dir) = path.parent() else { return false };
    let _ = fs::create_dir_all(dir);
    let temp = path.with_extension("new");
    let written = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temp)
        .and_then(|mut f| f.write_all(bytes));
    written.is_ok() && fs::rename(&temp, path).is_ok()
}

fn computer() -> Value {
    let mut c = Map::new();
    let release = fs::read_to_string("/etc/os-release").or_else(|_| fs::read_to_string("/usr/lib/os-release")).unwrap_or_default();
    if let Some(os) = os_from(&release) {
        c.insert("os".into(), os.into());
    }
    if let Some(arch) = arch_from(std::env::consts::ARCH) {
        c.insert("arch".into(), arch.into());
    }
    if let Some(gb) = fs::read_to_string("/proc/meminfo").ok().and_then(|m| memory_from(&m)) {
        c.insert("memory_gb".into(), gb.into());
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    c.insert("session".into(), session_from(var("WAYLAND_DISPLAY"), var("DISPLAY"), var("XDG_SESSION_TYPE")).into());
    let (vendors, screens) = graphics();
    c.insert("gpu".into(), gpu_from(&vendors).into());
    c.insert("screens".into(), screens.min(4).into());
    Value::Object(c)
}

fn hardware_text(c: &Value) -> Option<String> {
    let word = |k: &str| c[k].as_str().map(str::to_string);
    let number = |k: &str| c[k].as_u64();
    let parts: Vec<String> = [
        word("os"),
        word("arch"),
        number("memory_gb").map(|g| format!("{g} GB")),
        word("session"),
        word("gpu").map(|g| format!("{g} graphics")),
        number("screens").map(|n| format!("{n} screen{}", if n == 1 { "" } else { "s" })),
    ].into_iter().flatten().collect();
    if parts.is_empty() {
        return None;
    }
    let mut text = parts.join(", ");
    for (k, v) in c.as_object()? {
        text.push_str(&format!("\n{k}: {}", v.as_str().map_or_else(|| v.to_string(), str::to_string)));
    }
    Some(text)
}

fn os_from(release: &str) -> Option<String> {
    let field = |k: &str| release.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(|v| v.trim_matches('"').to_string());
    let id = field("ID").filter(|i| (1..=24).contains(&i.len()) && i.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))?;
    let major = field("VERSION_ID").and_then(|v| v.split('.').next().map(str::to_string)).filter(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()));
    Some(match major {
        Some(m) => format!("{} {m}", id.to_lowercase()),
        None => id.to_lowercase(),
    })
}

fn arch_from(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" => "armhf",
        "x86" => "i686",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

fn memory_from(meminfo: &str) -> Option<u64> {
    let kb: u64 = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.split_whitespace().next()?.parse().ok()?;
    MEMORY.iter().rev().find(|m| kb * 4 >= **m * 3 * 1024 * 1024).or(MEMORY.first()).copied()
}

fn session_from(wayland: Option<String>, x: Option<String>, kind: Option<String>) -> &'static str {
    match (wayland, x, kind.as_deref()) {
        (Some(_), _, _) | (_, _, Some("wayland")) => "wayland",
        (_, Some(_), _) | (_, _, Some("x11")) => "x11",
        _ => "none",
    }
}

fn graphics() -> (Vec<String>, u64) {
    let mut vendors = Vec::new();
    let mut screens = 0;
    for entry in fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("card") else { continue };
        if rest.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(v) = fs::read_to_string(entry.path().join("device/vendor")) {
                vendors.push(v.trim().to_string());
            }
        } else if fs::read_to_string(entry.path().join("status")).is_ok_and(|s| s.trim() == "connected") {
            screens += 1;
        }
    }
    (vendors, screens)
}

fn gpu_from(vendors: &[String]) -> &'static str {
    let maker = |v: &str| match v {
        "0x8086" => "intel",
        "0x1002" => "amd",
        "0x10de" => "nvidia",
        "0x1af4" | "0x1234" | "0x15ad" | "0x80ee" | "0x1b36" => "virtual",
        _ => "other",
    };
    ["nvidia", "amd", "intel", "virtual", "other"].into_iter()
        .find(|m| vendors.iter().any(|v| maker(v) == *m))
        .unwrap_or("none")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn note(kind: &str, what: &str, program: &str, file: Option<&str>, frames: &[&str]) -> Value {
        json!({ "kind": kind, "what": what, "program": program, "version": "1.0.16",
                "file": file, "line": file.map(|_| 42), "frames": frames })
    }

    const FIELDS: &[&str] = &["kind", "project", "version", "words", "hardware", "crash", "log", "username", "github"];
    const CRASH_FIELDS: &[&str] = &["program", "signal", "trace"];

    fn only_allowed_fields(report: &Value) {
        for k in report.as_object().unwrap().keys() {
            assert!(FIELDS.contains(&k.as_str()), "{k} is not a field Buildmorphic takes");
        }
        for k in report["crash"].as_object().unwrap().keys() {
            assert!(CRASH_FIELDS.contains(&k.as_str()), "crash.{k} is not a field Buildmorphic takes");
        }
    }

    #[test]
    fn a_panic_becomes_a_report_with_its_place() {
        let r = report_from(&note("crash", "panic", "trindle-ring", Some("src/ring/view.rs"), &["ring::view::open", "main"])).unwrap();
        assert_eq!(r, json!({ "kind": "crash", "project": "trindle", "version": "1.0.16",
            "crash": { "program": "trindle-ring", "signal": "panic", "trace": "trindle::ring::view::open (src/ring/view.rs:42)\ntrindle::main" } }));
        only_allowed_fields(&r);
    }

    #[test]
    fn the_compositor_gets_its_own_prefix() {
        let r = report_from(&note("crash", "panic", "trindle-compose", None, &["udev::run"])).unwrap();
        assert_eq!(r["crash"]["trace"], "trindle_compose::udev::run");
    }

    #[test]
    fn a_signal_note_has_no_trace() {
        let r = report_from(&note("crash", "SIGSEGV", "trindle-panel", None, &[])).unwrap();
        assert!(r["crash"].get("trace").is_none());
        assert_eq!(r["crash"]["signal"], "SIGSEGV");
        only_allowed_fields(&r);
    }

    #[test]
    fn an_error_is_a_crash_with_its_code_as_the_signal() {
        let r = report_from(&note("error", "E_SAVE_FAILED", "trindle-ring", None, &["ring::store::save"])).unwrap();
        assert_eq!(r["kind"], "crash");
        assert_eq!(r["crash"]["signal"], "E_SAVE_FAILED");
        only_allowed_fields(&r);
    }

    #[test]
    fn a_report_goes_to_the_repository_that_owns_the_program() {
        for repo in REPOSITORIES {
            assert_eq!(slug_for(repo), *repo);
        }
        for program in ["trindle-ring", "trindle-compose", "trindle-panel", "trindle-flows", "trindle-toolkit2", "other"] {
            assert_eq!(slug_for(program), "trindle", "{program}");
        }
        let r = report_from(&note("crash", "panic", "trindle-flow", None, &["a"])).unwrap();
        assert_eq!(r["project"], "trindle-flow");
        assert_eq!(report_from(&note("crash", "panic", "trindle-compose", None, &["a"])).unwrap()["project"], "trindle");
    }

    #[test]
    fn the_limits_are_kept() {
        let mut n = note("crash", "panic", "trindle-ring", None, &[]);
        n["version"] = "1".repeat(41).into();
        assert!(report_from(&n).is_none());
        n["version"] = "1".repeat(40).into();
        assert!(report_from(&n).is_some());
        assert!(report_from(&note("error", &"E".repeat(41), "trindle-ring", None, &[])).is_none());
        let long = "x".repeat(FN_LONGEST);
        let names: Vec<&str> = (0..FRAMES + 5).map(|_| long.as_str()).collect();
        let r = report_from(&note("crash", "panic", "trindle-ring", Some("src/ring/view.rs"), &names)).unwrap();
        assert!(r["crash"]["trace"].as_str().unwrap().chars().count() <= 30000);
    }

    #[test]
    fn anything_unexpected_is_refused() {
        assert!(report_from(&note("crash", "a message with /home/john", "trindle-ring", None, &[])).is_none());
        assert!(report_from(&note("error", "could not open /home/john", "trindle-ring", None, &[])).is_none());
        assert!(report_from(&note("crash", "panic", "Trindle Ring", None, &[])).is_none());
        assert!(report_from(&note("other", "panic", "trindle-ring", None, &[])).is_none());
        let mut n = note("crash", "panic", "trindle-ring", None, &[]);
        n["version"] = "1.0 beta".into();
        assert!(report_from(&n).is_none());
    }

    #[test]
    fn places_outside_the_project_are_never_sent() {
        for f in ["/home/john/.cargo/registry/src/gtk/lib.rs", "src/../../etc/passwd", "library/std/src/lib.rs", "src//a.rs"] {
            let r = report_from(&note("crash", "panic", "trindle-ring", Some(f), &["ring::open"])).unwrap();
            assert_eq!(r["crash"]["trace"], "trindle::ring::open", "{f}");
        }
    }

    #[test]
    fn function_names_follow_the_rules() {
        assert_eq!(clean_fn("trindle_compose::udev::<impl trindle_compose::state::AnvilState<trindle_compose::udev::UdevData>>::render_surface"),
            "trindle_compose::udev::<impl_trindle_compose::state::AnvilState<trindle_compose::udev::UdevData>>::render_surface");
        assert_eq!(clean_fn("<T as core::fmt::Debug>::fmt"), "<T as core::fmt::Debug>::fmt");
        assert_eq!(clean_fn("f<(A, B)>"), "f<(A, B)>");
        assert_eq!(clean_fn("a/b\\c@d~e\"f'g%h"), "abcdefgh");
        assert_eq!(clean_fn(&"x".repeat(500)).len(), FN_LONGEST);
    }

    #[test]
    fn no_more_than_32_frames() {
        let many: Vec<String> = (0..50).map(|i| format!("f{i}")).collect();
        let names: Vec<&str> = many.iter().map(String::as_str).collect();
        let r = report_from(&note("crash", "panic", "trindle-ring", None, &names)).unwrap();
        assert_eq!(r["crash"]["trace"].as_str().unwrap().lines().count(), 32);
    }

    #[test]
    fn the_same_fault_waits_once() {
        let r = report_from(&note("crash", "panic", "trindle-ring", None, &["a"])).unwrap();
        let other = report_from(&note("crash", "SIGABRT", "trindle-ring", None, &[])).unwrap();
        let mut waiting = Vec::new();
        for _ in 0..150 {
            add(&mut waiting, "crash", r.clone());
        }
        add(&mut waiting, "crash", other);
        assert_eq!(waiting.len(), 2);
        assert!(body(&waiting[0], &Map::new()).get("count").is_none());
    }

    #[test]
    fn the_computer_is_one_line_then_the_parts() {
        let c = json!({ "os": "debian 13", "arch": "x86_64", "memory_gb": 16, "session": "wayland", "gpu": "intel", "screens": 1 });
        let text = hardware_text(&c).unwrap();
        assert_eq!(text.lines().next().unwrap(), "debian 13, x86_64, 16 GB, wayland, intel graphics, 1 screen");
        assert!(text.lines().any(|l| l == "memory_gb: 16") && text.lines().any(|l| l == "os: debian 13"));
        assert!(hardware_text(&json!({})).is_none());
    }

    #[test]
    fn names_follow_buildmorphics_rules() {
        for ok in ["john-smith", "js", "a1"] {
            assert!(is_username(ok), "{ok}");
        }
        for bad in ["j", "-john", "john_smith", "john.smith", "john@example.com", "John", &"a".repeat(32), ""] {
            assert!(!is_username(bad), "{bad}");
        }
        for ok in ["John-Smith", "j", &"a".repeat(39)] {
            assert!(is_github(ok), "{ok}");
        }
        for bad in ["-j", "john_smith", "john@example.com", &"a".repeat(40), ""] {
            assert!(!is_github(bad), "{bad}");
        }
    }

    #[test]
    fn about_this_computer() {
        assert_eq!(os_from("NAME=\"Debian GNU/Linux\"\nID=debian\nVERSION_ID=\"13\"\n").unwrap(), "debian 13");
        assert_eq!(os_from("ID=ubuntu\nVERSION_ID=\"24.04\"\n").unwrap(), "ubuntu 24");
        assert_eq!(os_from("ID=\"arch\"\n").unwrap(), "arch");
        assert!(os_from("ID=\"a b\"\n").is_none());
        assert_eq!(memory_from("MemTotal:       32155852 kB\n"), Some(32));
        assert_eq!(memory_from("MemTotal:       15800000 kB\n"), Some(16));
        assert_eq!(memory_from("MemTotal:       24000000 kB\n"), Some(16));
        assert_eq!(memory_from("MemTotal:       12000000 kB\n"), Some(8));
        assert_eq!(memory_from("MemTotal:       500000 kB\n"), Some(1));
        assert_eq!(session_from(Some("wayland-1".into()), None, None), "wayland");
        assert_eq!(session_from(None, Some(":0".into()), None), "x11");
        assert_eq!(session_from(None, None, Some("tty".into())), "none");
        assert_eq!(gpu_from(&["0x8086".into(), "0x10de".into()]), "nvidia");
        assert_eq!(gpu_from(&["0x1af4".into()]), "virtual");
        assert_eq!(gpu_from(&[]), "none");
        assert_eq!(arch_from("x86_64"), Some("x86_64"));
        assert!(os_from(&format!("ID={}\n", "a".repeat(25))).is_none());
    }

    fn home(name: &str) -> (PathBuf, Places) {
        let dir = std::env::temp_dir().join(format!("crash-reports-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let places = Places::under(&dir);
        (dir, places)
    }

    fn leave_notes(at: &Places, notes: &[Value]) {
        fs::create_dir_all(at.notes.parent().unwrap()).unwrap();
        let text: String = notes.iter().map(|n| n.to_string() + "\n").collect();
        fs::write(&at.notes, text).unwrap();
    }

    fn switches(at: &Places, crashes: bool, errors: bool, computer: bool) {
        fs::create_dir_all(at.switches.parent().unwrap()).unwrap();
        fs::write(&at.switches, json!({ "crashes": crashes, "errors": errors, "computer": computer }).to_string()).unwrap();
    }

    fn names(at: &Places, text: &str) {
        fs::create_dir_all(at.settings.parent().unwrap()).unwrap();
        fs::write(&at.settings, text).unwrap();
    }

    fn run_with(at: &Places, time: u64, open: &[&str], answer: u16) -> Vec<(String, Value)> {
        let sent = RefCell::new(Vec::new());
        run(at, time, &|slug| open.contains(&slug), &|slug, b| {
            sent.borrow_mut().push((slug.to_string(), serde_json::from_str(b).unwrap()));
            answer
        });
        sent.into_inner()
    }

    #[test]
    fn nothing_is_sent_with_the_switches_off() {
        let (dir, at) = home("off");
        leave_notes(&at, &[note("crash", "panic", "trindle-ring", None, &["a"])]);
        switches(&at, false, false, false);
        assert!(run_with(&at, 10_000, &["trindle"], 201).is_empty());
        assert!(!at.notes.exists());
        assert_eq!(read_waiting(&at.waiting).len(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn each_report_goes_to_its_own_repository_and_only_when_it_is_taking_them() {
        let (dir, at) = home("slugs");
        switches(&at, true, true, false);
        leave_notes(&at, &[note("crash", "panic", "trindle-ring", None, &["a"]), note("crash", "SIGSEGV", "trindle-flow", None, &["b"]),
                           note("error", "E_X", "trindle-scan", None, &[])]);
        let asked = RefCell::new(Vec::new());
        let sent = RefCell::new(Vec::new());
        run(&at, 10_000, &|slug| { asked.borrow_mut().push(slug.to_string()); slug != "trindle-flow" },
            &|slug, b| { sent.borrow_mut().push((slug.to_string(), serde_json::from_str::<Value>(b).unwrap())); 201 });
        let slugs: Vec<_> = sent.borrow().iter().map(|(s, b)| (s.clone(), b["project"].as_str().unwrap().to_string())).collect();
        assert_eq!(slugs, [("trindle".to_string(), "trindle".to_string()), ("trindle-scan".to_string(), "trindle-scan".to_string())]);
        assert_eq!(*asked.borrow(), ["trindle", "trindle-flow", "trindle-scan"]);
        let waiting = read_waiting(&at.waiting);
        assert_eq!(waiting.len(), 1, "the report for a repository that is not taking them waits");
        assert_eq!(waiting[0]["report"]["project"], "trindle-flow");
        assert_eq!(waiting[0]["tries"], 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn every_report_sent_has_only_the_fields_buildmorphic_takes() {
        let (dir, at) = home("fields");
        switches(&at, true, true, true);
        names(&at, "# kept by Trindle Settings\nreport_username=John-Smith\nreport_github=john-smith\n");
        leave_notes(&at, &[note("crash", "panic", "trindle-ring", Some("src/ring/view.rs"), &["ring::open"]),
                           note("error", "E_SAVE_FAILED", "trindle-ring", None, &["ring::store::save"]),
                           note("crash", "SIGSEGV", "trindle-voice", None, &[])]);
        let sent = run_with(&at, 10_000, &["trindle", "trindle-voice"], 201);
        assert_eq!(sent.len(), 3);
        for (slug, b) in &sent {
            only_allowed_fields(b);
            assert_eq!(b["project"], slug.as_str());
            assert_eq!(b["username"], "john-smith");
            assert_eq!(b["github"], "john-smith");
            assert!(b["hardware"].as_str().unwrap().lines().next().unwrap().len() <= 80);
        }
        let log = fs::read_to_string(&at.sent).unwrap();
        let entry: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(entry["report"], sent[0].1);
        assert_eq!(entry["answer"], 201);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn names_and_the_computer_are_left_out_unless_wanted_and_fit() {
        let (dir, at) = home("names");
        switches(&at, true, true, false);
        names(&at, "report_username=john@example.com\nreport_github=john_smith\n");
        leave_notes(&at, &[note("crash", "panic", "trindle-ring", None, &["a"])]);
        let b = &run_with(&at, 10_000, &["trindle"], 201)[0].1;
        only_allowed_fields(b);
        for k in ["username", "github", "hardware"] {
            assert!(b.get(k).is_none(), "{k}");
        }
        assert!(!fs::read_to_string(&at.sent).unwrap().contains('@'));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_run_sends_each_fault_once_then_waits_an_hour() {
        let (dir, at) = home("once");
        switches(&at, true, false, false);
        let crash = note("crash", "panic", "trindle-ring", Some("src/ring/view.rs"), &["ring::open"]);
        leave_notes(&at, &[crash.clone(), crash.clone(), note("error", "E_SAVE_FAILED", "trindle-ring", None, &[])]);
        assert_eq!(run_with(&at, 10_000, &["trindle"], 201).len(), 1);

        leave_notes(&at, std::slice::from_ref(&crash));
        assert!(run_with(&at, 10_000 + 60, &["trindle"], 201).is_empty(), "sent again within the hour");
        assert_eq!(run_with(&at, 10_000 + EVERY, &["trindle"], 201).len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refusal_drops_and_an_outage_retries_a_while() {
        let (dir, at) = home("answers");
        switches(&at, true, true, true);
        for refused in [400, 403, 413] {
            leave_notes(&at, &[note("crash", "SIGSEGV", "trindle-ring", None, &[])]);
            run_with(&at, 10_000, &["trindle"], refused);
            assert_eq!(read_waiting(&at.waiting).len(), 0, "{refused}");
            assert!(fs::read_to_string(&at.sent).unwrap().contains(&format!("\"answer\":{refused}")));
            let _ = fs::remove_file(&at.last);
        }

        for waits in [0, 429, 503] {
            let _ = fs::remove_file(&at.waiting);
            leave_notes(&at, &[note("crash", "SIGBUS", "trindle-ring", None, &[])]);
            let mut time = 20_000;
            for _ in 0..TRIES - 1 {
                run_with(&at, time, &["trindle"], waits);
                time += EVERY;
            }
            assert_eq!(read_waiting(&at.waiting).len(), 1, "{waits}");
            run_with(&at, time, &["trindle"], waits);
            assert_eq!(read_waiting(&at.waiting).len(), 0, "{waits}");
            let _ = fs::remove_file(&at.last);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn notes_and_waiting_are_capped() {
        let (dir, at) = home("caps");
        let line = json!({ "kind": "crash", "what": "SIGBUS", "program": "trindle-ring", "version": "1.0.0" }).to_string() + "\n";
        let lines = (NOTES_MAX as usize / line.len()) * 2;
        leave_notes(&at, &[]);
        fs::write(&at.notes, line.repeat(lines)).unwrap();
        let taken = take_notes(&at).len();
        assert!(taken > 0 && taken < lines);
        let all: Vec<Value> = (0..WAITING_MAX + 50).map(|i| json!({ "from": "crash", "report": { "crash": { "signal": i.to_string() } } })).collect();
        fs::create_dir_all(at.waiting.parent().unwrap()).unwrap();
        fs::write(&at.waiting, Value::Array(all).to_string()).unwrap();
        assert_eq!(read_waiting(&at.waiting).len(), WAITING_MAX);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn too_many_stops_the_run_and_the_rest_wait() {
        let (dir, at) = home("many");
        switches(&at, true, true, false);
        leave_notes(&at, &[note("crash", "SIGBUS", "trindle-ring", None, &[]), note("crash", "SIGILL", "trindle-ring", None, &[])]);
        assert_eq!(run_with(&at, 10_000, &["trindle"], 503).len(), 1);
        let waiting = read_waiting(&at.waiting);
        assert_eq!((waiting.len(), waiting[0]["tries"].as_u64(), waiting[1]["tries"].as_u64()), (2, Some(1), Some(0)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reports_kept_by_version_1_0_0_are_not_sent() {
        let (dir, at) = home("old");
        fs::create_dir_all(at.waiting.parent().unwrap()).unwrap();
        fs::write(&at.waiting, json!([{ "report": { "kind": "crash", "program": "trindle-ring", "signal": "panic" }, "count": 2, "tries": 0 }]).to_string()).unwrap();
        assert!(read_waiting(&at.waiting).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn text_that_is_not_a_function_path_is_not_sent() {
        let r = report_from(&note("crash", "panic", "trindle-ring", None,
            &["Alice Smith confidential diagnosis", "ring::open", "call me on 07700 900123", "<T as core::fmt::Debug>::fmt", "x::{{closure}}"])).unwrap();
        assert_eq!(r["crash"]["trace"], "trindle::ring::open\ntrindle::<T as core::fmt::Debug>::fmt\ntrindle::x::{{closure}}");
        let only_text = report_from(&note("crash", "panic", "trindle-ring", None, &["john@example.com wrote this"])).unwrap();
        assert!(only_text["crash"].get("trace").is_none());
    }

    #[test]
    fn only_trindle_programs_are_reported() {
        for program in ["firefox", "ring", "trindle_ring", "trindlefoo", "-trindle"] {
            assert!(report_from(&note("crash", "SIGSEGV", program, None, &[])).is_none(), "{program}");
        }
        for program in ["trindle", "trindle-ring", "trindle-compose", "trindle-flow"] {
            assert!(report_from(&note("crash", "SIGSEGV", program, None, &[])).is_some(), "{program}");
        }
    }

    #[test]
    fn notes_are_kept_until_the_reports_from_them_are_saved() {
        let (dir, at) = home("saved");
        leave_notes(&at, &[note("crash", "panic", "trindle-ring", None, &["ring::open"])]);
        fs::create_dir_all(&at.waiting).unwrap();
        assert_eq!(run_with(&at, 10_000, &["trindle"], 0).len(), 1, "one try, not reached");
        assert!(at.taking.exists(), "notes deleted before the reports were saved");

        fs::remove_dir_all(&at.waiting).unwrap();
        assert_eq!(run_with(&at, 10_000 + EVERY, &["trindle"], 201).len(), 1);
        assert!(!at.taking.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_one_run_at_a_time() {
        let (dir, at) = home("lock");
        let first = lock(&at).expect("free lock not taken");
        assert!(lock(&at).is_none(), "two runs at once");
        drop(first);
        assert!(lock(&at).is_some());
        let _ = fs::remove_dir_all(dir);
    }

    fn fake_server(answers: &'static [(u16, &'static str)]) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/api/reports/trindle", listener.local_addr().unwrap());
        let thread = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (code, said) in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut got = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap();
                    got.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    let Some((head, rest)) = text.split_once("\r\n\r\n") else { continue };
                    let want = head.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    if rest.len() >= want || n == 0 {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&got).to_string());
                let reply = format!("HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{said}", said.len());
                stream.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });
        (address, thread)
    }

    #[test]
    fn curl_asks_first_then_posts_json_with_no_key() {
        let (address, thread) = fake_server(&[(200, "{\"accepting\":true}"), (200, "{\"accepting\":false}"), (503, ""), (201, "{\"ok\":true}"), (403, "{\"ok\":false}")]);
        assert!(accepting(&address));
        assert!(!accepting(&address));
        assert!(!accepting(&address));
        let report = report_from(&note("crash", "panic", "trindle-ring", Some("src/ring/view.rs"), &["ring::open"])).unwrap().to_string();
        assert_eq!(post(&address, &report), 201);
        assert_eq!(post(&address, &report), 403);
        let seen = thread.join().unwrap();
        assert!(seen[0].starts_with("GET /api/reports/trindle/status "), "{}", seen[0]);
        assert!(seen[3].starts_with("POST /api/reports/trindle "), "{}", seen[3]);
        let lower = seen[3].to_lowercase();
        assert!(lower.contains("content-type: application/json"));
        assert!(!lower.contains("authorization"));
        assert!(seen[3].ends_with(&report));
        assert_eq!(post("http://127.0.0.1:9/none", "{}"), 0, "no connection");
    }

    #[test]
    fn the_sent_list_keeps_the_last_fifty() {
        let (dir, at) = home("keep");
        for i in 0..60 {
            keep_sent(&at.sent, vec![json!({ "when": i })]);
        }
        let log = fs::read_to_string(&at.sent).unwrap();
        assert_eq!(log.lines().count(), KEEP_SENT);
        assert!(log.lines().next().unwrap().contains("\"when\":10"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_whole_computer_part_is_allowed() {
        let c = computer();
        for k in c.as_object().unwrap().keys() {
            assert!(["os", "arch", "memory_gb", "session", "gpu", "screens"].contains(&k.as_str()));
        }
    }
}
