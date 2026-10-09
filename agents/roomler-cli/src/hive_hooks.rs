// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P1j-3 — `roomler hive adopt | unadopt | hook`: a person adopts the
//! Claude Code sessions they run in a terminal (decision 11, spec §3h).
//!
//! `adopt` puts three hooks in the person's OWN user-level Claude Code
//! settings, and nobody else's. `unadopt` takes out exactly those three.
//! Claude Code then runs `roomler hive hook` as the person at each hook. The
//! hook reads the transcript's new lines, as the person, and hands them to
//! the daemon's adopt socket (`tunnel_core::localapi::hive_adopt`).
//!
//! | rule | why |
//! |---|---|
//! | every other setting stays byte for byte; inside `hooks`, every other event and group too | the file is the person's, and other tools keep their hooks there (FR-8's session restore on the dev box that built this) |
//! | a file that is not a JSON object, or `hooks` that is not one, is refused, never rewritten | a parse that guessed would destroy the person's settings |
//! | a symbolic link is followed and its target rewritten in place; the previous bytes are kept as `<file>.bak-roomler` | dotfile managers link this file; replacing the link would break their setup |
//! | the hook prints nothing and always exits 0 | Claude Code may show a hook's stderr and add its stdout to the model's context; a broken hook must never break a session |
//! | `SessionStart` and `Stop` run in the background (`async`); `SessionEnd` runs in the foreground | nobody at the terminal waits; `SessionEnd` gets a short shared budget and only flushes |

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde_json::value::{RawValue, to_raw_value};
use serde_json::{Value, json};
use tunnel_core::localapi::hive_adopt::{self as proto, HookEvent, Reply, Request};

/// An object whose values are kept exactly as written.
type Obj = IndexMap<String, Box<RawValue>>;

/// The events `adopt` hooks, and whether Claude Code runs each in the
/// background.
pub const EVENTS: [(&str, bool); 3] = [
    ("SessionStart", true),
    ("Stop", true),
    ("SessionEnd", false),
];

/// The longest a hook entry may run before Claude Code abandons it. A
/// version that runs it in the foreground (one that knows no `async`) waits
/// at most this long at a turn's end.
const HOOK_TIMEOUT_SECS: u64 = 30;

/// What a hook entry runs: this very binary, with `hive hook`, as ONE shell
/// command line.
///
/// ⚠️ Shell form, never exec form (`command` + `args`). `roomlerd` with no
/// arguments RUNS THE DAEMON, so a Claude Code that dropped an `args` list it
/// did not know would start a daemon as the person at every hook. A command
/// line is run whole by every version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    pub command: String,
}

impl HookCommand {
    /// The running binary. Embedded in the daemon (`roomlerd cli`, which the
    /// installed `roomler` shim re-execs) the entry runs `roomlerd cli hive
    /// hook` directly, without the shim's hop.
    pub fn this_binary(embedded: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("locating this binary")?;
        let path = exe.to_str().context("this binary's path is not UTF-8")?;
        Ok(Self::for_binary(path, embedded))
    }

    /// The entry for the binary at `path`.
    pub fn for_binary(path: &str, embedded: bool) -> Self {
        let sub = if embedded {
            "cli hive hook"
        } else {
            "hive hook"
        };
        Self {
            command: format!("{} {sub}", shell_quote(path)),
        }
    }

    fn handler(&self, background: bool) -> Value {
        let mut h = json!({
            "type": "command",
            "command": self.command,
            "timeout": HOOK_TIMEOUT_SECS,
        });
        if background {
            h["async"] = json!(true);
        }
        h
    }
}

/// `s` as one word for a POSIX shell: single quotes, a quote inside closed,
/// escaped and reopened.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The first word of a shell command line, unquoted: what a hook entry runs.
fn first_word(command: &str) -> (String, &str) {
    let mut word = String::new();
    let mut chars = command.trim_start().char_indices().peekable();
    let rest_from = |i: usize| &command.trim_start()[i..];
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' => {
                for (_, q) in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '\\' => {
                if let Some((_, e)) = chars.next() {
                    word.push(e);
                }
            }
            c if c.is_whitespace() => return (word, rest_from(i)),
            c => word.push(c),
        }
    }
    (word, "")
}

/// Whether a hook handler is one `adopt` installed: a command line that runs
/// a Roomler binary with `hive hook` (or `cli hive hook`) and nothing else.
pub fn ours(handler: &Value) -> bool {
    let command = handler.get("command").and_then(Value::as_str).unwrap_or("");
    let (program, rest) = first_word(command);
    let stem = Path::new(&program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let rest: Vec<&str> = rest.split_whitespace().collect();
    matches!(stem, "roomler" | "roomlerd" | "roomler-shim")
        && (rest == ["hive", "hook"] || rest == ["cli", "hive", "hook"])
}

fn group_has_ours(group: &RawValue) -> bool {
    serde_json::from_str::<Value>(group.get())
        .ok()
        .and_then(|g| g.get("hooks").and_then(Value::as_array).cloned())
        .is_some_and(|hs| hs.iter().any(ours))
}

fn object(text: &str, what: &str) -> Result<Obj> {
    serde_json::from_str::<Obj>(text)
        .with_context(|| format!("{what} is not a JSON object — the file was not touched"))
}

fn pretty(doc: &Obj) -> Result<String> {
    let mut s = serde_json::to_string_pretty(doc)?;
    s.push('\n');
    Ok(s)
}

/// The settings with `adopt`'s three hooks in, and whether anything changed.
/// `None`: there is no settings file yet.
pub fn adopt_doc(text: Option<&str>, hook: &HookCommand) -> Result<(String, bool)> {
    let mut doc = match text.map(str::trim) {
        None | Some("") => Obj::new(),
        Some(t) => object(t, "the settings file")?,
    };
    let mut hooks = match doc.get("hooks") {
        None => Obj::new(),
        Some(raw) => object(raw.get(), "its `hooks`")?,
    };
    let mut changed = false;
    for (event, background) in EVENTS {
        let mut groups: Vec<Box<RawValue>> = match hooks.get(event) {
            None => Vec::new(),
            Some(raw) => serde_json::from_str(raw.get()).with_context(|| {
                format!("`hooks.{event}` is not a list — the file was not touched")
            })?,
        };
        if groups.iter().any(|g| group_has_ours(g)) {
            continue;
        }
        groups.push(to_raw_value(
            &json!({ "hooks": [hook.handler(background)] }),
        )?);
        hooks.insert(event.to_string(), to_raw_value(&groups)?);
        changed = true;
    }
    if !changed {
        return Ok((text.unwrap_or_default().to_string(), false));
    }
    doc.insert("hooks".to_string(), to_raw_value(&hooks)?);
    Ok((pretty(&doc)?, true))
}

/// The settings with `adopt`'s hooks taken out, every other key and hook
/// left as it was, and whether anything changed. A group or an event left
/// empty by the removal goes with it, and so does a `hooks` left empty.
pub fn unadopt_doc(text: &str) -> Result<(String, bool)> {
    if text.trim().is_empty() {
        return Ok((text.to_string(), false));
    }
    let mut doc = object(text, "the settings file")?;
    let Some(raw_hooks) = doc.get("hooks") else {
        return Ok((text.to_string(), false));
    };
    let mut hooks = object(raw_hooks.get(), "its `hooks`")?;
    let mut changed = false;
    for (event, _) in EVENTS {
        let Some(raw) = hooks.get(event) else {
            continue;
        };
        let groups: Vec<Box<RawValue>> = serde_json::from_str(raw.get())
            .with_context(|| format!("`hooks.{event}` is not a list — the file was not touched"))?;
        let mut kept: Vec<Box<RawValue>> = Vec::new();
        let mut touched = false;
        for g in groups {
            let mut v: Value = serde_json::from_str(g.get())?;
            let removed = match v.get_mut("hooks").and_then(Value::as_array_mut) {
                Some(hs) => {
                    let before = hs.len();
                    hs.retain(|h| !ours(h));
                    (before != hs.len()).then_some(hs.is_empty())
                }
                None => None,
            };
            match removed {
                // Untouched: kept exactly as written.
                None => kept.push(g),
                // Ours alone: the group goes.
                Some(true) => touched = true,
                // Ours among others: the others stay.
                Some(false) => {
                    touched = true;
                    kept.push(to_raw_value(&v)?);
                }
            }
        }
        if touched {
            changed = true;
            if kept.is_empty() {
                hooks.shift_remove(event);
            } else {
                hooks.insert(event.to_string(), to_raw_value(&kept)?);
            }
        }
    }
    if !changed {
        return Ok((text.to_string(), false));
    }
    if hooks.is_empty() {
        doc.shift_remove("hooks");
    } else {
        doc.insert("hooks".to_string(), to_raw_value(&hooks)?);
    }
    Ok((pretty(&doc)?, true))
}

/// The person's user-level Claude Code settings: `--settings`, else
/// `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
pub fn settings_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .context("no home directory for Claude Code's settings")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Rewrite `path` with `text`: a link followed to its target, the previous
/// bytes kept beside it, the file's mode kept, written whole (a temporary,
/// then a rename).
fn write_settings(path: &Path, previous: Option<&[u8]>, text: &str) -> Result<PathBuf> {
    let target = if path.exists() {
        std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        path.to_path_buf()
    };
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("settings.json")
        .to_string();
    if let Some(bytes) = previous {
        let bak = target.with_file_name(format!("{name}.bak-roomler"));
        std::fs::write(&bak, bytes).with_context(|| format!("writing {}", bak.display()))?;
    }
    let tmp = target.with_file_name(format!(".{name}.roomler-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))?;
    Ok(target)
}

/// `roomler hive adopt`.
pub fn adopt(explicit: Option<PathBuf>, embedded: bool) -> Result<()> {
    if cfg!(windows) {
        bail!("adopting terminal sessions is not available on Windows yet");
    }
    let path = settings_path(explicit)?;
    let previous = match std::fs::read(&path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = previous
        .as_deref()
        .map(std::str::from_utf8)
        .transpose()
        .context("the settings file is not UTF-8 — it was not touched")?;
    let hook = HookCommand::this_binary(embedded)?;
    let (next, changed) = adopt_doc(text, &hook)?;
    if changed {
        let written = write_settings(&path, previous.as_deref(), &next)?;
        println!(
            "Adopted: the Claude Code sessions you run in a terminal here are mirrored into Roomler, where only you see them."
        );
        println!("  hooks added to {}", written.display());
    } else {
        println!("Already adopted ({}).", path.display());
    }
    match proto::socket_path() {
        Some(s) if s.exists() => println!("  this device adopts terminal sessions"),
        _ => println!(
            "  this device does not adopt yet: its owner turns on `hive_adopt` and maps your \
             account in `hive_accounts`; until then the hooks do nothing"
        ),
    }
    Ok(())
}

/// `roomler hive unadopt`.
pub fn unadopt(explicit: Option<PathBuf>) -> Result<()> {
    let path = settings_path(explicit)?;
    let previous = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Nothing to undo: {} does not exist.", path.display());
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = std::str::from_utf8(&previous)
        .context("the settings file is not UTF-8 — it was not touched")?;
    let (next, changed) = unadopt_doc(text)?;
    if changed {
        let written = write_settings(&path, Some(&previous), &next)?;
        println!(
            "Unadopted: the hooks are out of {}; nothing else changed.",
            written.display()
        );
    } else {
        println!("Nothing to undo: no Roomler hooks in {}.", path.display());
    }
    Ok(())
}

/// What Claude Code hands a hook on stdin, the fields this one reads.
#[derive(Debug, PartialEq, Eq)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub event: HookEvent,
    pub reason: Option<String>,
}

/// Parse the hook's stdin. `None`: not Claude Code's hook input.
pub fn hook_input(raw: &[u8]) -> Option<HookInput> {
    let v: Value = serde_json::from_slice(raw).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let event = match s("hook_event_name").as_deref() {
        Some("SessionStart") => HookEvent::SessionStart,
        Some("Stop") => HookEvent::Stop,
        Some("SessionEnd") => HookEvent::SessionEnd,
        _ => HookEvent::Other,
    };
    Some(HookInput {
        session_id: s("session_id")?,
        transcript_path: s("transcript_path")?,
        cwd: s("cwd").unwrap_or_default(),
        event,
        reason: s("reason"),
    })
}

/// One `Lines` request's worth of the transcript, from `offset`: whole lines
/// up to [`proto::MAX_CHUNK`], and the bytes of one line after them that is
/// too long to send ([`proto::MAX_LINE`]) or not UTF-8. `None`: nothing whole
/// to send yet (a line still being written).
pub fn next_chunk(
    file: &mut (impl Read + Seek),
    offset: u64,
) -> std::io::Result<Option<(String, u64)>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; proto::MAX_CHUNK.max(proto::MAX_LINE) + 1];
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let full = filled == buf.len();
    let buf = &buf[..filled];
    let mut data = String::new();
    let mut at = 0;
    while at < buf.len() {
        let Some(nl) = buf[at..].iter().position(|b| *b == b'\n') else {
            break;
        };
        let line = &buf[at..at + nl + 1];
        // A line alone may be as long as MAX_LINE; with others, the chunk.
        let fits = data.is_empty() || data.len() + line.len() <= proto::MAX_CHUNK;
        match std::str::from_utf8(line) {
            Ok(l) if line.len() <= proto::MAX_LINE && fits => {
                data.push_str(l);
                at += line.len();
            }
            // Room for it in the next request.
            Ok(_) if line.len() <= proto::MAX_LINE => break,
            // Not text: skipped, after what came before.
            _ => return Ok(Some((data, line.len() as u64))),
        }
    }
    if !data.is_empty() {
        return Ok(Some((data, 0)));
    }
    if !full {
        // A line still being written: it goes once it ends.
        return Ok(None);
    }
    // A line longer than MAX_LINE: skipped whole once its newline is there.
    // Read on to find it, counting, never keeping.
    let mut len = filled as u64;
    let mut more = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut more)?;
        if n == 0 {
            return Ok(None);
        }
        if let Some(nl) = more[..n].iter().position(|b| *b == b'\n') {
            return Ok(Some((String::new(), len + nl as u64 + 1)));
        }
        len += n as u64;
    }
}

/// `roomler hive hook`: what Claude Code runs. Never fails and prints
/// nothing: an adopt that cannot happen now happens at the next hook, or not
/// at all, and the person's session goes on either way.
pub async fn hook() {
    let mut raw = Vec::new();
    let _ = std::io::stdin().take(1024 * 1024).read_to_end(&mut raw);
    let Some(input) = hook_input(&raw) else {
        return;
    };
    let _ = mirror(&input).await;
}

/// The hook's connection to the adopt socket: one request, one reply.
#[cfg(unix)]
struct Conn {
    write: tokio::net::unix::OwnedWriteHalf,
    replies: tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
}

#[cfg(unix)]
impl Conn {
    async fn ask(&mut self, req: Request) -> Result<Reply> {
        use tokio::io::AsyncWriteExt;
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.write.write_all(&line).await?;
        let reply =
            tokio::time::timeout(std::time::Duration::from_secs(20), self.replies.next_line())
                .await??
                .context("the daemon closed the socket")?;
        Ok(serde_json::from_str(&reply)?)
    }
}

#[cfg(unix)]
async fn mirror(input: &HookInput) -> Result<()> {
    let Some(socket) = proto::socket_path() else {
        return Ok(());
    };
    mirror_at(&socket, input).await
}

/// [`hook`]'s work against the adopt socket at `socket`: what the daemon's
/// own tests drive, so the two halves of the protocol are tested together.
#[cfg(unix)]
pub async fn mirror_at(socket: &Path, input: &HookInput) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::UnixStream::connect(socket),
    )
    .await??;
    let (read, write) = stream.into_split();
    let mut conn = Conn {
        write,
        replies: BufReader::new(read).lines(),
    };
    let hello = conn
        .ask(Request::Hello {
            harness_session: input.session_id.clone(),
            cwd: input.cwd.clone(),
            event: input.event,
        })
        .await?;
    if !hello.ok {
        return Ok(());
    }
    let mut offset = hello.offset.unwrap_or(0);
    // Read as the person: this process runs as them.
    let mut file = std::fs::File::open(&input.transcript_path)?;
    let mut sent = 0usize;
    let mut resynced = false;
    while sent < proto::MAX_PER_RUN {
        let Some((data, skipped)) = next_chunk(&mut file, offset)? else {
            break;
        };
        let n = data.len();
        let reply = conn
            .ask(Request::Lines {
                from: offset,
                data,
                skipped,
            })
            .await?;
        match (reply.ok, reply.offset) {
            (true, Some(next)) => {
                offset = next;
                sent += n + skipped as usize;
            }
            // Lost its place once: start again where the daemon's copy ends.
            (false, Some(next)) if !resynced => {
                offset = next;
                resynced = true;
            }
            _ => return Ok(()),
        }
    }
    match input.event {
        HookEvent::Stop => {
            conn.ask(Request::TurnEnded).await?;
        }
        HookEvent::SessionEnd => {
            conn.ask(Request::End {
                reason: input.reason.clone(),
            })
            .await?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn mirror(_input: &HookInput) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook() -> HookCommand {
        HookCommand::for_binary("/usr/bin/roomler", false)
    }

    /// A settings file as this dev box keeps it: someone else's hooks (FR-8's
    /// session restore) on two of the three events, other settings around.
    const THEIRS: &str = r#"{
  "model": "opus",
  "permissions": {
    "allow": ["Bash(git status)"],
    "deny": []
  },
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "pwsh -NoProfile -File C:/x/hook.ps1" }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          { "type": "command", "command": "pwsh -NoProfile -File C:/x/hook.ps1" }
        ]
      }
    ]
  },
  "statusLine": { "type": "command", "command": "~/.claude/statusline.sh" }
}
"#;

    #[test]
    fn adopt_adds_three_hooks_and_keeps_everything_else() {
        let (out, changed) = adopt_doc(Some(THEIRS), &hook()).unwrap();
        assert!(changed);
        let v: Value = serde_json::from_str(&out).unwrap();
        let before: Value = serde_json::from_str(THEIRS).unwrap();
        for key in ["model", "permissions", "statusLine"] {
            assert_eq!(v[key], before[key], "{key} untouched");
        }
        // Theirs first, ours after, on every event.
        assert_eq!(
            v["hooks"]["SessionStart"][0],
            before["hooks"]["SessionStart"][0]
        );
        assert_eq!(
            v["hooks"]["SessionEnd"][0],
            before["hooks"]["SessionEnd"][0]
        );
        for (event, background) in EVENTS {
            let groups = v["hooks"][event].as_array().unwrap();
            let mine: Vec<&Value> = groups
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap())
                .filter(|h| ours(h))
                .collect();
            assert_eq!(mine.len(), 1, "{event}: {out}");
            assert_eq!(mine[0]["command"], json!("'/usr/bin/roomler' hive hook"));
            assert_eq!(mine[0].get("async").is_some(), background, "{event}");
        }
        // Top-level keys stay in their order.
        let doc = serde_json::from_str::<Obj>(&out).unwrap();
        let keys: Vec<&str> = doc.keys().map(String::as_str).collect();
        assert_eq!(keys, ["model", "permissions", "hooks", "statusLine"]);
        // A second adopt changes nothing.
        let (again, changed) = adopt_doc(Some(&out), &hook()).unwrap();
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn unadopt_takes_out_exactly_what_adopt_put_in() {
        let (adopted, _) = adopt_doc(Some(THEIRS), &hook()).unwrap();
        let (out, changed) = unadopt_doc(&adopted).unwrap();
        assert!(changed);
        let after: Value = serde_json::from_str(&out).unwrap();
        let before: Value = serde_json::from_str(THEIRS).unwrap();
        assert_eq!(after, before, "the file means what it meant before adopt");
        // Nothing ours left to take: a second unadopt changes nothing.
        let (again, changed) = unadopt_doc(&out).unwrap();
        assert!(!changed);
        assert_eq!(again, out);
    }

    #[test]
    fn a_file_with_no_hooks_round_trips_to_no_hooks() {
        let plain = "{\n  \"model\": \"sonnet\"\n}\n";
        let (adopted, changed) = adopt_doc(Some(plain), &hook()).unwrap();
        assert!(changed);
        let (back, _) = unadopt_doc(&adopted).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&back).unwrap(),
            json!({"model": "sonnet"})
        );
        // No file at all: a file with the hooks alone.
        let (fresh, changed) = adopt_doc(None, &hook()).unwrap();
        assert!(changed);
        assert_eq!(
            serde_json::from_str::<Value>(&fresh).unwrap()["hooks"]
                .as_object()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn a_file_that_is_not_a_settings_object_is_refused() {
        assert!(adopt_doc(Some("[1, 2]"), &hook()).is_err());
        assert!(adopt_doc(Some("{ not json"), &hook()).is_err());
        assert!(adopt_doc(Some(r#"{"hooks": []}"#), &hook()).is_err());
        assert!(adopt_doc(Some(r#"{"hooks": {"Stop": {}}}"#), &hook()).is_err());
        assert!(unadopt_doc("{ not json").is_err());
    }

    #[test]
    fn only_a_roomler_hive_hook_is_ours() {
        let c = |s: &str| json!({ "type": "command", "command": s });
        assert!(ours(&c("'/usr/bin/roomler' hive hook")));
        assert!(ours(&c("'/usr/bin/roomlerd' cli hive hook")));
        assert!(ours(&c("/usr/local/bin/roomler hive hook")));
        assert!(
            ours(&c("'/opt/my apps/roomler' hive hook")),
            "a path with a space"
        );
        assert!(!ours(&c("'/usr/bin/roomler' peers")));
        assert!(!ours(&c("'/usr/bin/other' hive hook")));
        assert!(
            !ours(&c("'/usr/bin/roomler' hive hook; rm -rf ~")),
            "anything more is someone else's"
        );
        assert!(
            !ours(&json!({"command": "/usr/bin/roomler", "args": ["hive", "hook"]})),
            "exec form was never written"
        );
    }

    /// The entry runs the very binary, quoted, whatever its path holds: a
    /// `roomlerd` reached with no arguments would run the daemon.
    #[test]
    fn the_entry_is_one_command_line_for_this_binary() {
        let h = HookCommand::for_binary("/opt/it's here/roomlerd", true);
        assert_eq!(h.command, r"'/opt/it'\''s here/roomlerd' cli hive hook");
        let (program, rest) = first_word(&h.command);
        assert_eq!(program, "/opt/it's here/roomlerd");
        assert_eq!(
            rest.split_whitespace().collect::<Vec<_>>(),
            ["cli", "hive", "hook"]
        );
        assert!(ours(&h.handler(true)));
        let v = h.handler(false);
        assert!(v.get("args").is_none(), "never exec form: {v}");
        assert_eq!(v["timeout"], json!(HOOK_TIMEOUT_SECS));
        assert!(
            v.get("async").is_none(),
            "SessionEnd runs in the foreground"
        );
        assert_eq!(h.handler(true)["async"], json!(true));
    }

    #[test]
    fn the_hook_input_is_claude_codes() {
        let i = hook_input(
            br#"{"session_id":"u","transcript_path":"/t.jsonl","cwd":"/w","hook_event_name":"SessionEnd","reason":"logout"}"#,
        )
        .unwrap();
        assert_eq!(
            (i.event, i.reason.as_deref(), i.cwd.as_str()),
            (HookEvent::SessionEnd, Some("logout"), "/w")
        );
        assert!(hook_input(b"not json").is_none());
        assert!(
            hook_input(br#"{"hook_event_name":"Stop"}"#).is_none(),
            "no session"
        );
    }

    #[test]
    fn chunks_are_whole_lines_and_a_huge_line_is_skipped() {
        use std::io::Cursor;
        let mut f = Cursor::new(b"{\"a\":1}\n{\"b\":2}\n{\"partial\"".to_vec());
        assert_eq!(
            next_chunk(&mut f, 0).unwrap(),
            Some(("{\"a\":1}\n{\"b\":2}\n".to_string(), 0))
        );
        assert_eq!(
            next_chunk(&mut f, 16).unwrap(),
            None,
            "a line still being written"
        );

        let mut big = vec![b'x'; proto::MAX_LINE + 10];
        big.push(b'\n');
        big.extend_from_slice(b"{\"c\":3}\n");
        let mut f = Cursor::new(big);
        assert_eq!(
            next_chunk(&mut f, 0).unwrap(),
            Some((String::new(), (proto::MAX_LINE + 11) as u64)),
            "a line too long to send is skipped whole"
        );
        assert_eq!(
            next_chunk(&mut f, (proto::MAX_LINE + 11) as u64).unwrap(),
            Some(("{\"c\":3}\n".to_string(), 0))
        );

        let mut f = Cursor::new(b"{\"ok\":1}\n\xff\xfe\n".to_vec());
        assert_eq!(
            next_chunk(&mut f, 0).unwrap(),
            Some(("{\"ok\":1}\n".to_string(), 3)),
            "a line that is not text is skipped after what came before"
        );
    }
}
