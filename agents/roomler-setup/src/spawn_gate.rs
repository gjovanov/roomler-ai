// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-129 C — the gate on how the wizard's Windows path names a program.
//!
//! A program named without a path is looked for in the folder of the running
//! EXE before System32 (Rust's `Command`), and in the current directory by the
//! shell's `runas` verb. The wizard's folder is usually the person's Downloads
//! and the same image goes on to run elevated, so a bare name there is a file
//! someone else may have left, run with the administrator's token and no
//! prompt. `restrict_dll_search` closes that door for DLLs; this closes it for
//! EXEs: every spawn on the path names its program in full
//! (`roomlerd::appdirs::system32_tool`).
//!
//! The test parses the source of everything the wizard runs on Windows, the
//! wizard's own files, its mechanics crate and the daemon's modules it calls,
//! with the `cfg` of a Windows, non-test build applied, and fails on any
//! program named by a bare literal or by an expression it has not been told
//! about. A new spawn is therefore a decision someone has to write down here.

use std::path::{Path, PathBuf};

use quote::ToTokens;
use syn::visit::{self, Visit};

/// Folders scanned whole, from the repository root: every file a new module
/// could be added to.
const WHOLE_FOLDERS: &[&str] = &[
    "agents/roomler-setup/src",
    "crates/roomler-setup-core/src",
    "agents/roomlerd/src/install_detect",
];

/// The daemon's files the wizard calls into, from the repository root: what
/// `use roomlerd::…` and `roomler_cli::…` reach in `agents/roomler-setup/src`,
/// followed through the crate. `updater.rs` and `service.rs` are the only two
/// with a spawn on this path; the others are here so that one added to them
/// fails too.
const DAEMON_FILES: &[&str] = &[
    "agents/roomlerd/src/code_signature.rs",
    "agents/roomlerd/src/updater.rs",
    "agents/roomlerd/src/service.rs",
    "agents/roomlerd/src/jwt_introspect.rs",
    "agents/roomlerd/src/win_identity.rs",
    "agents/roomlerd/src/org_join.rs",
    "agents/roomlerd/src/win_service/environment.rs",
    "agents/roomlerd/src/win_service/system_context_attempt.rs",
    "agents/roomler-cli/src/config.rs",
    "crates/agent-core/src/appdirs.rs",
    "crates/agent-core/src/config.rs",
    "crates/agent-core/src/config_surface.rs",
    "crates/agent-core/src/enrollment.rs",
    "crates/agent-core/src/machine.rs",
    "crates/agent-core/src/machine_tree.rs",
    "crates/agent-core/src/pairing.rs",
];

/// Helpers that return a full path: an argument built on a call to one (with
/// `.join(…)`, `.display()`, …) names its program in full.
const FULL_PATH_HELPERS: &[&str] = &["system32_tool", "windows_directory"];

/// Spawns that compile into the Windows build and are not bare names at run
/// time there, or are never reached there: `(file, argument as written, why)`.
/// A stale row fails the test as well, so the list cannot rot.
const KNOWN: &[(&str, &str, &str)] = &[
    (
        "agents/roomler-setup/src/orchestrator_agent.rs",
        "exe . display () . to_string ()",
        "`roomlerd anchor create`: the install folder's own roomlerd.exe, a full \
         path, under the %ProgramFiles% that `launch::start_clean` set",
    ),
    (
        "agents/roomlerd/src/updater.rs",
        "launch",
        "the daemon's own post-install watcher, from its current_exe or a staged \
         copy of it: a full path, and the wizard does not call that function",
    ),
    (
        "agents/roomler-setup/src/unix_daemon.rs",
        "binary",
        "the Linux and macOS pipeline, which `cmd_install` never runs on Windows",
    ),
    (
        "agents/roomler-setup/src/unix_daemon.rs",
        "cmd . program",
        "the same",
    ),
    (
        "agents/roomler-setup/src/unix_daemon.rs",
        "program",
        "the same",
    ),
];

#[derive(Debug, PartialEq, Eq)]
struct Spawn {
    file: String,
    argument: String,
    bare: bool,
}

/// Does `cfg(meta)` hold on a Windows, non-test build? Anything this does not
/// know holds: a feature may be on, and looking at too much is the safe side.
fn cfg_holds(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => !(path.is_ident("unix") || path.is_ident("test")),
        syn::Meta::NameValue(pair) => {
            let value = pair.value.to_token_stream().to_string().replace('"', "");
            let names_the_os =
                pair.path.is_ident("target_os") || pair.path.is_ident("target_family");
            !names_the_os || value == "windows"
        }
        syn::Meta::List(list) => {
            let nested = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|p| p.into_iter().collect::<Vec<_>>())
                .unwrap_or_default();
            if list.path.is_ident("not") {
                !nested.iter().all(cfg_holds)
            } else if list.path.is_ident("any") {
                nested.iter().any(cfg_holds)
            } else if list.path.is_ident("all") {
                nested.iter().all(cfg_holds)
            } else {
                true
            }
        }
    }
}

/// Is the thing these attributes sit on compiled into a Windows, non-test
/// build?
fn compiled(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().all(|attr| {
        !attr.path().is_ident("cfg")
            || attr
                .parse_args::<syn::Meta>()
                .map_or(true, |meta| cfg_holds(&meta))
    })
}

struct Scan<'a> {
    file: &'a str,
    found: Vec<Spawn>,
}

impl Scan<'_> {
    fn record(&mut self, argument: &syn::Expr) {
        let mut inner = argument;
        while let syn::Expr::Reference(r) = inner {
            inner = &r.expr;
        }
        if let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = inner
        {
            let name = s.value();
            // `/bin/sh` and the like cannot come from the wizard's folder.
            if !name.contains(['/', '\\']) {
                self.found.push(Spawn {
                    file: self.file.to_string(),
                    argument: format!("{name:?}"),
                    bare: true,
                });
            }
            return;
        }
        let mut root = inner;
        while let syn::Expr::MethodCall(m) = root {
            root = &m.receiver;
        }
        let named_in_full = matches!(root, syn::Expr::Call(call) if matches!(
            &*call.func,
            syn::Expr::Path(p) if p.path.segments.last().is_some_and(|s| {
                FULL_PATH_HELPERS.contains(&s.ident.to_string().as_str())
            })
        ));
        if !named_in_full {
            self.found.push(Spawn {
                file: self.file.to_string(),
                argument: inner.to_token_stream().to_string(),
                bare: false,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if compiled(&i.attrs) {
            visit::visit_item_fn(self, i);
        }
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        if compiled(&i.attrs) {
            visit::visit_item_mod(self, i);
        }
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if compiled(&i.attrs) {
            visit::visit_item_impl(self, i);
        }
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if compiled(&i.attrs) {
            visit::visit_impl_item_fn(self, i);
        }
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        if compiled(&i.attrs) {
            visit::visit_local(self, i);
        }
    }
    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        if compiled(&i.attrs) {
            visit::visit_arm(self, i);
        }
    }
    fn visit_expr_block(&mut self, i: &'ast syn::ExprBlock) {
        if compiled(&i.attrs) {
            visit::visit_expr_block(self, i);
        }
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if compiled(&call.attrs)
            && let syn::Expr::Path(path) = &*call.func
        {
            let segments: Vec<String> = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            let is_spawn = segments.ends_with(&["Command".to_string(), "new".to_string()])
                || segments.last().is_some_and(|s| s == "spawn_elevated");
            if is_spawn && let Some(argument) = call.args.first() {
                self.record(argument);
            }
        }
        visit::visit_expr_call(self, call);
    }
}

/// The names `source` calls (the last segment of a path call, or a method), in
/// order: so a test can ask whether code does something, and a comment or a
/// string saying so does not count.
pub(crate) fn called_names(source: &str) -> Vec<String> {
    struct Names(Vec<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let syn::Expr::Path(path) = &*call.func
                && let Some(last) = path.path.segments.last()
            {
                self.0.push(last.ident.to_string());
            }
            visit::visit_expr_call(self, call);
        }
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            // The receiver first: reading order, for a chain.
            visit::visit_expr_method_call(self, call);
            self.0.push(call.method.to_string());
        }
    }
    let parsed = syn::parse_file(source).expect("the source parses");
    let mut names = Names(Vec::new());
    names.visit_file(&parsed);
    names.0
}

/// Every spawn in `source` that a Windows, non-test build compiles.
fn spawns_in(file: &str, source: &str) -> Vec<Spawn> {
    let parsed = syn::parse_file(source).unwrap_or_else(|e| panic!("{file}: {e}"));
    let mut scan = Scan {
        file,
        found: Vec::new(),
    };
    scan.visit_file(&parsed);
    scan.found
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("agents/roomler-setup sits two below the root")
        .to_path_buf()
}

/// The files the gate reads, each as `(path from the root, its text)`.
fn sources() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files: Vec<String> = DAEMON_FILES.iter().map(|f| f.to_string()).collect();
    for folder in WHOLE_FOLDERS {
        for entry in std::fs::read_dir(root.join(folder)).expect(folder) {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".rs") {
                files.push(format!("{folder}/{name}"));
            }
        }
    }
    files
        .into_iter()
        .map(|file| {
            let text = std::fs::read_to_string(root.join(&file)).expect(&file);
            (file, text)
        })
        .collect()
}

/// The gate itself.
#[test]
fn no_program_on_the_windows_path_is_named_by_a_bare_name() {
    let mut unknown = Vec::new();
    let mut used = vec![false; KNOWN.len()];
    for (file, text) in sources() {
        for spawn in spawns_in(&file, &text) {
            let row = KNOWN
                .iter()
                .position(|(f, a, _)| *f == spawn.file && *a == spawn.argument);
            match row {
                Some(row) => used[row] = true,
                None => unknown.push(spawn),
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "programs on the wizard's Windows path that are not named in full \
         (use roomlerd::appdirs::system32_tool, or add a row to KNOWN with the \
         reason): {unknown:#?}"
    );
    let stale: Vec<_> = KNOWN
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|(row, _)| row)
        .collect();
    assert!(
        stale.is_empty(),
        "rows of KNOWN no spawn matches: {stale:#?}"
    );
}

/// A gate that cannot see the thing it guards passes on anything, so it is
/// shown each shape it must catch and each it must leave alone.
#[test]
fn the_gate_sees_what_it_is_for() {
    let bare = |source: &str| {
        spawns_in("t.rs", source)
            .into_iter()
            .map(|s| (s.argument, s.bare))
            .collect::<Vec<_>>()
    };
    let yes = |name: &str| vec![(format!("{name:?}"), true)];

    assert_eq!(
        bare(r#"fn f() { std::process::Command::new("icacls"); }"#),
        yes("icacls"),
        "the plain form"
    );
    assert_eq!(
        bare(r#"fn f() { tokio::process::Command::new("icacls.exe"); }"#),
        yes("icacls.exe"),
        "tokio's, and a name with its extension"
    );
    assert_eq!(
        bare(r#"fn f() { without_stdio(Command::new("msiexec").args(a)).spawn(); }"#),
        yes("msiexec"),
        "inside another call"
    );
    assert_eq!(
        bare(r#"fn f() { updater::spawn_elevated("msiexec.exe", a); }"#),
        yes("msiexec.exe"),
        "the elevated spawn, which the shell resolves from the current directory"
    );
    assert_eq!(
        bare(r#"#[cfg(windows)] fn f() { Command::new("schtasks"); }"#),
        yes("schtasks"),
        "a Windows-only function"
    );
    assert_eq!(
        bare(r#"mod m { impl T { fn f() { Command::new("schtasks"); } } }"#),
        yes("schtasks"),
        "in a method, in a module"
    );
    assert_eq!(
        bare(r#"fn f() { #[cfg(target_os = "windows")] { Command::new("sc"); } }"#),
        yes("sc"),
        "a Windows-only block"
    );
    assert_eq!(
        bare(r#"#[cfg(not(unix))] fn f() { Command::new("sc"); }"#),
        yes("sc"),
        "`not`"
    );
    assert_eq!(
        bare(r#"fn f() { Command::new(name); }"#),
        vec![("name".to_string(), false)],
        "an expression the gate has not been told about"
    );

    for clean in [
        r#"fn f() { Command::new(system32_tool("icacls.exe")); }"#,
        r#"fn f() { Command::new(&roomlerd::appdirs::system32_tool("icacls.exe")); }"#,
        r#"fn f() { spawn_elevated(&system32_tool("msiexec.exe").display().to_string(), a); }"#,
        r#"fn f() { Command::new(windows_directory().join("explorer.exe")); }"#,
        r#"fn f() { Command::new("/bin/sh"); }"#,
        r#"#[cfg(unix)] fn f() { Command::new("systemctl"); }"#,
        r#"#[cfg(target_os = "macos")] fn f() { Command::new("launchctl"); }"#,
        r#"#[cfg(test)] mod tests { fn f() { Command::new("cmd"); } }"#,
        r#"fn f() { #[cfg(target_os = "linux")] { Command::new("tar"); } }"#,
        r#"#[cfg(all(unix, target_os = "linux"))] fn f() { Command::new("tar"); }"#,
        r#"#[cfg(any(target_os = "linux", target_os = "macos"))] fn f() { Command::new("tar"); }"#,
        r#"// Command::new("msiexec") in a comment"
           fn f() {}"#,
    ] {
        assert_eq!(bare(clean), Vec::new(), "{clean}");
    }
}
