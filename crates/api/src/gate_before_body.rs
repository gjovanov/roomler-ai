// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! #4218 — a gated write decides its gate BEFORE it reads its body.
//!
//! axum runs a handler's extractors before the handler's first line. A write
//! that takes its body as `Json<T>` therefore answers a body that does not
//! parse (400, 413, 415 or 422) before any gate in the handler has run: a
//! caller the gate refuses is told about the body instead of being refused,
//! and a platform-operator route, which answers a non-operator 404 so that it
//! stays unseen, is seen. So a gated write takes
//! `Result<Json<T>, JsonRejection>` and unwraps it straight after its gate
//! (`let Json(body) = body?;`); `ApiError::JsonBody` answers a caller the
//! gate lets through what the extractor answered (#4187).
//!
//! This test reads the server's source (this crate, `roomler-core` and every
//! `roomler-ai-mod-*` crate, the files a non-test build compiles) and fails
//! when
//!
//! 1. a function takes its body as `Json<T>` or `Option<Json<T>>` (both
//!    extractors answer before the handler runs) and calls a gate, or
//! 2. a function takes `Result<Json<T>, _>` and uses it in, or before, the
//!    first statement that calls a gate.
//!
//! A **gate** is a function other than a handler that checks a permission
//! bit (`permissions::has`) or the platform-operator allowlist
//! (`platform_admins.contains`), or that calls a gate; a handler's own check
//! counts for that handler. So `require_permission`, `require_platform_admin`
//! and every helper built on them are gates without being named here.
//!
//! What it flags today is listed, each entry with its reason, and both lists
//! may only shrink: an entry the scan no longer flags fails the test until it
//! is removed.
//! - [`DEFERRED`]: gated writes outside the network module, fixed by #4254.
//! - [`BY_DESIGN`]: handlers whose check needs the body, or that read the
//!   allowlist to shape their answer rather than to refuse.
//!
//! What it cannot see:
//! - A check it does not recognise as a gate: an ownership or author check
//!   (`chat::message::update` is one, #4254), a bit read some other way, a
//!   gate called as a method, through a closure or a function pointer, inside
//!   a macro's arguments (`syn` leaves those unparsed), or defined outside the
//!   scanned crates.
//! - A call it cannot resolve. A call resolves by name: to the function of the
//!   calling file, else to the one file that defines the name, else to the
//!   file the path's module segment names (`agent_exec::authorize`). A name
//!   that several files define, as `new` or `init`, is otherwise not
//!   followed.
//! - It reads source, not the router. A handler a macro writes is unseen, and
//!   a function no route mounts is still checked.
//! - Only `Json` bodies. `Query<T>`, a typed `Path<T>`, `Form<T>`,
//!   `Multipart` and a `Bytes` or `String` body's 413 also answer before the
//!   handler runs, and are not checked.
//! - Check 2 compares top-level statements, and a use of the body inside a
//!   macro's arguments is unseen.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::visit::Visit;

/// #4254 — gated writes outside the network module that still take their body
/// as `Json<T>`: `(file, function, issue)`. Fix one the #4218 way and remove
/// its entry; the test fails while a fixed one is still listed.
const DEFERRED: &[(&str, &str, &str)] = &[
    ("crates/modules/chat/src/membership.rs", "prune", "#4254"),
    ("crates/modules/chat/src/room.rs", "update", "#4254"),
    (
        "crates/modules/fleet/src/agent.rs",
        "trigger_agent_update",
        "#4254",
    ),
    (
        "crates/modules/fleet/src/agent.rs",
        "trigger_agents_update",
        "#4254",
    ),
    ("crates/modules/fleet/src/agent.rs", "update_agent", "#4254"),
    (
        "crates/modules/fleet/src/agent_exec.rs",
        "set_org_settings",
        "#4254",
    ),
    (
        "crates/modules/fleet/src/agent_exec.rs",
        "set_policy",
        "#4254",
    ),
    ("crates/modules/fleet/src/agent_org.rs", "join_org", "#4254"),
    (
        "crates/modules/fleet/src/enroll_key.rs",
        "mint_key",
        "#4254",
    ),
    (
        "crates/modules/fleet/src/enroll_key.rs",
        "set_org_settings",
        "#4254",
    ),
    (
        "crates/modules/fleet/src/external_access.rs",
        "set_enabled",
        "#4254",
    ),
    (
        "crates/modules/fleet/src/keep_busy.rs",
        "set_org_settings",
        "#4254",
    ),
    (
        "crates/modules/remote/src/session_policy.rs",
        "set_limits",
        "#4254",
    ),
    (
        "crates/modules/remote/src/session_policy.rs",
        "set_policy",
        "#4254",
    ),
    ("crates/modules/saas/src/newsletter.rs", "create", "#4254"),
    ("crates/modules/saas/src/newsletter.rs", "send", "#4254"),
    (
        "crates/modules/saas/src/newsletter.rs",
        "test_send",
        "#4254",
    ),
    ("crates/modules/saas/src/newsletter.rs", "update", "#4254"),
    ("crates/modules/saas/src/stripe.rs", "change_plan", "#4254"),
    (
        "crates/modules/saas/src/stripe.rs",
        "create_checkout",
        "#4254",
    ),
    (
        "crates/modules/saas/src/stripe.rs",
        "create_portal",
        "#4254",
    ),
];

/// Handlers that read their body before a gate on purpose:
/// `(file, function, why)`.
const BY_DESIGN: &[(&str, &str, &str)] = &[
    (
        "crates/api/src/routes/auth.rs",
        "login",
        "reads the platform allowlist for `is_platform_admin` in its answer, not to refuse",
    ),
    (
        "crates/api/src/routes/auth.rs",
        "refresh",
        "reads the platform allowlist for `is_platform_admin` in its answer, not to refuse",
    ),
    (
        "crates/api/src/routes/auth.rs",
        "register",
        "reads the platform allowlist for `is_platform_admin` in its answer, not to refuse",
    ),
    (
        "crates/modules/fleet/src/agent.rs",
        "enroll_agent",
        "the enrollment credential is the body: `issuer_may_enroll` asks its minter's bit, \
         not the caller's",
    ),
    (
        "crates/modules/fleet/src/exec_service.rs",
        "anchor",
        "the enrollment credential is the body: `issuer_may_enroll` asks its minter's bit, \
         not the caller's",
    ),
    (
        "crates/modules/fleet/src/agent_exec.rs",
        "exec",
        "membership first; the exec chain decides in `dispatch` from the body's shell, and \
         audits each refusal, a 200 result, with the command",
    ),
    (
        "crates/modules/fleet/src/agent_exec.rs",
        "exec_bulk",
        "membership first; the exec chain decides in `dispatch` from the body's shell, and \
         audits each refusal, a 200 result, with the command",
    ),
    (
        "crates/modules/fleet/src/external_access.rs",
        "set_policy",
        "membership first; the bit it needs depends on what the body changes \
         (`decide_approval` compares the stored and the requested policy)",
    ),
    (
        "crates/modules/fleet/src/external_access.rs",
        "set_principal",
        "membership first; the bit it needs depends on whether the body revokes or restores \
         (`decide_revocation`), and the audit row records which",
    ),
    (
        "crates/modules/fleet/src/remote_config.rs",
        "set_desired_config",
        "the bit it needs depends on what the body changes (`decide` compares the stored and \
         the requested config)",
    ),
    (
        "crates/modules/network/src/routes/agent_ssh.rs",
        "request_session",
        "membership first; the SSH chain decides in `dispatch` from the body's key and \
         length, and audits every refusal",
    ),
    (
        "crates/modules/network/src/routes/peer_relay.rs",
        "set_policy",
        "membership first; the bit it needs depends on what the body changes \
         (`decide_approval` compares the stored and the requested policy)",
    ),
    (
        "crates/modules/network/src/routes/tunnel.rs",
        "enroll_tunnel_client",
        "the enrollment credential and the machine id are the body: whether a bit is asked \
         depends on the machine it names (`revive_scope`)",
    ),
];

/// One function of the scanned source.
struct Func<'a> {
    file: &'a str,
    name: String,
    sig: &'a syn::Signature,
    block: &'a syn::Block,
}

/// A function the scan flags, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    file: String,
    name: String,
    why: String,
}

/// The gates of the scanned source, by `(file, function)`, with what made
/// each one a gate.
type Gates = BTreeMap<(String, String), Why>;

/// The files that define each function name.
type Index = BTreeMap<String, BTreeSet<String>>;

/// What made a function a gate.
#[derive(Debug, Clone)]
enum Why {
    /// It calls `permissions::has`.
    Bit,
    /// It calls `platform_admins.contains`.
    Allowlist,
    /// It calls the gate this path names.
    Calls(Vec<String>),
}

/// The calls that an expression, a statement or a block makes.
#[derive(Default)]
struct Calls {
    /// The path of every path call: `a::b::c(…)` gives `["a", "b", "c"]`.
    paths: BTreeSet<Vec<String>>,
    /// A `permissions::has(…)` call: a permission bit is checked.
    bit: bool,
    /// A `….platform_admins.contains(…)` call: the platform-operator allowlist.
    allowlist: bool,
    /// Every single-segment path read (`body`), for check 2.
    idents: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Calls {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(func) = &*call.func {
            let path: Vec<String> = func
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            if path.ends_with(&["permissions".to_owned(), "has".to_owned()]) {
                self.bit = true;
            }
            if !path.is_empty() {
                self.paths.insert(path);
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "contains" && names_field(&call.receiver, "platform_admins") {
            self.allowlist = true;
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if let Some(ident) = path.path.get_ident() {
            self.idents.insert(ident.to_string());
        }
        syn::visit::visit_expr_path(self, path);
    }
}

impl Calls {
    fn of_block(block: &syn::Block) -> Self {
        let mut calls = Self::default();
        calls.visit_block(block);
        calls
    }

    fn of_stmt(stmt: &syn::Stmt) -> Self {
        let mut calls = Self::default();
        calls.visit_stmt(stmt);
        calls
    }
}

/// Whether `expr` is `….field` or `field`, through `&`, `*` and `(…)`.
fn names_field(expr: &syn::Expr, field: &str) -> bool {
    match expr {
        syn::Expr::Field(f) => matches!(&f.member, syn::Member::Named(name) if name == field),
        syn::Expr::Path(p) => p.path.is_ident(field),
        syn::Expr::Reference(r) => names_field(&r.expr, field),
        syn::Expr::Paren(p) => names_field(&p.expr, field),
        syn::Expr::Unary(u) => names_field(&u.expr, field),
        _ => false,
    }
}

/// How a parameter takes the request body.
enum Body {
    /// `Json<T>` or `Option<Json<T>>`: read by the extractor, before the
    /// handler's first line.
    Extracted,
    /// `Result<Json<T>, _>` bound to this name (`None`: to a pattern the scan
    /// cannot follow): read where the handler unwraps it.
    Deferred(Option<String>),
}

fn last_segment(ty: &syn::Type) -> Option<&syn::PathSegment> {
    match ty {
        syn::Type::Path(p) if p.qself.is_none() => p.path.segments.last(),
        syn::Type::Paren(p) => last_segment(&p.elem),
        syn::Type::Group(g) => last_segment(&g.elem),
        _ => None,
    }
}

fn first_type_argument(segment: &syn::PathSegment) -> Option<&syn::Type> {
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })
}

fn is_json(ty: &syn::Type) -> bool {
    last_segment(ty).is_some_and(|segment| segment.ident == "Json")
}

fn body(argument: &syn::FnArg) -> Option<Body> {
    let syn::FnArg::Typed(typed) = argument else {
        return None;
    };
    let segment = last_segment(&typed.ty)?;
    if segment.ident == "Json" {
        return Some(Body::Extracted);
    }
    let inner = first_type_argument(segment)?;
    if segment.ident == "Option" && is_json(inner) {
        return Some(Body::Extracted);
    }
    if segment.ident == "Result" && is_json(inner) {
        let name = match &*typed.pat {
            syn::Pat::Ident(binding) => Some(binding.ident.to_string()),
            _ => None,
        };
        return Some(Body::Deferred(name));
    }
    None
}

/// A handler: it takes `State<_>` or a body. Only a router calls one, so a
/// handler that calls a gate does not make its callers gated.
fn is_handler(sig: &syn::Signature) -> bool {
    sig.inputs.iter().any(|argument| {
        body(argument).is_some()
            || matches!(argument, syn::FnArg::Typed(typed)
                if last_segment(&typed.ty).is_some_and(|segment| segment.ident == "State"))
    })
}

/// Whether a `cfg` predicate holds only in a test build.
fn needs_test(meta: &syn::Meta) -> bool {
    match meta {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) => {
            let inner = list
                .parse_args_with(Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated)
                .unwrap_or_default();
            if list.path.is_ident("all") {
                inner.iter().any(needs_test)
            } else if list.path.is_ident("any") {
                !inner.is_empty() && inner.iter().all(needs_test)
            } else {
                false
            }
        }
        syn::Meta::NameValue(_) => false,
    }
}

/// Whether an item carries a `cfg` that only a test build satisfies.
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<syn::Meta>().is_ok_and(|m| needs_test(&m))
    })
}

fn functions<'a>(file: &'a str, items: &'a [syn::Item], out: &mut Vec<Func<'a>>) {
    for item in items {
        match item {
            syn::Item::Fn(f) if !test_only(&f.attrs) => out.push(Func {
                file,
                name: f.sig.ident.to_string(),
                sig: &f.sig,
                block: &f.block,
            }),
            syn::Item::Impl(block) if !test_only(&block.attrs) => {
                for f in block.items.iter().filter_map(|item| match item {
                    syn::ImplItem::Fn(f) if !test_only(&f.attrs) => Some(f),
                    _ => None,
                }) {
                    out.push(Func {
                        file,
                        name: f.sig.ident.to_string(),
                        sig: &f.sig,
                        block: &f.block,
                    });
                }
            }
            syn::Item::Mod(m) if !test_only(&m.attrs) => {
                if let Some((_, items)) = &m.content {
                    functions(file, items, out);
                }
            }
            _ => {}
        }
    }
}

/// What a scan of some source found.
struct Scan {
    findings: Vec<Finding>,
    gates: Gates,
}

/// Check 1 and check 2 over `files`: `(path, parsed file)`.
fn scan(files: &[(String, syn::File)]) -> Scan {
    let mut funcs = Vec::new();
    for (file, parsed) in files {
        if !test_only(&parsed.attrs) {
            functions(file, &parsed.items, &mut funcs);
        }
    }
    let calls: Vec<Calls> = funcs.iter().map(|f| Calls::of_block(f.block)).collect();
    let mut index = Index::new();
    for f in &funcs {
        index
            .entry(f.name.clone())
            .or_default()
            .insert(f.file.to_owned());
    }

    // The gates: the functions other than handlers that check a bit or the
    // allowlist themselves, then, until nothing changes, those that call one.
    let mut gates = Gates::new();
    for (f, c) in funcs.iter().zip(&calls) {
        if is_handler(f.sig) {
            continue;
        }
        let why = if c.bit {
            Why::Bit
        } else if c.allowlist {
            Why::Allowlist
        } else {
            continue;
        };
        gates
            .entry((f.file.to_owned(), f.name.clone()))
            .or_insert(why);
    }
    loop {
        let mut added = Vec::new();
        for (f, c) in funcs.iter().zip(&calls) {
            let key = (f.file.to_owned(), f.name.clone());
            if is_handler(f.sig) || gates.contains_key(&key) {
                continue;
            }
            if let Some(path) = c
                .paths
                .iter()
                .find(|path| is_gate(&gates, &index, f.file, path))
            {
                added.push((key, Why::Calls(path.clone())));
            }
        }
        if added.is_empty() {
            break;
        }
        gates.extend(added);
    }

    let gate_calls = |f: &Func<'_>, c: &Calls| -> Vec<String> {
        let mut out: Vec<String> = c
            .paths
            .iter()
            .filter(|path| is_gate(&gates, &index, f.file, path))
            .map(|path| chain(&gates, &index, f.file, path))
            .collect();
        if c.bit {
            out.push("permissions::has".to_owned());
        }
        if c.allowlist {
            out.push("platform_admins.contains".to_owned());
        }
        out
    };

    let mut findings = Vec::new();
    for (f, c) in funcs.iter().zip(&calls) {
        for argument in &f.sig.inputs {
            let Some(taken) = body(argument) else {
                continue;
            };
            let called = gate_calls(f, c);
            if called.is_empty() {
                continue;
            }
            let why = match taken {
                Body::Extracted => {
                    format!("takes its body as Json<T> and calls {}", called.join(", "))
                }
                Body::Deferred(None) => {
                    "binds its Result<Json<T>, _> to a pattern the guard cannot follow".to_owned()
                }
                Body::Deferred(Some(binding)) => {
                    let statements: Vec<Calls> = f.block.stmts.iter().map(Calls::of_stmt).collect();
                    let gate = statements.iter().position(|s| {
                        s.bit
                            || s.allowlist
                            || s.paths
                                .iter()
                                .any(|path| is_gate(&gates, &index, f.file, path))
                    });
                    let read = statements.iter().position(|s| s.idents.contains(&binding));
                    match (gate, read) {
                        (Some(gate), Some(read)) if read <= gate => format!(
                            "uses `{binding}` in statement {} and calls its gate in statement {}",
                            read + 1,
                            gate + 1
                        ),
                        _ => continue,
                    }
                }
            };
            findings.push(Finding {
                file: f.file.to_owned(),
                name: f.name.clone(),
                why,
            });
        }
    }
    findings.sort();
    findings.dedup();
    Scan { findings, gates }
}

/// The module a file is: its stem, or its folder's name for a `mod.rs` or a
/// crate root.
fn module_of(file: &str) -> Option<&str> {
    let mut parts = file.rsplit('/');
    let stem = parts.next()?.strip_suffix(".rs")?;
    if !matches!(stem, "mod" | "lib" | "main") {
        return Some(stem);
    }
    parts.find(|part| *part != "src")
}

/// The gate a call of `path` from `file` reaches, if it reaches one: the
/// function of that name in `file` when the file defines one, else the one
/// file that defines the name, else the one the path's module segment names.
fn resolve<'g>(
    gates: &'g Gates,
    index: &Index,
    file: &str,
    path: &[String],
) -> Option<(&'g (String, String), &'g Why)> {
    let name = path.last()?;
    let files = index.get(name)?;
    let at: &str = if files.contains(file) {
        file
    } else if files.len() == 1 {
        files.first()?.as_str()
    } else {
        let module = path.get(path.len().checked_sub(2)?)?.as_str();
        let mut named = files
            .iter()
            .filter(|candidate| module_of(candidate) == Some(module));
        let one = named.next()?;
        if named.next().is_some() {
            return None;
        }
        one.as_str()
    };
    gates.get_key_value(&(at.to_owned(), name.clone()))
}

fn is_gate(gates: &Gates, index: &Index, file: &str, path: &[String]) -> bool {
    resolve(gates, index, file, path).is_some()
}

/// The call and the gates it reaches, down to the check itself:
/// `gate → require_permission → permissions::has`.
fn chain(gates: &Gates, index: &Index, file: &str, path: &[String]) -> String {
    let mut out = path.last().cloned().unwrap_or_default();
    let (mut file, mut path) = (file.to_owned(), path.to_vec());
    for _ in 0..8 {
        let Some(((at, _), why)) = resolve(gates, index, &file, &path) else {
            break;
        };
        match why {
            Why::Bit => return format!("{out} → permissions::has"),
            Why::Allowlist => return format!("{out} → platform_admins.contains"),
            Why::Calls(next) => {
                out = format!(
                    "{out} → {}",
                    next.last().map(String::as_str).unwrap_or_default()
                );
                (file, path) = (at.clone(), next.clone());
            }
        }
    }
    out
}

/// The repository root, as `../..` from this crate. Not canonicalized: a
/// verbatim Windows path takes no `/` in what is joined to it.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `relative`, written with `/`, below `base`, one component at a time.
fn at(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

/// The crate roots the scan starts from: this crate, the core and every
/// module.
fn crate_roots(repo: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        at(repo, "crates/api/src/lib.rs"),
        at(repo, "crates/api/src/main.rs"),
        at(repo, "crates/core/src/lib.rs"),
    ];
    let mut modules: Vec<PathBuf> = std::fs::read_dir(at(repo, "crates/modules"))
        .expect("crates/modules")
        .map(|entry| entry.expect("a module folder").path())
        .filter(|path| path.join("Cargo.toml").is_file())
        .collect();
    modules.sort();
    roots.extend(modules.into_iter().map(|module| at(&module, "src/lib.rs")));
    roots
}

/// Every file of the module tree below each root that a non-test build
/// compiles, parsed: `(path from the repository root with /, file)`.
fn read_tree(repo: &Path) -> Vec<(String, syn::File)> {
    let mut todo: Vec<(PathBuf, PathBuf)> = crate_roots(repo)
        .into_iter()
        .map(|root| {
            assert!(root.is_file(), "{}: no such crate root", root.display());
            let dir = root.parent().expect("src").to_path_buf();
            (root, dir)
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    while let Some((file, dir)) = todo.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let source =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let parsed = syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        mod_files(&parsed.items, &dir, &mut todo);
        let relative = file.strip_prefix(repo).expect("a file of the repository");
        let relative: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        out.push((relative.join("/"), parsed));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The file of every `mod name;` among `items` that a non-test build
/// compiles, with the folder its own `mod`s live in.
fn mod_files(items: &[syn::Item], dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        if test_only(&m.attrs) {
            continue;
        }
        let name = m.ident.to_string();
        assert!(
            !m.attrs.iter().any(|attr| attr.path().is_ident("path")),
            "mod {name} in {}: the scan does not follow #[path]",
            dir.display()
        );
        let sub = dir.join(&name);
        match &m.content {
            Some((_, items)) => mod_files(items, &sub, out),
            None => {
                let file = [dir.join(format!("{name}.rs")), sub.join("mod.rs")]
                    .into_iter()
                    .find(|f| f.is_file())
                    .unwrap_or_else(|| panic!("mod {name}: no file in {}", dir.display()));
                out.push((file, sub));
            }
        }
    }
}

#[test]
fn every_gated_write_decides_its_gate_before_it_reads_its_body() {
    let repo = repo_root();
    let files = read_tree(&repo);
    assert!(
        files.len() > 100,
        "the scan read {} files: is it reading the server's crates?",
        files.len()
    );
    let found = scan(&files).findings;

    let listed: BTreeSet<(&str, &str)> = DEFERRED
        .iter()
        .chain(BY_DESIGN)
        .map(|(file, name, _)| (*file, *name))
        .collect();
    assert_eq!(
        listed.len(),
        DEFERRED.len() + BY_DESIGN.len(),
        "a handler is listed twice"
    );
    let flagged: BTreeSet<(&str, &str)> = found
        .iter()
        .map(|f| (f.file.as_str(), f.name.as_str()))
        .collect();

    let new: Vec<String> = found
        .iter()
        .filter(|f| !listed.contains(&(f.file.as_str(), f.name.as_str())))
        .map(|f| format!("  {} fn {}: {}", f.file, f.name, f.why))
        .collect();
    let stale: Vec<String> = listed
        .iter()
        .filter(|key| !flagged.contains(*key))
        .map(|(file, name)| format!("  {file} fn {name}"))
        .collect();

    let mut problems = String::new();
    if !new.is_empty() {
        problems.push_str(&format!(
            "#4218: these read their body before their gate. Take the body as \
             `Result<Json<T>, JsonRejection>` and unwrap it straight after the gate \
             (`let Json(body) = body?;`), as `set_magic_dns` in \
             crates/modules/network/src/routes/overlay_route.rs does. A check that needs \
             the body goes in BY_DESIGN, with why:\n{}\n",
            new.join("\n")
        ));
    }
    if !stale.is_empty() {
        problems.push_str(&format!(
            "listed in DEFERRED or BY_DESIGN, but the scan no longer flags them (fixed, \
             renamed or gone): remove each entry:\n{}\n",
            stale.join("\n")
        ));
    }
    assert!(problems.is_empty(), "{problems}");
}

/// The scan still recognises the core's two gates, so a rename of either, or
/// of what they check, cannot quietly leave every handler unchecked.
#[test]
fn the_scan_recognises_the_core_gates() {
    let gates = scan(&read_tree(&repo_root())).gates;
    for name in ["require_permission", "require_platform_admin"] {
        assert!(
            gates.contains_key(&("crates/core/src/guards.rs".to_owned(), name.to_owned())),
            "crates/core/src/guards.rs {name} is no longer a gate to the scan"
        );
    }
}

/// What the scan flags in in-memory sources: `(path, source)`.
fn flagged(sources: &[(&str, &str)]) -> Vec<(String, String)> {
    let files: Vec<(String, syn::File)> = sources
        .iter()
        .map(|(path, source)| {
            (
                (*path).to_owned(),
                syn::parse_file(source).expect("a source"),
            )
        })
        .collect();
    scan(&files)
        .findings
        .into_iter()
        .map(|f| (f.name, f.why))
        .collect()
}

const CORE: (&str, &str) = (
    "core/guards.rs",
    r#"
    pub async fn require_permission(state: &Core, tid: ObjectId, user: ObjectId, flag: u64) -> Result<(), ApiError> {
        let perms = state.tenants.get_member_permissions(tid, user).await?;
        if !permissions::has(perms, flag) {
            return Err(ApiError::Forbidden("no".into()));
        }
        Ok(())
    }
    pub fn require_platform_admin(state: &Core, auth: &AuthUser) -> Result<(), ApiError> {
        if state.platform_admins.contains(&auth.user_id) { Ok(()) } else { Err(ApiError::NotFound("".into())) }
    }
    "#,
);

#[test]
fn a_gated_handler_that_takes_json_is_flagged() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn set(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
                require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "set");
    assert!(
        found[0].1.contains("require_permission → permissions::has"),
        "{found:?}"
    );
}

#[test]
fn an_optional_json_body_is_read_before_the_handler_too() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn send(State(state): State<Core>, auth: AuthUser, body: Option<Json<Req>>) -> Result<(), ApiError> {
                guards::require_platform_admin(&state, &auth)?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1.contains("platform_admins.contains"), "{found:?}");
}

#[test]
fn a_body_unwrapped_after_its_gate_passes() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn set(State(state): State<Core>, auth: AuthUser, Path(t): Path<String>, body: Result<Json<Req>, JsonRejection>) -> Result<(), ApiError> {
                let tid = parse_tid(&t)?;
                require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                let Json(body) = body?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_body_unwrapped_before_its_gate_is_flagged() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn set(State(state): State<Core>, auth: AuthUser, body: Result<Json<Req>, JsonRejection>) -> Result<(), ApiError> {
                let Json(body) = body?;
                require_permission(&state, body.tid, auth.user_id, MANAGE_AGENTS).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].1.contains("uses `body` in statement 1"),
        "{found:?}"
    );
}

#[test]
fn a_helper_that_calls_a_gate_is_a_gate() {
    let found = flagged(&[
        CORE,
        (
            "helpers.rs",
            r#"
            pub async fn gate(state: &Core, t: &str, auth: &AuthUser) -> Result<ObjectId, ApiError> {
                let tid = parse_tid(t)?;
                require_permission(state, tid, auth.user_id, MANAGE_AGENTS).await?;
                Ok(tid)
            }
            "#,
        ),
        (
            "h.rs",
            r#"
            pub async fn create(State(state): State<Core>, auth: AuthUser, Path(t): Path<String>, Json(body): Json<Req>) -> Result<(), ApiError> {
                let tid = gate(&state, &t, &auth).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0]
            .1
            .contains("gate → require_permission → permissions::has"),
        "{found:?}"
    );
}

#[test]
fn a_check_written_in_the_handler_is_a_gate() {
    let found = flagged(&[(
        "h.rs",
        r#"
        pub async fn reclaim(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
            if !state.platform_admins.contains(&auth.user_id) {
                return Err(ApiError::NotFound("Not found".into()));
            }
            Ok(())
        }
        "#,
    )]);
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn a_handler_with_no_gate_passes() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn login(State(state): State<Core>, Json(body): Json<Login>) -> Result<(), ApiError> {
                let user = state.users.find(&body.username).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_handler_that_calls_a_gate_does_not_gate_its_callers() {
    // `set` is gated, but it is a handler: a function that calls it by name
    // (a test, a wrapper) is not made a gate by it.
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            pub async fn set(State(state): State<Core>, auth: AuthUser, body: Result<Json<Req>, JsonRejection>) -> Result<(), ApiError> {
                require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                let Json(body) = body?;
                Ok(())
            }
            pub async fn other(Json(body): Json<Req>) -> Result<(), ApiError> {
                helper().await
            }
            async fn helper() -> Result<(), ApiError> {
                set(todo!(), todo!(), todo!()).await
            }
            "#,
        ),
    ]);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_function_of_the_calling_file_wins_over_a_gate_of_the_same_name() {
    let found = flagged(&[
        CORE,
        (
            "a.rs",
            r#"
            async fn check(state: &Core, auth: &AuthUser) -> Result<(), ApiError> {
                require_platform_admin(state, auth)
            }
            "#,
        ),
        (
            "b.rs",
            r#"
            fn check(body: &Req) -> Result<(), ApiError> { Ok(()) }
            pub async fn create(State(state): State<Core>, Json(body): Json<Req>) -> Result<(), ApiError> {
                check(&body)?;
                Ok(())
            }
            "#,
        ),
    ]);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_name_several_files_define_is_followed_only_where_its_path_names_the_file() {
    // Two files define `start`, one of them a gate: `start(…)` from a third
    // file (or `Vec::new`, `tokio::spawn`, …) is not followed, and
    // `ops::start(…)` is.
    let found = flagged(&[
        CORE,
        (
            "x/ops.rs",
            r#"
            pub async fn start(state: &Core, auth: &AuthUser) -> Result<(), ApiError> {
                require_platform_admin(state, auth)
            }
            "#,
        ),
        (
            "x/jobs.rs",
            r#"
            pub fn start() {}
            "#,
        ),
        (
            "x/h.rs",
            r#"
            pub async fn unresolved(State(state): State<Core>, Json(body): Json<Req>) -> Result<(), ApiError> {
                start();
                Ok(())
            }
            pub async fn named(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
                crate::ops::start(&state, &auth).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["named"], "{found:?}");
}

#[test]
fn code_only_a_test_build_compiles_is_not_read() {
    let found = flagged(&[
        CORE,
        (
            "h.rs",
            r#"
            #[cfg(test)]
            mod tests {
                pub async fn set(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
                    require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                    Ok(())
                }
            }
            #[cfg(all(test, feature = "x"))]
            pub async fn set_x(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
                require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                Ok(())
            }
            #[cfg(not(test))]
            pub async fn set_y(State(state): State<Core>, auth: AuthUser, Json(body): Json<Req>) -> Result<(), ApiError> {
                require_permission(&state, tid, auth.user_id, MANAGE_AGENTS).await?;
                Ok(())
            }
            "#,
        ),
    ]);
    let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["set_y"], "{found:?}");
}
