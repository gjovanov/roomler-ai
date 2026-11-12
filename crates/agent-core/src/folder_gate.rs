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
    ] {
        assert_eq!(named(source), [function], "{source}");
    }
    for source in [
        r#"fn f() { p.join("AppData").join("Local"); }"#,
        r#"fn f() { node_env("APPDATA"); }"#,
        r#"fn f() { cmd.env("APPDATA", x); }"#,
        r#"fn f() { std::env::var_os("SystemRoot"); }"#,
        r#"#[cfg(test)] mod tests { fn f() { std::env::var_os("ProgramData"); } }"#,
        r#"#[cfg(unix)] fn f() { std::env::var_os("ProgramData"); }"#,
        r#"#[cfg(target_os = "linux")] fn f() { std::env::var_os("APPDATA"); }"#,
        r#"#[cfg(not(windows))] fn f() { std::env::var_os("APPDATA"); }"#,
        r#"fn f() { #[cfg(unix)] { std::env::var_os("ProgramData"); } }"#,
        r#"#![cfg(unix)] fn f() { std::env::var_os("ProgramData"); }"#,
    ] {
        assert!(named(source).is_empty(), "{source}");
    }
}
