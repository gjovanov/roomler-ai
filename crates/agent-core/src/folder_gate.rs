// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3702 — the gate on where the daemon reads a folder variable.
//!
//! `%ProgramData%`, `%ProgramFiles%`, `%APPDATA%`, `%LOCALAPPDATA%` and
//! `%USERPROFILE%` name folders, and a process's environment can name other
//! ones. A step on the service account takes the machine's folders from the
//! system (`appdirs::machine_program_data`, `appdirs::machine_program_files`)
//! and its own profile from the profile list (`appdirs::system_profile`), so
//! [`crate::appdirs`] is the one module of the daemon's two crates that reads
//! any of these variables.
//!
//! The test parses every source file of `crates/agent-core` and
//! `agents/roomlerd`, with the `cfg` of a Windows, non-test build applied,
//! and fails on any function outside `appdirs` that reads a variable and names
//! one of these, unless it is listed in [`KNOWN`] with the reason. A stale row
//! fails too, so the list cannot rot.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};

/// The variables, with the names that mean the same folders. Windows takes a
/// variable's name in any case.
const FOLDER_VARIABLES: &[&str] = &[
    "ProgramData",
    "ALLUSERSPROFILE",
    "ProgramFiles",
    "ProgramW6432",
    "ProgramFiles(x86)",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
];

/// Folders scanned whole, from the repository root.
const FOLDERS: &[&str] = &["crates/agent-core/src", "agents/roomlerd/src"];

/// The module that reads them.
const READER: &str = "crates/agent-core/src/appdirs.rs";

/// Reads outside [`READER`]: `(file, function, why)`.
const KNOWN: &[(&str, &str, &str)] = &[
    (
        "crates/agent-core/src/files_dir.rs",
        "from_env",
        "the received-files denylist's Windows roots, in the process that checks \
         a path: not a folder a step writes, and on the service account the \
         values are the system's own once `appdirs::pin_system_folders` ran",
    ),
    (
        "crates/agent-core/src/companion_autostart.rs",
        "program_files_dirs",
        "how the desktop companion was installed, read by the companion as the \
         person: not a folder a step writes",
    ),
];

/// Whether a `cfg` holds on a Windows, non-test build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holds {
    Yes,
    No,
    Maybe,
}

fn holds(meta: &syn::Meta) -> Holds {
    match meta {
        syn::Meta::Path(path) if path.is_ident("test") || path.is_ident("unix") => Holds::No,
        syn::Meta::Path(path) if path.is_ident("windows") => Holds::Yes,
        syn::Meta::Path(_) => Holds::Maybe,
        syn::Meta::NameValue(pair)
            if pair.path.is_ident("target_os") || pair.path.is_ident("target_family") =>
        {
            match &pair.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(value),
                    ..
                }) if value.value() == "windows" => Holds::Yes,
                _ => Holds::No,
            }
        }
        syn::Meta::NameValue(_) => Holds::Maybe,
        syn::Meta::List(list) => {
            let nested: Vec<Holds> = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|p| p.iter().map(holds).collect())
                .unwrap_or_default();
            if list.path.is_ident("not") {
                match nested.first() {
                    Some(Holds::Yes) => Holds::No,
                    Some(Holds::No) => Holds::Yes,
                    _ => Holds::Maybe,
                }
            } else if list.path.is_ident("any") {
                if nested.contains(&Holds::Yes) {
                    Holds::Yes
                } else if nested.iter().all(|h| *h == Holds::No) {
                    Holds::No
                } else {
                    Holds::Maybe
                }
            } else if list.path.is_ident("all") {
                if nested.contains(&Holds::No) {
                    Holds::No
                } else if nested.iter().all(|h| *h == Holds::Yes) {
                    Holds::Yes
                } else {
                    Holds::Maybe
                }
            } else {
                Holds::Maybe
            }
        }
    }
}

/// Is the thing these attributes sit on compiled into a Windows, non-test
/// build? What the gate cannot tell (a feature) is: looking at too much is the
/// safe side.
fn compiled(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().all(|attr| {
        !attr.path().is_ident("cfg")
            || attr
                .parse_args::<syn::Meta>()
                .map_or(true, |meta| holds(&meta) != Holds::No)
    })
}

/// A read of the environment: `std::env::var`, `env::var_os`, or one of them
/// imported by name, called or passed as a function.
fn reads_a_variable(path: &syn::Path) -> bool {
    let names: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    match names.as_slice() {
        [.., last] if last != "var" && last != "var_os" => false,
        [_] => true,
        [.., env, _] => env == "env",
        [] => false,
    }
}

/// What one function's body does: whether it reads a variable, and which of
/// [`FOLDER_VARIABLES`] it names.
#[derive(Default)]
struct Body {
    reads: bool,
    names: Vec<String>,
}

impl<'ast> Visit<'ast> for Body {
    fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
        if reads_a_variable(&i.path) {
            self.reads = true;
        }
        visit::visit_expr_path(self, i);
    }
    fn visit_lit_str(&mut self, i: &'ast syn::LitStr) {
        let value = i.value();
        if FOLDER_VARIABLES
            .iter()
            .any(|name| value.eq_ignore_ascii_case(name))
        {
            self.names.push(value);
        }
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        if compiled(&i.attrs) {
            visit::visit_local(self, i);
        }
    }
    fn visit_expr_block(&mut self, i: &'ast syn::ExprBlock) {
        if compiled(&i.attrs) {
            visit::visit_expr_block(self, i);
        }
    }
    fn visit_stmt_macro(&mut self, i: &'ast syn::StmtMacro) {
        if compiled(&i.attrs) {
            visit::visit_stmt_macro(self, i);
        }
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if compiled(&i.attrs) {
            visit::visit_expr_call(self, i);
        }
    }
    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        if compiled(&i.attrs) {
            visit::visit_expr_method_call(self, i);
        }
    }
    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        if compiled(&i.attrs) {
            visit::visit_arm(self, i);
        }
    }
    fn visit_item(&mut self, i: &'ast syn::Item) {
        // An item inside a body (a nested fn, a const) is part of it.
        let attrs = match i {
            syn::Item::Fn(f) => &f.attrs,
            syn::Item::Const(c) => &c.attrs,
            syn::Item::Static(s) => &s.attrs,
            syn::Item::Mod(m) => &m.attrs,
            syn::Item::Impl(m) => &m.attrs,
            _ => return visit::visit_item(self, i),
        };
        if compiled(attrs) {
            visit::visit_item(self, i);
        }
    }
}

/// One function, constant or static that reads a variable and names one of
/// [`FOLDER_VARIABLES`]: `(function, names)`.
#[derive(Debug, PartialEq, Eq)]
struct Read {
    function: String,
    names: Vec<String>,
}

/// Every compiled function of a file, by name.
struct Functions {
    found: Vec<Read>,
}

impl Functions {
    fn body(&mut self, name: String, visit_body: impl FnOnce(&mut Body)) {
        let mut body = Body::default();
        visit_body(&mut body);
        if body.reads && !body.names.is_empty() {
            body.names.sort();
            body.names.dedup();
            self.found.push(Read {
                function: name,
                names: body.names,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Functions {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if compiled(&i.attrs) {
            self.body(i.sig.ident.to_string(), |b| b.visit_block(&i.block));
        }
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if compiled(&i.attrs) {
            self.body(i.sig.ident.to_string(), |b| b.visit_block(&i.block));
        }
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if compiled(&i.attrs)
            && let Some(block) = &i.default
        {
            self.body(i.sig.ident.to_string(), |b| b.visit_block(block));
        }
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        if compiled(&i.attrs) {
            self.body(i.ident.to_string(), |b| b.visit_expr(&i.expr));
        }
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        if compiled(&i.attrs) {
            self.body(i.ident.to_string(), |b| b.visit_expr(&i.expr));
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
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        if compiled(&i.attrs) {
            visit::visit_item_trait(self, i);
        }
    }
}

fn reads_in(file: &str, source: &str) -> Vec<Read> {
    let parsed = syn::parse_file(source).unwrap_or_else(|e| panic!("{file}: {e}"));
    if !compiled(&parsed.attrs) {
        return Vec::new();
    }
    let mut functions = Functions { found: Vec::new() };
    functions.visit_file(&parsed);
    functions.found
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root")
}

/// Every `.rs` file under `dir`, as `(path from the root with /, source)`.
fn sources_in(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources_in(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path).unwrap();
            out.push((name, source));
        }
    }
}

#[test]
fn only_appdirs_reads_a_folder_variable() {
    let root = repo_root();
    let mut sources = Vec::new();
    for folder in FOLDERS {
        sources_in(&root, &root.join(folder), &mut sources);
    }
    assert!(
        sources.iter().any(|(f, _)| f == READER),
        "the scan reaches {READER}"
    );
    let mut unknown = Vec::new();
    let mut seen = Vec::new();
    for (file, source) in sources.iter().filter(|(f, _)| f != READER) {
        for read in reads_in(file, source) {
            if KNOWN
                .iter()
                .any(|(f, function, _)| f == file && *function == read.function)
            {
                seen.push((file.clone(), read.function));
            } else {
                unknown.push(format!("{file}: {} names {:?}", read.function, read.names));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "a folder variable read outside appdirs; take the folder from \
         `appdirs` (the machine's from the system, the service account's \
         profile from the profile list), or list the read in KNOWN with why:\n{}",
        unknown.join("\n")
    );
    for (file, function, _) in KNOWN {
        assert!(
            seen.iter().any(|(f, n)| f == file && n == function),
            "a stale KNOWN row: {file} {function}"
        );
    }
}

#[test]
fn the_gate_sees_what_it_is_for() {
    let named = |source: &str| -> Vec<String> {
        reads_in("t.rs", source)
            .into_iter()
            .map(|r| r.function)
            .collect()
    };
    // Every case is read, and every miss is listed at once.
    let mut missed = Vec::new();
    for (source, function) in [
        (r#"fn f() { std::env::var_os("PROGRAMDATA"); }"#, "f"),
        (r#"fn f() { std::env::var("ProgramFiles").ok(); }"#, "f"),
        (r#"fn f() { env::var_os("appdata"); }"#, "f"),
        (r#"fn f() { var_os("LOCALAPPDATA"); }"#, "f"),
        (
            r#"fn f() { ["ProgramFiles", "ProgramW6432"].iter().filter_map(std::env::var_os); }"#,
            "f",
        ),
        (
            r#"fn f() { for (v, d) in [("USERPROFILE", 1)] { std::env::var(v); } }"#,
            "f",
        ),
        (
            r#"impl T { fn g() { std::env::var_os("ALLUSERSPROFILE"); } }"#,
            "g",
        ),
        (
            r#"#[cfg(windows)] fn f() { std::env::var_os("ProgramData"); }"#,
            "f",
        ),
        (
            r#"#[cfg(not(unix))] fn f() { std::env::var_os("ProgramData"); }"#,
            "f",
        ),
        (
            r#"#[cfg(feature = "x")] fn f() { std::env::var_os("ProgramData"); }"#,
            "f",
        ),
        (
            r#"#[cfg(not(feature = "x"))] fn f() { std::env::var_os("ProgramData"); }"#,
            "f",
        ),
        (
            r#"fn f() { #[cfg(target_os = "windows")] { std::env::var_os("ProgramData"); } }"#,
            "f",
        ),
        (
            r#"static S: X = X::new(|| std::env::var_os("APPDATA"));"#,
            "S",
        ),
        // #3778: the name and the read in two items.
        (
            r#"const NAME: &str = "APPDATA"; fn f() { std::env::var_os(NAME); }"#,
            "f",
        ),
        (
            r#"static NAMES: [&str; 2] = ["TEMP", "TMP"]; fn f() { NAMES.iter().filter_map(std::env::var_os); }"#,
            "f",
        ),
        (
            r#"impl T { const DIR: &str = "LOCALAPPDATA"; fn g() { std::env::var_os(Self::DIR); } }"#,
            "g",
        ),
        (
            r#"fn node_env(name: &str) -> Option<OsString> { std::env::var_os(name) } fn f() { node_env("APPDATA"); }"#,
            "f",
        ),
        // A walk of the whole environment.
        (
            r#"fn f() { std::env::vars().find(|(k, _)| k.eq_ignore_ascii_case("APPDATA")); }"#,
            "f",
        ),
        (
            r#"fn f() { env::vars_os().filter(|(k, _)| k == "USERPROFILE").count(); }"#,
            "f",
        ),
        // The name inside a longer string, a read through another API, a
        // renamed import.
        (
            r#"fn f() { std::env::var_os("X"); let _ = r"%LOCALAPPDATA%\roomler"; }"#,
            "f",
        ),
        (
            r#"unsafe fn f() { GetEnvironmentVariableW(PCWSTR(wide("APPDATA").as_ptr()), None); }"#,
            "f",
        ),
        (
            r#"unsafe fn f() { ExpandEnvironmentStringsW(PCWSTR(wide(r"%ProgramData%\roomler").as_ptr()), None); }"#,
            "f",
        ),
        (
            r#"use std::env::var_os as folder; fn f() { folder("APPDATA"); }"#,
            "f",
        ),
        (
            r#"use std::env as e; fn f() { e::var_os("APPDATA"); }"#,
            "f",
        ),
        // Anything inside a macro's arguments.
        (
            r#"fn f() { let dir = format!(r"{}\roomler", std::env::var("APPDATA").unwrap()); }"#,
            "f",
        ),
        (
            r#"fn f() { tracing::info!(dir = ?std::env::var_os("APPDATA"), "found"); }"#,
            "f",
        ),
        (
            r#"unsafe fn f() { GetEnvironmentVariableW(w!("APPDATA"), None); }"#,
            "f",
        ),
        // Names that also name folders.
        (r#"fn f() { std::env::var_os("TEMP"); }"#, "f"),
        (r#"fn f() { std::env::var_os("tmp"); }"#, "f"),
        (r#"fn f() { std::env::var_os("HOMEDRIVE"); }"#, "f"),
        (r#"fn f() { std::env::var_os("HomePath"); }"#, "f"),
        // macOS and Linux: a `directories` lookup takes the folders from the
        // environment there ($HOME, $XDG_*).
        (r#"fn f() { directories::BaseDirs::new(); }"#, "f"),
        (
            r#"use directories::ProjectDirs; fn f() { ProjectDirs::from("ai", "roomler", "x"); }"#,
            "f",
        ),
        (
            r#"#[cfg(target_os = "macos")] fn f() { directories::UserDirs::new(); }"#,
            "f",
        ),
        (
            r#"#[cfg(unix)] fn f() { directories::BaseDirs::new(); }"#,
            "f",
        ),
    ] {
        if named(source) != [function] {
            missed.push(format!("not seen as a read: {source}"));
        }
    }
    for source in [
        r#"fn f() { p.join("AppData").join("Local"); }"#,
        // A helper that takes the name and does not read the environment.
        r#"fn node_env(name: &str) -> String { name.to_owned() } fn f() { node_env("APPDATA"); }"#,
        r#"fn f() { cmd.env("APPDATA", x); }"#,
        r#"fn f() { std::env::var_os("SystemRoot"); }"#,
        r#"#[cfg(test)] mod tests { fn f() { std::env::var_os("ProgramData"); } }"#,
        r#"#[cfg(unix)] fn f() { std::env::var_os("ProgramData"); }"#,
        r#"#[cfg(target_os = "linux")] fn f() { std::env::var_os("APPDATA"); }"#,
        r#"#[cfg(not(windows))] fn f() { std::env::var_os("APPDATA"); }"#,
        r#"fn f() { #[cfg(unix)] { std::env::var_os("ProgramData"); } }"#,
        r#"#![cfg(unix)] fn f() { std::env::var_os("ProgramData"); }"#,
        // A doc comment is not code.
        "/// Reads %APPDATA%.\nfn f() { std::env::var_os(\"X\"); }",
        // On Windows `directories` asks the system for the known folders.
        r#"#[cfg(windows)] fn f() { directories::BaseDirs::new(); }"#,
        // A type is not a lookup.
        r#"fn f(dirs: &ProjectDirs) -> &Path { dirs.config_dir() }"#,
    ] {
        if !named(source).is_empty() {
            missed.push(format!("seen as a read: {source}"));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

/// The package whose link graph the gate follows: the daemon.
const ROOT: &str = "roomlerd";

/// Packages in this repository that [`ROOT`] links and the gate does not
/// scan: `(package, why)`. A stale row fails, as a stale [`KNOWN`] row does.
const NOT_SCANNED: &[(&str, &str)] = &[
    ("rtp", VENDORED),
    ("scrap", VENDORED),
    ("webrtc", VENDORED),
    ("webrtc-ice", VENDORED),
    ("webrtc-sctp", VENDORED),
    ("wintun-bindings", VENDORED),
];

const VENDORED: &str = "a vendored fork of third-party code, patched in at the workspace \
                        root (`[patch.crates-io]`): linked, not first-party code";

/// A workspace member: the files its library and binaries start from, and
/// the packages it names as normal dependencies, on every target and behind
/// every feature.
#[derive(Default)]
struct Member {
    lib: Vec<PathBuf>,
    bin: Vec<PathBuf>,
    normal: BTreeSet<String>,
}

/// The packages in this repository that [`ROOT`] links, read from the
/// workspace's own records so that the test needs neither the network nor a
/// registry cache: `cargo metadata --no-deps --offline` reads only the
/// members' manifests (their targets, and each dependency with its kind),
/// and `Cargo.lock` holds the resolved edges through the packages that are
/// not members (a registry crate, a vendored fork).
struct Graph {
    members: BTreeMap<String, Member>,
    /// Every linked package without a registry source: a member, or a fork
    /// patched in by path.
    linked: BTreeSet<String>,
}

impl Graph {
    fn read(root: &Path) -> Self {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out = std::process::Command::new(cargo)
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
            ])
            .arg("--manifest-path")
            .arg(root.join("Cargo.toml"))
            .output()
            .expect("cargo metadata");
        assert!(
            out.status.success(),
            "cargo metadata: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("Cargo.lock");
        Self::from(&out.stdout, &lock)
    }

    fn from(metadata: &[u8], lock: &str) -> Self {
        let metadata: serde_json::Value = serde_json::from_slice(metadata).expect("metadata JSON");
        let mut members = BTreeMap::new();
        for package in metadata["packages"].as_array().expect("packages") {
            let mut member = Member::default();
            for target in package["targets"].as_array().expect("targets") {
                let src = PathBuf::from(target["src_path"].as_str().expect("src_path"));
                let kind = |k: &str| {
                    target["kind"]
                        .as_array()
                        .is_some_and(|v| v.contains(&k.into()))
                };
                // A library of any crate type except a proc-macro, which runs
                // in the compiler and is not linked.
                if ["lib", "rlib", "dylib", "cdylib", "staticlib"]
                    .into_iter()
                    .any(kind)
                {
                    member.lib.push(src);
                } else if kind("bin") {
                    member.bin.push(src);
                }
            }
            for dependency in package["dependencies"].as_array().expect("dependencies") {
                // `kind` is null for a normal dependency, "dev" or "build"
                // otherwise; `target` and `optional` are not looked at.
                if dependency["kind"].is_null() {
                    member
                        .normal
                        .insert(dependency["name"].as_str().expect("name").into());
                }
            }
            members.insert(package["name"].as_str().expect("name").into(), member);
        }
        let lock: toml::Table = toml::from_str(lock).expect("Cargo.lock TOML");
        // (name, version, from a registry, dependencies as the lock writes them)
        let packages: Vec<(&str, &str, bool, Vec<&str>)> = lock["package"]
            .as_array()
            .expect("[[package]]")
            .iter()
            .map(|p| {
                let deps = p.get("dependencies").and_then(toml::Value::as_array);
                (
                    p["name"].as_str().expect("name"),
                    p["version"].as_str().expect("version"),
                    p.get("source").is_some(),
                    deps.into_iter()
                        .flatten()
                        .filter_map(toml::Value::as_str)
                        .collect(),
                )
            })
            .collect();
        let mut queue: Vec<usize> = packages
            .iter()
            .position(|p| p.0 == ROOT && !p.2)
            .into_iter()
            .collect();
        assert!(!queue.is_empty(), "{ROOT} is not in Cargo.lock");
        let mut seen = BTreeSet::new();
        let mut linked = BTreeSet::new();
        while let Some(at) = queue.pop() {
            if !seen.insert(at) {
                continue;
            }
            let (name, _, registry, deps) = &packages[at];
            if !registry {
                linked.insert(name.to_string());
            }
            // The lock records a member's dev and build dependencies too, so
            // a member's edges are only the normal ones its manifest names.
            // Another package's are all the lock records: the lock holds no
            // dev-dependency of a package that is not a member.
            let normal = members.get(*name).filter(|_| !registry).map(|m| &m.normal);
            for dep in deps {
                // "name", "name version" or "name version (source)".
                let mut parts = dep.split(' ');
                let (dep, version) = (parts.next().unwrap_or_default(), parts.next());
                if normal.is_some_and(|normal| !normal.contains(dep)) {
                    continue;
                }
                queue.extend(packages.iter().enumerate().filter_map(|(i, p)| {
                    (p.0 == dep && version.is_none_or(|v| p.1 == v)).then_some(i)
                }));
            }
        }
        Self { members, linked }
    }
}

/// The packages the gate scans.
fn scanned(graph: &Graph, root: &Path) -> BTreeSet<String> {
    graph
        .members
        .iter()
        .filter(|(_, m)| {
            m.lib.iter().chain(&m.bin).any(|src| {
                let src = src.canonicalize().unwrap_or_else(|_| src.clone());
                FOLDERS.iter().any(|f| src.starts_with(root.join(f)))
            })
        })
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn the_scan_is_what_roomlerd_links() {
    let root = repo_root();
    let graph = Graph::read(&root);
    let scanned = scanned(&graph, &root);
    let unscanned: Vec<&String> = graph
        .linked
        .iter()
        .filter(|p| !scanned.contains(*p) && !NOT_SCANNED.iter().any(|(n, _)| n == p))
        .collect();
    assert!(
        unscanned.is_empty(),
        "packages in this repository that {ROOT} links and the gate does not scan; scan \
         them, or list each in NOT_SCANNED with why: {unscanned:?}"
    );
    for (package, _) in NOT_SCANNED {
        assert!(
            graph.linked.contains(*package),
            "a stale NOT_SCANNED row: {package}"
        );
    }
    for package in &scanned {
        assert!(
            graph.linked.contains(package),
            "the gate scans {package}, which {ROOT} does not link"
        );
    }
}

/// The walk on a made-up workspace: normal dependencies on every target and
/// behind every feature count, a dev- or build-dependency does not, and a
/// fork is reached through the registry crate that depends on it.
#[test]
fn the_walk_follows_what_is_linked() {
    let member = |name: &str, deps: &str| {
        format!(
            r#"{{"name":"{name}","targets":[{{"kind":["lib"],"src_path":"/r/{name}/src/lib.rs"}}],"dependencies":[{deps}]}}"#
        )
    };
    let metadata = format!(
        r#"{{"packages":[{},{},{},{},{},{}]}}"#,
        member(
            ROOT,
            r#"{"name":"a","kind":null},{"name":"b","kind":null,"target":"cfg(windows)","optional":true},{"name":"c","kind":"dev"},{"name":"d","kind":"build"},{"name":"reg","kind":null}"#
        ),
        member("a", ""),
        member("b", ""),
        member("c", ""),
        member("d", ""),
        member("e", ""),
    );
    let lock = format!(
        r#"
[[package]]
name = "{ROOT}"
version = "0.1.0"
dependencies = ["a", "b", "c", "d", "reg"]
[[package]]
name = "a"
version = "0.1.0"
[[package]]
name = "b"
version = "0.1.0"
[[package]]
name = "c"
version = "0.1.0"
dependencies = ["e"]
[[package]]
name = "d"
version = "0.1.0"
[[package]]
name = "e"
version = "0.1.0"
[[package]]
name = "reg"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
dependencies = ["fork 2.0.0"]
[[package]]
name = "fork"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
[[package]]
name = "fork"
version = "2.0.0"
"#
    );
    let graph = Graph::from(metadata.as_bytes(), &lock);
    assert_eq!(
        graph.linked.iter().map(String::as_str).collect::<Vec<_>>(),
        ["a", "b", "fork", ROOT]
    );
}
