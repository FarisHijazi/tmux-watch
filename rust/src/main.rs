//! tmux-watch — tile matching tmux sessions into a single hub session.
//!
//! Rust port of `src/tmux_watch/cli.py` (0.3.0), behaviour-for-behaviour:
//! same hub names, same `@tw-args` / `@tw-src` formats (old and new binaries
//! can reconcile each other's hubs), same output and exit codes.
//!
//! Usage:
//!     tmux-watch [-d N | --max-depth N] [-n] [-x REGEX]... [host:]PATH...
//!     tmux-watch purge-hubs

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

const PROG: &str = "tmux-watch";
const HUB_ARGS_KEY: &str = "@tw-args"; // session option: JSON of (depth, specs, exclude)
const PANE_SRC_KEY: &str = "@tw-src"; // per-pane option: "<host>\t<session>"

/// A fatal error: printed as-is to stderr, exit status 1 (Python's sys.exit(str)).
struct Fatal(String);
type R<T> = Result<T, Fatal>;

fn fatal<T>(msg: impl Into<String>) -> R<T> {
    Err(Fatal(msg.into()))
}

// ---------- subprocess helpers ----------

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn run(cmd: &str, args: &[&str]) -> Out {
    match Command::new(cmd).args(args).output() {
        Ok(o) => Out {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Out { ok: false, stdout: String::new(), stderr: e.to_string() },
    }
}

fn tmux(args: &[&str]) -> Out {
    run("tmux", args)
}

fn session_exists(name: &str) -> bool {
    tmux(&["has-session", "-t", name]).ok
}

// ---------- python-compatible JSON (hub identity must hash identically) ----------

fn py_str(s: &str) -> String {
    // json.dumps default: ensure_ascii=True
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn py_list(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(", "))
}

fn py_depth(depth: Option<i64>) -> String {
    depth.map_or("null".into(), |d| d.to_string())
}

// ---------- specs ----------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Spec {
    host: String, // "" for local; ssh target otherwise
    path: String, // absolute path on `host`
}

/// rsync rule: split on first ':' only if it appears before any '/'.
fn parse_spec(arg: &str) -> R<(String, String)> {
    let colon = arg.find(':');
    let slash = arg.find('/');
    if let Some(c) = colon {
        if slash.is_none_or(|s| c < s) {
            let (host, path) = (&arg[..c], &arg[c + 1..]);
            if host == "local" {
                return fatal(format!(
                    "{PROG}: 'local:' prefix is not a valid host — drop it and use the bare path: {path}"
                ));
            }
            return Ok((host.into(), path.into()));
        }
    }
    Ok((String::new(), arg.into()))
}

/// shlex.quote
fn sh_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Like sh_quote but keeps a leading `~` / `~/` so the remote shell expands it.
fn quote_remote_path(path: &str) -> String {
    if path == "~" {
        return "~".into();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("~/{}", sh_quote(rest));
    }
    sh_quote(path)
}

fn expanduser(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if path == "~" {
        return home;
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => path.into(),
    }
}

fn resolve_spec(host: &str, path: &str) -> R<Spec> {
    if host.is_empty() {
        let p = expanduser(path);
        if !Path::new(&p).is_dir() {
            return fatal(format!("{PROG}: not a directory: {path}"));
        }
        let abs = std::fs::canonicalize(&p).map_err(|e| Fatal(format!("{PROG}: {e}")))?;
        return Ok(Spec { host: String::new(), path: abs.to_string_lossy().into() });
    }
    let r = run(
        "ssh",
        &["-o", "ConnectTimeout=5", "-o", "BatchMode=yes", host,
          &format!("cd {} && pwd -P", quote_remote_path(path))],
    );
    if !r.ok {
        let err = r.stderr.trim();
        return fatal(format!(
            "{PROG}: {host}: cannot resolve {path}: {}",
            if err.is_empty() { "ssh failed" } else { err }
        ));
    }
    Ok(Spec { host: host.into(), path: r.stdout.trim().into() })
}

fn display_target(host: &str, name_or_path: &str) -> String {
    if host.is_empty() { name_or_path.into() } else { format!("{host}:{name_or_path}") }
}

// ---------- session listing ----------

/// `tmux list-panes -F '#S\t#{pane_current_path}'` → dedupe by session (first pane wins).
fn parse_panes_output(text: &str) -> Vec<(String, String)> {
    let mut seen: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some((s, p)) = line.split_once('\t') {
            if !seen.iter().any(|(n, _)| n == s) {
                seen.push((s.into(), p.into()));
            }
        }
    }
    seen
}

fn list_local_sessions() -> Vec<(String, String)> {
    let r = tmux(&["list-panes", "-a", "-F", "#S\t#{pane_current_path}"]);
    if r.ok { parse_panes_output(&r.stdout) } else { Vec::new() }
}

fn list_remote_sessions(host: &str) -> Option<Vec<(String, String)>> {
    let r = run(
        "ssh",
        &["-o", "ConnectTimeout=5", "-o", "BatchMode=yes", host,
          "tmux list-panes -a -F '#S\t#{pane_current_path}' 2>/dev/null"],
    );
    r.ok.then(|| parse_panes_output(&r.stdout))
}

fn within(path: &str, base: &str, depth: Option<i64>) -> bool {
    if path == base {
        return true;
    }
    let Some(rel) = path.strip_prefix(&format!("{base}/")) else { return false };
    match depth {
        None => true,
        Some(d) => rel.matches('/').count() as i64 + 1 <= d,
    }
}

fn is_hub_session(name: &str) -> bool {
    name == "hub" || name.starts_with("hub/")
}

fn compile_exclude(patterns: &[String]) -> R<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| {
            Regex::new(p).map_err(|e| Fatal(format!("{PROG}: bad --exclude regex {}: {e}", py_repr(p))))
        })
        .collect()
}

fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

type Pair = (String, String); // (host, session)

fn list_pairs(specs: &[Spec], depth: Option<i64>, exclude: &[String]) -> R<(Vec<Pair>, HashSet<String>)> {
    let rx = compile_exclude(exclude)?;
    let mut hosts: Vec<&str> = Vec::new(); // first-seen order, like a Python dict
    for s in specs {
        if !hosts.contains(&s.host.as_str()) {
            hosts.push(&s.host);
        }
    }
    let mut pairs = Vec::new();
    let mut seen = HashSet::new();
    let mut unreachable = HashSet::new();
    for host in hosts {
        let host_specs: Vec<&Spec> = specs.iter().filter(|s| s.host == host).collect();
        let sessions = if host.is_empty() { Some(list_local_sessions()) } else { list_remote_sessions(host) };
        let Some(sessions) = sessions else {
            unreachable.insert(host.to_string());
            continue;
        };
        for (name, path) in sessions {
            if is_hub_session(&name) || rx.iter().any(|r| r.is_match(&name)) {
                continue;
            }
            if host_specs.iter().any(|sp| within(&path, &sp.path, depth)) {
                let key = (host.to_string(), name);
                if seen.insert(key.clone()) {
                    pairs.push(key);
                }
            }
        }
    }
    Ok((pairs, unreachable))
}

// ---------- hub naming ----------

fn hub_name(depth: Option<i64>, specs: &[Spec], exclude: &[String]) -> String {
    let mut sorted_specs = specs.to_vec();
    sorted_specs.sort();
    let mut excl = exclude.to_vec();
    excl.sort();
    // json.dumps({...}, sort_keys=True)
    let key = format!(
        "{{\"depth\": {}, \"exclude\": {}, \"specs\": {}}}",
        py_depth(depth),
        py_list(excl.iter().map(|e| py_str(e))),
        py_list(sorted_specs.iter().map(|s| py_list([py_str(&s.host), py_str(&s.path)]))),
    );
    let digest = sha1_smol::Sha1::from(key.as_bytes()).digest().to_string();
    let first = &sorted_specs[0];
    let base = Path::new(&first.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "root".into());
    let label = if first.host.is_empty() { base } else { format!("{}-{base}", first.host) };
    let label = Regex::new(r"[^A-Za-z0-9]+").unwrap().replace_all(&label, "-");
    let label: String = label.trim_matches('-').to_lowercase().chars().take(30).collect();
    format!("hub/{label}__{}", &digest[..6])
}

// ---------- pane operations ----------

fn attach_cmd(host: &str, session: &str) -> String {
    if host.is_empty() {
        format!("TMUX= tmux attach -t {}", sh_quote(session))
    } else {
        format!("ssh -t {} tmux attach -t {}", sh_quote(host), sh_quote(session))
    }
}

fn split_window(hub: &str, command: &str) -> String {
    tmux(&["split-window", "-t", hub, "-P", "-F", "#{pane_id}", command]).stdout.trim().into()
}

fn set_pane_src(pid: &str, host: &str, session: &str) {
    tmux(&["set-option", "-p", "-t", pid, PANE_SRC_KEY, &format!("{host}\t{session}")]);
}

fn install_pane(pid: &str, host: &str, sess: &str) {
    set_pane_src(pid, host, sess);
    tmux(&["select-pane", "-t", pid, "-T", &display_target(host, sess)]);
}

/// {(host, sess): pane_id} for panes with @tw-src; legacy panes are migrated from their title.
fn list_panes_with_identity(hub: &str) -> BTreeMap<Pair, String> {
    let r = tmux(&["list-panes", "-t", hub, "-F", "#{pane_id}\t#{pane_title}\t#{@tw-src}"]);
    let mut out = BTreeMap::new();
    if !r.ok {
        return out;
    }
    for line in r.stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let (pid, title, src) = (parts[0], parts[1], parts.get(2).copied().unwrap_or(""));
        let (host, sess) = match src.split_once('\t') {
            Some((h, s)) if !src.is_empty() => (h.to_string(), s.to_string()),
            _ => {
                let (h, s) = match title.split_once(':') {
                    Some((h, s)) => (h.to_string(), s.to_string()),
                    None => (String::new(), title.to_string()),
                };
                if s.is_empty() {
                    continue;
                }
                set_pane_src(pid, &h, &s);
                (h, s)
            }
        };
        out.insert((host, sess), pid.to_string());
    }
    out
}

// ---------- hub state ----------

fn store_hub_args(hub: &str, depth: Option<i64>, specs: &[Spec], exclude: &[String]) {
    let payload = format!(
        "{{\"depth\": {}, \"specs\": {}, \"exclude\": {}}}",
        py_depth(depth),
        py_list(specs.iter().map(|s| format!("{{\"host\": {}, \"path\": {}}}", py_str(&s.host), py_str(&s.path)))),
        py_list(exclude.iter().map(|e| py_str(e))),
    );
    tmux(&["set-option", "-t", hub, HUB_ARGS_KEY, &payload]);
}

fn read_hub_args(hub: &str) -> R<(Option<i64>, Vec<Spec>, Vec<String>)> {
    let r = tmux(&["show-options", "-v", "-t", hub, HUB_ARGS_KEY]);
    if !r.ok || r.stdout.trim().is_empty() {
        return fatal(format!("{PROG}: no {HUB_ARGS_KEY} on session {hub}"));
    }
    let bad = || Fatal(format!("{PROG}: corrupt {HUB_ARGS_KEY} on session {hub}"));
    let data: Value = serde_json::from_str(r.stdout.trim()).map_err(|_| bad())?;
    let depth = data["depth"].as_i64();
    let specs = data["specs"]
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|s| Spec {
            host: s["host"].as_str().unwrap_or("").into(),
            path: s["path"].as_str().unwrap_or("").into(),
        })
        .collect();
    // hubs created before --exclude existed have no "exclude" key
    let exclude = data["exclude"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok((depth, specs, exclude))
}

// ---------- locking ----------

fn with_lock<T>(hub: &str, f: impl FnOnce() -> T) -> T {
    let path = format!("/tmp/tmux-watch-{}.lock", hub.replace('/', "_"));
    let file = File::create(&path).ok();
    if let Some(f) = &file {
        let _ = f.lock(); // flock(LOCK_EX), shared with the Python poller
    }
    let out = f();
    drop(file); // closing the fd releases the lock
    out
}

// ---------- layout: one block per host ----------
// See cli.py: `select-layout tiled` interleaves hosts; each host gets its own
// column-block (spec order), near-square grid inside.

/// Sizes for weights.len() cells sharing `total`, minus 1-cell separators.
fn split_sizes(total: i64, weights: &[i64]) -> Vec<i64> {
    let room = total - (weights.len() as i64 - 1);
    let sum: i64 = weights.iter().sum();
    let mut sizes: Vec<i64> = weights.iter().map(|w| (room * w).div_euclid(sum)).collect();
    let used: i64 = sizes.iter().sum();
    *sizes.last_mut().unwrap() += room - used;
    sizes
}

/// Panes per row for a near-square grid of n panes.
fn grid(n: usize) -> Vec<usize> {
    let cols = ((n as f64).sqrt() + 0.49).round_ties_even().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    let (base, extra) = (n / rows, n % rows);
    (0..rows).map(|i| base + usize::from(i < extra)).collect()
}

enum Node {
    Leaf(String),
    Split { horiz: bool, kids: Vec<(i64, Node)> },
}

fn render(node: &Node, w: i64, h: i64, x: i64, y: i64) -> String {
    match node {
        Node::Leaf(pid) => format!("{w}x{h},{x},{y},{}", pid.trim_start_matches('%')),
        Node::Split { kids, .. } if kids.len() == 1 => render(&kids[0].1, w, h, x, y),
        Node::Split { horiz, kids } => {
            let weights: Vec<i64> = kids.iter().map(|(k, _)| *k).collect();
            let sizes = split_sizes(if *horiz { w } else { h }, &weights);
            let mut off = if *horiz { x } else { y };
            let mut parts = Vec::new();
            for (size, (_, kid)) in sizes.iter().zip(kids) {
                parts.push(if *horiz { render(kid, *size, h, off, y) } else { render(kid, w, *size, x, off) });
                off += size + 1;
            }
            let (o, c) = if *horiz { ('{', '}') } else { ('[', ']') };
            format!("{w}x{h},{x},{y}{o}{}{c}", parts.join(","))
        }
    }
}

fn checksum(layout: &str) -> String {
    let mut csum: u32 = 0;
    for b in layout.chars() {
        csum = ((csum >> 1) + ((csum & 1) << 15) + b as u32) & 0xFFFF;
    }
    format!("{csum:04x}")
}

fn host_layout(groups: &[Vec<String>], w: i64, h: i64) -> String {
    let block = |pids: &[String]| {
        let mut rows = Vec::new();
        let mut i = 0;
        for k in grid(pids.len()) {
            let row = pids[i..i + k].iter().map(|p| (1, Node::Leaf(p.clone()))).collect();
            rows.push((1, Node::Split { horiz: true, kids: row }));
            i += k;
        }
        Node::Split { horiz: false, kids: rows }
    };
    // Block width weighted by its widest row, so tiles come out roughly equal.
    let body = Node::Split {
        horiz: true,
        kids: groups.iter().map(|g| (*grid(g.len()).iter().max().unwrap() as i64, block(g))).collect(),
    };
    let body = render(&body, w, h, 0, 0);
    format!("{},{body}", checksum(&body))
}

fn apply_layout(hub: &str) -> R<()> {
    let panes = list_panes_with_identity(hub);
    let (_, specs, _) = read_hub_args(hub)?;
    let mut order: Vec<String> = Vec::new();
    let pane_hosts: std::collections::BTreeSet<&String> = panes.keys().map(|(h, _)| h).collect();
    for h in specs.iter().map(|s| &s.host).chain(pane_hosts) {
        if !order.contains(h) {
            order.push(h.clone());
        }
    }
    let groups: Vec<Vec<String>> = order
        .iter()
        .map(|host| panes.iter().filter(|((h, _), _)| h == host).map(|(_, pid)| pid.clone()).collect())
        .filter(|g: &Vec<String>| !g.is_empty())
        .collect();
    // tmux fills a custom layout's cells in PANE INDEX order and ignores the ids
    // written in the string, so the panes must first be swapped into group order.
    let want: Vec<&String> = groups.iter().flatten().collect();
    let mut cur: Vec<String> =
        tmux(&["list-panes", "-t", hub, "-F", "#{pane_id}"]).stdout.split_whitespace().map(String::from).collect();
    for (i, pid) in want.iter().enumerate() {
        if i < cur.len() && &cur[i] != *pid {
            if let Some(j) = cur.iter().position(|c| c == *pid) {
                tmux(&["swap-pane", "-d", "-s", pid, "-t", &cur[i]]);
                cur.swap(i, j);
            }
        }
    }
    let size = tmux(&["display-message", "-p", "-t", hub, "#{window_width} #{window_height}"]);
    let wh: Vec<i64> = size.stdout.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    if groups.len() > 1 && wh.len() == 2 && tmux(&["select-layout", "-t", hub, &host_layout(&groups, wh[0], wh[1])]).ok {
        return Ok(());
    }
    tmux(&["select-layout", "-t", hub, "tiled"]);
    Ok(())
}

// ---------- reconcile ----------

fn reconcile(hub: &str) -> R<(usize, usize)> {
    let (depth, specs, exclude) = read_hub_args(hub)?;
    let (pairs, unreachable) = list_pairs(&specs, depth, &exclude)?;
    let src: HashSet<&Pair> = pairs.iter().collect();
    let panes = list_panes_with_identity(hub);

    let mut added = 0;
    for (host, sess) in pairs.iter().filter(|p| !panes.contains_key(*p)) {
        let pid = split_window(hub, &attach_cmd(host, sess));
        if !pid.is_empty() {
            install_pane(&pid, host, sess);
            added += 1;
        }
    }
    let mut removed = 0;
    for (key, pid) in &panes {
        if src.contains(key) || unreachable.contains(&key.0) {
            continue;
        }
        tmux(&["kill-pane", "-t", pid]);
        removed += 1;
    }
    if added > 0 || removed > 0 {
        apply_layout(hub)?;
    }
    Ok((added, removed))
}

// ---------- hub creation ----------

fn spawn_poller(hub: &str) {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| PROG.into());
    tmux(&["run-shell", "-b", &format!("exec {} _poll {}", sh_quote(&exe), sh_quote(hub))]);
}

fn create_hub(hub: &str, depth: Option<i64>, specs: &[Spec], exclude: &[String], pairs: &[Pair]) -> R<()> {
    let (first_host, first_sess) = &pairs[0];
    let r = tmux(&["new-session", "-d", "-s", hub, &attach_cmd(first_host, first_sess)]);
    if !r.ok {
        return fatal(format!("{PROG}: failed to create hub: {}", r.stderr.trim()));
    }
    let panes = tmux(&["list-panes", "-t", hub, "-F", "#{pane_id}"]);
    let first_pid = panes.stdout.lines().next().unwrap_or("").trim().to_string();
    install_pane(&first_pid, first_host, first_sess);
    for (host, sess) in &pairs[1..] {
        let pid = split_window(hub, &attach_cmd(host, sess));
        if !pid.is_empty() {
            install_pane(&pid, host, sess);
        }
    }
    tmux(&["set-option", "-t", hub, "pane-border-status", "top"]);
    tmux(&["set-option", "-t", hub, "pane-border-format", " #{pane_title} "]);
    store_hub_args(hub, depth, specs, exclude);
    apply_layout(hub)?;
    spawn_poller(hub);
    Ok(())
}

// ---------- legacy hub detection ----------

fn warn_legacy_hubs() {
    let r = tmux(&["list-sessions", "-F", "#S"]);
    if !r.ok {
        return;
    }
    let stale: Vec<&str> = r
        .stdout
        .lines()
        .filter(|s| is_hub_session(s))
        .filter(|s| {
            let a = tmux(&["show-options", "-v", "-t", s, HUB_ARGS_KEY]);
            !a.ok || a.stdout.trim().is_empty()
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    eprintln!(
        "{PROG}: warning: {} legacy hub(s) without poller — run `{PROG} purge-hubs` to clean them up:",
        stale.len()
    );
    for h in stale {
        eprintln!("  {h}");
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn warn_unreachable(unreachable: &HashSet<String>) {
    if unreachable.is_empty() {
        return;
    }
    let mut hosts: Vec<&String> = unreachable.iter().collect();
    hosts.sort();
    let hosts: Vec<&str> = hosts.iter().map(|s| s.as_str()).collect();
    eprintln!("{PROG}: warning: unreachable hosts skipped: {}", hosts.join(", "));
}

// ---------- entry points ----------

fn cmd_main(depth: Option<i64>, mut paths: Vec<String>, exclude: Vec<String>, dry_run: bool) -> R<u8> {
    if paths.is_empty() {
        paths.push(".".into());
    }
    compile_exclude(&exclude)?; // fail fast on a bad regex
    warn_legacy_hubs();
    let specs = paths.iter().map(|p| parse_spec(p).and_then(|(h, p)| resolve_spec(&h, &p))).collect::<R<Vec<_>>>()?;
    let hub = hub_name(depth, &specs, &exclude);

    if dry_run {
        let (pairs, unreachable) = list_pairs(&specs, depth, &exclude)?;
        warn_unreachable(&unreachable);
        println!("hub: {hub}");
        println!("depth: {}", depth.map_or("unlimited".into(), |d| d.to_string()));
        println!("exclude: {}", if exclude.is_empty() { "none".into() } else { exclude.join(", ") });
        println!("specs:");
        for s in &specs {
            println!("  {}", display_target(&s.host, &s.path));
        }
        println!("would attach ({} session{}):", pairs.len(), plural(pairs.len()));
        for (host, sess) in &pairs {
            println!("  {}", display_target(host, sess));
        }
        return Ok(0);
    }

    if session_exists(&hub) {
        // Same args ⇒ same hub. Kill the stale one and rebuild fresh.
        tmux(&["kill-session", "-t", &hub]);
    }
    let (pairs, unreachable) = list_pairs(&specs, depth, &exclude)?;
    if pairs.is_empty() {
        return fatal(format!("{PROG}: no matching tmux sessions"));
    }
    warn_unreachable(&unreachable);
    create_hub(&hub, depth, &specs, &exclude, &pairs)?;
    // The hub dies with its last client (see cli.py): set in the attach itself,
    // because on a detached session destroy-unattached kills it immediately.
    let err = Command::new("tmux")
        .args(["attach", "-t", &hub, ";", "set-option", "-t", &hub, "destroy-unattached", "on"])
        .exec();
    fatal(format!("{PROG}: exec tmux: {err}"))
}

fn cmd_reconcile(hub: &str) -> R<u8> {
    let (added, removed) = with_lock(hub, || reconcile(hub))?;
    println!("{PROG}: reconciled {hub}: +{added} -{removed}");
    Ok(0)
}

fn cmd_purge_hubs() -> R<u8> {
    let r = tmux(&["list-sessions", "-F", "#S"]);
    if !r.ok {
        println!("{PROG}: no tmux server running");
        return Ok(0);
    }
    let hubs: Vec<&str> = r.stdout.lines().filter(|s| is_hub_session(s)).collect();
    if hubs.is_empty() {
        println!("{PROG}: no hub sessions to purge");
        return Ok(0);
    }
    for h in &hubs {
        tmux(&["kill-session", "-t", h]);
        println!("killed {h}");
    }
    println!(
        "{PROG}: purged {} hub session{} (pollers self-exit within one tick)",
        hubs.len(),
        plural(hubs.len())
    );
    Ok(0)
}

fn cmd_poll(hub: &str) -> R<u8> {
    let interval: f64 = std::env::var("TW_POLL_INTERVAL").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(3.0);
    // First tick fires quickly so panes create_hub couldn't split get filled in.
    let mut delay = interval.min(0.5);
    while session_exists(hub) {
        std::thread::sleep(Duration::from_secs_f64(delay.max(0.0)));
        if with_lock(hub, || reconcile(hub)).is_err() {
            return Ok(0); // hub vanished / args unreadable: same as Python's SystemExit
        }
        delay = interval;
    }
    Ok(0)
}

// ---------- argument parsing (argparse-compatible surface) ----------

const USAGE: &str = "usage: tmux-watch [-h] [-d N] [-n] [-x REGEX] [[host:]PATH ...]";

fn help() -> String {
    format!(
        "{USAGE}\n\nTile matching tmux sessions into a single hub session.\n\n\
positional arguments:\n  [host:]PATH           directories to watch (default: .); host: prefix for\n                        remote (rsync style)\n\n\
options:\n  -h, --help            show this help message and exit\n\
  -d, --max-depth N     max directory depth below each PATH\n\
  -n, --dry-run         list sessions that would be attached, then exit\n\
  -x, --exclude REGEX   skip sessions whose name matches REGEX (repeatable;\n                        hub/* is always skipped)\n\n\
subcommands:\n  purge-hubs    kill all hub/* sessions and exit\n"
    )
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("{USAGE}\n{PROG}: error: {msg}");
    ExitCode::from(2)
}

enum Parsed {
    Run { depth: Option<i64>, dry_run: bool, exclude: Vec<String>, paths: Vec<String> },
    Help,
}

fn parse_args(argv: &[String]) -> Result<Parsed, String> {
    let longs = ["--max-depth", "--dry-run", "--exclude", "--help"];
    let (mut depth, mut dry_run, mut exclude, mut paths) = (None, false, Vec::new(), Vec::new());
    let mut i = 0;
    let mut only_positional = false;
    let mut pending: Option<String> = None;
    loop {
        let arg = match pending.take() {
            Some(a) => a,
            None if i < argv.len() => {
                i += 1;
                argv[i - 1].clone()
            }
            None => break,
        };
        let arg = &arg;
        if only_positional || arg == "-" || !arg.starts_with('-') || arg.parse::<f64>().is_ok() {
            paths.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }
        // normalise to (option, inline value)
        let (opt, inline): (String, Option<String>) = if let Some(long) = arg.strip_prefix("--") {
            let (name, val) = match long.split_once('=') {
                Some((n, v)) => (format!("--{n}"), Some(v.to_string())),
                None => (arg.clone(), None),
            };
            let hits: Vec<&&str> = longs.iter().filter(|l| l.starts_with(name.as_str())).collect();
            match hits.as_slice() {
                [one] => (one.to_string(), val),
                [] => return Err(format!("unrecognized arguments: {arg}")),
                many => {
                    let names: Vec<&str> = many.iter().map(|s| **s).collect();
                    return Err(format!("ambiguous option: {name} could match {}", names.join(", ")));
                }
            }
        } else {
            let short = &arg[..2];
            let rest = &arg[2..];
            match short {
                "-d" | "-x" => (short.into(), (!rest.is_empty()).then(|| rest.to_string())),
                "-n" | "-h" if rest.is_empty() => (short.into(), None),
                "-n" | "-h" => {
                    // bundled flags, e.g. -nd2 == -n -d2
                    pending = Some(format!("-{rest}"));
                    (short.into(), None)
                }
                _ => return Err(format!("unrecognized arguments: {arg}")),
            }
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            match argv.get(i) {
                Some(v) if !v.starts_with('-') || v.parse::<f64>().is_ok() => {
                    i += 1;
                    Ok(v.clone())
                }
                _ => Err(format!("argument {name}: expected one argument")),
            }
        };
        match opt.as_str() {
            "-d" | "--max-depth" => {
                let v = value("-d/--max-depth")?;
                depth = Some(v.trim().parse::<i64>().map_err(|_| format!("argument -d/--max-depth: invalid int value: '{v}'"))?);
            }
            "-x" | "--exclude" => exclude.push(value("-x/--exclude")?),
            "-n" | "--dry-run" => {
                if inline.is_some() {
                    return Err(format!("argument -n/--dry-run: ignored explicit argument '{}'", inline.unwrap()));
                }
                dry_run = true
            }
            "-h" | "--help" => return Ok(Parsed::Help),
            _ => unreachable!(),
        }
    }
    Ok(Parsed::Run { depth, dry_run, exclude, paths })
}

fn finish(r: R<u8>) -> ExitCode {
    match r {
        Ok(code) => ExitCode::from(code),
        Err(Fatal(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = argv.first().map(String::as_str) {
        if first == "_poll" || first == "_reconcile" {
            let Some(hub) = argv.get(1) else {
                return finish(fatal(format!("{PROG}: {first} requires HUB argument")));
            };
            return finish(if first == "_poll" { cmd_poll(hub) } else { cmd_reconcile(hub) });
        }
        if first == "purge-hubs" {
            if argv.len() > 1 {
                return finish(fatal(format!("{PROG}: purge-hubs takes no arguments")));
            }
            return finish(cmd_purge_hubs());
        }
    }
    match parse_args(&argv) {
        Ok(Parsed::Help) => {
            print!("{}", help());
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run { depth, dry_run, exclude, paths }) => finish(cmd_main(depth, paths, exclude, dry_run)),
        Err(msg) => usage_error(&msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing_follows_rsync() {
        assert_eq!(parse_spec("box:~/src").ok().unwrap(), ("box".into(), "~/src".into()));
        assert_eq!(parse_spec("/a:b").ok().unwrap(), ("".into(), "/a:b".into()));
        assert!(parse_spec("local:/x").is_err());
    }

    #[test]
    fn depth_limits() {
        assert!(within("/a", "/a", Some(0)));
        assert!(within("/a/b", "/a", Some(1)));
        assert!(!within("/a/b/c", "/a", Some(1)));
        assert!(!within("/ab", "/a", None));
    }

    #[test]
    fn layout_checksum_matches_tmux() {
        // value from python cli.host_layout([['%1','%2'],['%3']], 100, 40)
        let v = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            host_layout(&[v(&["%1", "%2"]), v(&["%3"])], 100, 40),
            "3b14,100x40,0,0{66x40,0,0{32x40,0,0,1,33x40,33,0,2},33x40,67,0,3}"
        );
        assert_eq!(
            host_layout(&[v(&["%1", "%2", "%3", "%4", "%5"]), v(&["%6", "%7", "%8"])], 211, 50),
            "f05d,211x50,0,0{126x50,0,0[126x24,0,0{41x24,0,0,1,41x24,42,0,2,42x24,84,0,3},126x25,0,25{62x25,0,25,4,63x25,63,25,5}],84x50,127,0[84x24,127,0{41x24,127,0,6,42x24,169,0,7},84x25,127,25,8]}"
        );
    }
}
