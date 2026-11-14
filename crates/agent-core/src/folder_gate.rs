// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3702, #3778 — the gate on where the daemon finds a folder.
//!
//! `%ProgramData%`, `%ProgramFiles%`, `%APPDATA%`, `%LOCALAPPDATA%`,
//! `%USERPROFILE%` and `%TEMP%` name folders, and a process's environment can
//! name other ones. A step on the service account takes the machine's folders
//! from the system (`appdirs::machine_program_data`,
//! `appdirs::machine_program_files`) and its own profile from the profile list
//! (`appdirs::system_profile`), so [`crate::appdirs`] is the one module of the
//! daemon that reads any of these variables. On macOS and Linux a
//! `directories` lookup takes the folders from the environment (`$HOME`,
//! `$XDG_*`), so it is held to the same module.
//!
//! **What the gate scans**: every package in this repository that `roomlerd`
//! links, followed through the dependency graph ([`Graph`]), so a crate is
//! scanned the day it is linked; in each, the files its library (and the
//! daemon's binary) compiles, through every `mod` the build keeps. Each file
//! is read as a Windows, a Linux and a macOS non-test build compiles it.
//!
//! **What counts as a read**, in a function, `const`, `static` or macro item
//! outside `appdirs`:
//! - on Windows, a body that reads the environment and names one of
//!   [`FOLDER_VARIABLES`]. It reads through `std::env`'s `var`, `var_os`,
//!   `vars` or `vars_os` (a renamed import too), one of [`OS_READS`], or a
//!   helper of the scanned code that reads one by a name its caller gives it.
//!   It names one in a literal, alone or inside a longer string
//!   (`%APPDATA%\…`), in a macro's arguments, or through a `const` or
//!   `static` that holds it;
//! - on macOS and Linux, a `directories` lookup ([`LOOKUPS`]).
//!
//! Every read outside `appdirs` has a row in [`KNOWN`] for each build that
//! flags it, of one of two kinds ([`Row`]): **right**, with the reason it is
//! right where it is, or **tracked**, when it is not right for an entry point
//! the row names, with the one issue that removes it. The gate prints how many
//! tracked rows there are and the issues they name, so the number is in every
//! run's log. A linked package that is not scanned is listed in
//! [`NOT_SCANNED`]. A row whose read is gone fails as stale, of either kind,
//! so no list can rot.
//!
//! What it does not see: a helper called with method syntax (`x.helper(..)`:
//! a parser does not know the receiver's type, and `cmd.env("APPDATA", x)`
//! sets a child's variable), a helper of a helper (which of its parameters
//! is the key is not followed), a helper outside the scanned code, and a
//! name put together at run time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, TokenStream, TokenTree};
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
    "HOMEDRIVE",
    "HOMEPATH",
    "TEMP",
    "TMP",
];

/// The names that are words of prose and of file names too (`x.tmp`).
const SHORT: &[&str] = &["TEMP", "TMP"];

/// `std::env`'s readers. `var`, `var_os` and `vars_os` count alone, as an
/// imported name; after a path, only after `env`.
const ENV_READS: &[&str] = &["var", "var_os", "vars", "vars_os"];

/// The other readers of a process's environment, whatever the path before
/// them.
const OS_READS: &[&str] = &[
    "GetEnvironmentVariableA",
    "GetEnvironmentVariableW",
    "ExpandEnvironmentStringsA",
    "ExpandEnvironmentStringsW",
    "GetEnvironmentStrings",
    "GetEnvironmentStringsA",
    "GetEnvironmentStringsW",
    "getenv",
    "_wgetenv",
    "secure_getenv",
];

/// `directories`' types, whose constructors take the folders from the
/// environment on macOS and Linux.
const LOOKUPS: &[&str] = &["ProjectDirs", "BaseDirs", "UserDirs"];

/// The module that reads them.
const READER: &str = "crates/agent-core/src/appdirs.rs";

/// What a row of [`KNOWN`] says of its read.
#[derive(Clone, Copy, Debug)]
enum Row {
    /// Right where it is, for this reason.
    Right(&'static str),
    /// Not right for the entry point the text names, until the issue
    /// removes it. The text states the platform, the account that runs the
    /// entry point and the folder the read returns there.
    Tracked(u32, &'static str),
}

/// A read outside [`READER`] on one build: `(file, function, build, row)`.
type KnownRow = (&'static str, &'static str, Build, Row);

/// Reads outside [`READER`], one row for each build that flags one.
const KNOWN: &[KnownRow] = &[
    (
        "crates/agent-core/src/files_dir.rs",
        "from_env",
        Build::Windows,
        Row::Right(
            "Windows: the received-files denylist's roots, in the process that \
             checks a path: not a folder a step writes, and on the service account \
             the values are the system's own once `appdirs::pin_system_folders` ran",
        ),
    ),
    (
        "crates/agent-core/src/companion_autostart.rs",
        "program_files_dirs",
        Build::Windows,
        Row::Right(
            "Windows: how the desktop companion was installed, read by the \
             companion as the person: not a folder a step writes",
        ),
    ),
    (
        "agents/roomler-cli/src/update.rs",
        "sudo_user_config_path",
        Build::Linux,
        Row::Right(
            "Linux: `sudo roomler self-update` runs as root and re-bases the path \
             onto the home the account database gives `SUDO_USER`, so it names \
             that person's config; `HOME` only names the path's tail",
        ),
    ),
    (
        "agents/roomler-cli/src/update.rs",
        "sudo_user_config_path",
        Build::Macos,
        Row::Right(
            "macOS: `sudo roomler self-update` runs as root and re-bases the path \
             onto the home the account database gives `SUDO_USER`, so it names \
             that person's config; `HOME` only names the path's tail",
        ),
    ),
    (
        "agents/roomlerd/src/files.rs",
        "private_staging_root",
        Build::Linux,
        Row::Right(
            "Linux: root (the system service, `sudo`) takes `/var/lib/roomler/staging` \
             or the seat person's home from the account database first; only a \
             daemon that is not root, the person's own, reads its own \
             `XDG_STATE_HOME` or `HOME`",
        ),
    ),
    (
        "agents/roomlerd/src/files.rs",
        "process_home",
        Build::Linux,
        Row::Right(
            "Linux: reached only by a process that is not root, which names its \
             own home; root, `sudo roomlerd run` too, takes its home from the \
             account database first (`account_home` in `writer_context`)",
        ),
    ),
    (
        "agents/roomlerd/src/files.rs",
        "own_home_and_downloads",
        Build::Linux,
        Row::Right(
            "Linux: read only by a daemon that is not root, the person's own, so \
             it names the person's home and Downloads; root takes its home from \
             the account database first (`account_home`)",
        ),
    ),
    (
        "agents/roomlerd/src/recording/folder.rs",
        "default_choice",
        Build::Linux,
        Row::Right(
            "Linux: resolved only by a recorder that runs as the person (the \
             daemon itself when it is the person, or the root daemon's child, \
             given the person's `HOME` from the account database), so it names \
             the person's folders",
        ),
    ),
    (
        "agents/roomlerd/src/recording/folder.rs",
        "default_choice",
        Build::Macos,
        Row::Right(
            "macOS: resolved only by a recorder that runs as the daemon's own \
             account, the person (a root daemon launches no recorder), so it \
             names the person's folders",
        ),
    ),
    // FR-147: on a Mac, a root step of ours does nothing inside a person's
    // home.
    (
        "agents/roomlerd/src/files.rs",
        "process_home",
        Build::Macos,
        Row::Tracked(
            3834,
            "macOS: `sudo roomlerd run` runs as root and keeps the caller's `HOME`, \
             so the daemon's active home is the caller's, not root's `/var/root`",
        ),
    ),
    (
        "agents/roomlerd/src/files.rs",
        "default_download_dir",
        Build::Macos,
        Row::Tracked(
            3834,
            "macOS: `sudo roomlerd run` runs as root and keeps the caller's `HOME`, \
             so this names the caller's `~/Downloads`, not root's",
        ),
    ),
    (
        "agents/roomlerd/src/instance_lock.rs",
        "unix_lock_path",
        Build::Macos,
        Row::Tracked(
            3834,
            "macOS: `sudo roomlerd run` runs as root and keeps the caller's `HOME`, \
             so its lock is in the caller's `~/Library/Caches`, not root's",
        ),
    ),
    (
        "agents/roomlerd/src/service.rs",
        "plist_path",
        Build::Macos,
        Row::Tracked(
            3834,
            "macOS: `sudo roomlerd service install` or `uninstall` runs as root and \
             keeps the caller's `HOME`, so this names the caller's \
             `~/Library/LaunchAgents`, not root's",
        ),
    ),
    (
        "agents/roomler-cli/src/config.rs",
        "default_config_path",
        Build::Macos,
        Row::Tracked(
            3834,
            "macOS: `sudo roomler enroll`, `forward` or `socks5` without `--config` \
             runs as root and keeps the caller's `HOME`, so this names the caller's \
             tunnel config, not root's",
        ),
    ),
    // Linux: a root command run through a `sudo` that keeps `HOME`.
    (
        "agents/roomlerd/src/service.rs",
        "user_unit_path_for",
        Build::Linux,
        Row::Tracked(
            3866,
            "Linux: where `sudo` keeps the caller's `HOME`, `sudo roomlerd service \
             install`, `start` or `stop` (root, no packaged unit) names the caller's \
             `systemd/user` folder here",
        ),
    ),
    (
        "agents/roomler-cli/src/config.rs",
        "default_config_path",
        Build::Linux,
        Row::Tracked(
            3866,
            "Linux: where `sudo` keeps the caller's `HOME`, `sudo roomler enroll`, \
             `forward` or `socks5` (root, no `--config`) names the caller's tunnel \
             config here",
        ),
    ),
    (
        "agents/roomlerd/src/instance_lock.rs",
        "unix_lock_path",
        Build::Linux,
        Row::Tracked(
            3866,
            "Linux: where `sudo` keeps the caller's `HOME`, `sudo roomlerd run` \
             (root) takes its lock in the caller's folders here",
        ),
    ),
];

/// A build the gate reads the sources as: a non-test build for one OS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Build {
    Windows,
    Linux,
    Macos,
}

const BUILDS: [Build; 3] = [Build::Windows, Build::Linux, Build::Macos];

impl Build {
    fn os(self) -> &'static str {
        match self {
            Build::Windows => "windows",
            Build::Linux => "linux",
            Build::Macos => "macos",
        }
    }

    fn family(self) -> &'static str {
        if self == Build::Windows {
            "windows"
        } else {
            "unix"
        }
    }
}

/// Whether a `cfg` holds on a build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holds {
    Yes,
    No,
    Maybe,
}

fn holds(meta: &syn::Meta, build: Build) -> Holds {
    let when = |yes: bool| if yes { Holds::Yes } else { Holds::No };
    match meta {
        syn::Meta::Path(path) if path.is_ident("test") => Holds::No,
        syn::Meta::Path(path) if path.is_ident("windows") || path.is_ident("unix") => {
            when(path.is_ident(build.family()))
        }
        syn::Meta::Path(_) => Holds::Maybe,
        syn::Meta::NameValue(pair) => {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(value),
                ..
            }) = &pair.value
            else {
                return Holds::Maybe;
            };
            let value = value.value();
            if pair.path.is_ident("target_os") {
                when(value == build.os())
            } else if pair.path.is_ident("target_family") {
                when(value == build.family())
            } else if pair.path.is_ident("target_vendor") && value == "apple" {
                when(build == Build::Macos)
            } else {
                Holds::Maybe
            }
        }
        syn::Meta::List(list) => {
            let nested: Vec<Holds> = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .map(|p| p.iter().map(|m| holds(m, build)).collect())
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

/// Is the thing these attributes sit on compiled into `build`? What the gate
/// cannot tell (a feature) is: looking at too much is the safe side.
fn compiled(attrs: &[syn::Attribute], build: Build) -> bool {
    attrs.iter().all(|attr| {
        !attr.path().is_ident("cfg")
            || attr
                .parse_args::<syn::Meta>()
                .map_or(true, |meta| holds(&meta, build) != Holds::No)
    })
}

fn segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// What a file's `use` items name: a reader of [`ENV_READS`] imported by
/// name, any reader renamed, `std::env` renamed, a type of [`LOOKUPS`]
/// renamed.
#[derive(Default)]
struct Imports {
    reads: BTreeSet<String>,
    env: BTreeSet<String>,
    lookups: BTreeSet<String>,
}

impl Imports {
    fn of(file: &syn::File) -> Self {
        struct Uses(Imports);
        impl<'ast> Visit<'ast> for Uses {
            fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
                self.0.tree(&i.tree, &mut Vec::new());
            }
        }
        let mut uses = Uses(Imports::default());
        uses.visit_file(file);
        uses.0
    }

    fn tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        let from_env = prefix.last().is_some_and(|m| m == "env");
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.tree(&p.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Group(g) => g.items.iter().for_each(|t| self.tree(t, prefix)),
            syn::UseTree::Name(n) => {
                let name = n.ident.to_string();
                if from_env && ENV_READS.contains(&name.as_str()) {
                    self.reads.insert(name);
                }
            }
            syn::UseTree::Rename(r) => {
                let (name, alias) = (r.ident.to_string(), r.rename.to_string());
                if name == "env" {
                    self.env.insert(alias);
                } else if OS_READS.contains(&name.as_str())
                    || from_env && ENV_READS.contains(&name.as_str())
                {
                    self.reads.insert(alias);
                } else if LOOKUPS.contains(&name.as_str()) {
                    self.lookups.insert(alias);
                }
            }
            syn::UseTree::Glob(_) => {}
        }
    }

    /// Whether a path names a reader of the environment.
    fn reads(&self, path: &[String]) -> bool {
        match path {
            [] => false,
            [.., last] if OS_READS.contains(&last.as_str()) => true,
            [one] => {
                matches!(one.as_str(), "var" | "var_os" | "vars_os") || self.reads.contains(one)
            }
            [.., module, last] => {
                ENV_READS.contains(&last.as_str()) && (module == "env" || self.env.contains(module))
            }
        }
    }

    /// The `directories` lookup a path makes: a type of [`LOOKUPS`] and what
    /// follows it (`BaseDirs::new`, `directories::ProjectDirs::from`).
    fn lookup(&self, path: &[String]) -> Option<String> {
        path.windows(2)
            .find(|w| LOOKUPS.contains(&w[0].as_str()) || self.lookups.contains(&w[0]))
            .map(|w| w.join("::"))
    }
}

/// What the scan knows across files, found to a fixed point.
#[derive(Default, PartialEq)]
struct Known {
    /// Functions and `macro_rules!` of the scanned code that read the
    /// environment by a name their caller gives them.
    helpers: BTreeSet<String>,
    /// `const`s and `static`s that hold a folder variable's name.
    names: BTreeMap<String, BTreeSet<String>>,
}

/// What one item's body does.
struct Body<'a> {
    build: Build,
    imports: &'a Imports,
    known: &'a Known,
    /// The item's parameters: a function's, or a `macro_rules!`'s `$name`s.
    params: &'a BTreeSet<String>,
    /// It reads the environment.
    reads: bool,
    /// It reads by a name its caller gives it: a reader's key that is one of
    /// its parameters, or a reader passed as a value or a walk of the whole
    /// environment in an item that takes any.
    keyed: bool,
    /// The folder variables it names.
    names: BTreeSet<String>,
    /// The `directories` lookups it makes.
    lookups: BTreeSet<String>,
}

impl Body<'_> {
    /// A path, called with `keyed` true when its first argument carries one
    /// of the item's parameters, or used as a value with `keyed` true when
    /// the item takes any.
    fn path(&mut self, path: &[String], keyed: bool) {
        let Some(last) = path.last() else { return };
        let reader = self.imports.reads(path);
        if reader || self.known.helpers.contains(last) {
            self.reads = true;
            // A helper is one that reads itself: a parameter that a helper
            // passes on may be any of its arguments, not the key.
            self.keyed |= keyed && reader;
        }
        if let Some(lookup) = self.imports.lookup(path) {
            self.lookups.insert(lookup);
        }
        if let Some(names) = self.known.names.get(last) {
            self.names.extend(names.iter().cloned());
        }
    }

    /// Every folder variable a literal names, alone or inside a longer
    /// string, as a word of its own in any case; one of [`SHORT`] inside a
    /// longer string only as `%TEMP%` or `$env:TEMP`.
    fn text(&mut self, text: &str) {
        let text = text.to_ascii_lowercase();
        for name in FOLDER_VARIABLES {
            let word = name.to_ascii_lowercase();
            if SHORT.contains(name) {
                if text == word
                    || text.contains(&format!("%{word}%"))
                    || text.contains(&format!("$env:{word}"))
                {
                    self.names.insert((*name).to_owned());
                }
                continue;
            }
            let alone = |at: usize| {
                let before = text[..at].chars().next_back();
                let after = text[at + word.len()..].chars().next();
                ![before, after]
                    .into_iter()
                    .flatten()
                    .any(|c| c.is_alphanumeric() || c == '_')
            };
            if text.match_indices(&word).any(|(at, _)| alone(at)) {
                self.names.insert((*name).to_owned());
            }
        }
    }

    fn literal(&mut self, lit: &syn::Lit) {
        match lit {
            syn::Lit::Str(s) => self.text(&s.value()),
            syn::Lit::ByteStr(s) => self.text(&String::from_utf8_lossy(&s.value())),
            syn::Lit::CStr(s) => self.text(&s.value().to_string_lossy()),
            _ => {}
        }
    }

    /// A macro's tokens, which the parser leaves unparsed: every literal, and
    /// every path with the call that follows it. A method (`.name`) is
    /// skipped, as it is outside a macro.
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let mut at = 0;
        while at < tokens.len() {
            match &tokens[at] {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Literal(literal) => self.literal(&syn::Lit::new(literal.clone())),
                TokenTree::Punct(_) => {}
                TokenTree::Ident(_) => {
                    let method = at > 0
                        && matches!(&tokens[at - 1], TokenTree::Punct(p) if p.as_char() == '.');
                    let mut path = Vec::new();
                    while let Some(TokenTree::Ident(ident)) = tokens.get(at) {
                        path.push(ident.to_string());
                        let colons = matches!(
                            (tokens.get(at + 1), tokens.get(at + 2)),
                            (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
                                if a.as_char() == ':' && b.as_char() == ':'
                        );
                        if !colons || !matches!(tokens.get(at + 3), Some(TokenTree::Ident(_))) {
                            break;
                        }
                        at += 3;
                    }
                    if !method {
                        let keyed = match tokens.get(at + 1) {
                            Some(TokenTree::Group(g))
                                if g.delimiter() == Delimiter::Parenthesis =>
                            {
                                self.leads_with_param(g.stream())
                            }
                            _ => !self.params.is_empty(),
                        };
                        self.path(&path, keyed);
                    }
                }
            }
            at += 1;
        }
    }

    /// Whether a call's tokens start with one of the item's parameters
    /// (`name`, `&name`, a `macro_rules!`'s `$name`).
    fn leads_with_param(&self, tokens: TokenStream) -> bool {
        let mut tokens = tokens
            .into_iter()
            .skip_while(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == '&'));
        match tokens.next() {
            Some(TokenTree::Punct(p)) if p.as_char() == '$' => true,
            Some(TokenTree::Ident(ident)) => self.params.contains(&ident.to_string()),
            _ => false,
        }
    }

    /// Whether an expression carries one of the item's parameters.
    fn mentions_param(&self, expr: &syn::Expr) -> bool {
        struct Mentions<'p>(&'p BTreeSet<String>, bool);
        impl<'ast> Visit<'ast> for Mentions<'_> {
            fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
                self.1 |= i
                    .path
                    .get_ident()
                    .is_some_and(|p| self.0.contains(&p.to_string()));
            }
        }
        let mut mentions = Mentions(self.params, false);
        mentions.visit_expr(expr);
        mentions.1
    }
}

impl<'ast> Visit<'ast> for Body<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {
        // A doc comment is not code, and a `cfg` is read where it gates.
    }
    fn visit_expr_path(&mut self, i: &'ast syn::ExprPath) {
        self.path(&segments(&i.path), !self.params.is_empty());
    }
    fn visit_expr_call(&mut self, i: &'ast syn::ExprCall) {
        if !compiled(&i.attrs, self.build) {
            return;
        }
        match &*i.func {
            syn::Expr::Path(func) => {
                // A walk of the whole environment takes no key: it counts as
                // keyed in an item that takes any.
                let keyed = match i.args.first() {
                    Some(first) => self.mentions_param(first),
                    None => !self.params.is_empty(),
                };
                self.path(&segments(&func.path), keyed);
            }
            func => self.visit_expr(func),
        }
        for arg in &i.args {
            self.visit_expr(arg);
        }
    }
    fn visit_macro(&mut self, i: &'ast syn::Macro) {
        if segments(&i.path)
            .last()
            .is_some_and(|m| self.known.helpers.contains(m))
        {
            self.reads = true;
            self.keyed |= self.leads_with_param(i.tokens.clone());
        }
        self.tokens(i.tokens.clone());
    }
    fn visit_lit(&mut self, i: &'ast syn::Lit) {
        self.literal(i);
    }
    fn visit_local(&mut self, i: &'ast syn::Local) {
        if compiled(&i.attrs, self.build) {
            visit::visit_local(self, i);
        }
    }
    fn visit_expr_block(&mut self, i: &'ast syn::ExprBlock) {
        if compiled(&i.attrs, self.build) {
            visit::visit_expr_block(self, i);
        }
    }
    fn visit_stmt_macro(&mut self, i: &'ast syn::StmtMacro) {
        if compiled(&i.attrs, self.build) {
            visit::visit_stmt_macro(self, i);
        }
    }
    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        if compiled(&i.attrs, self.build) {
            visit::visit_expr_method_call(self, i);
        }
    }
    fn visit_arm(&mut self, i: &'ast syn::Arm) {
        if compiled(&i.attrs, self.build) {
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
        if compiled(attrs, self.build) {
            visit::visit_item(self, i);
        }
    }
}

/// One item of a file, as a build compiles it.
struct Item {
    name: String,
    /// A function or `macro_rules!` that takes arguments.
    takes: bool,
    /// A `const` or `static`.
    holds: bool,
    reads: bool,
    keyed: bool,
    names: BTreeSet<String>,
    lookups: BTreeSet<String>,
}

/// Every compiled function, `const`, `static` and macro item of a file.
struct Items<'a> {
    build: Build,
    imports: &'a Imports,
    known: &'a Known,
    found: Vec<Item>,
}

impl Items<'_> {
    fn item(
        &mut self,
        name: String,
        params: BTreeSet<String>,
        holds: bool,
        visit_body: impl FnOnce(&mut Body),
    ) {
        let takes = !params.is_empty();
        let mut body = Body {
            build: self.build,
            imports: self.imports,
            known: self.known,
            params: &params,
            reads: false,
            keyed: false,
            names: BTreeSet::new(),
            lookups: BTreeSet::new(),
        };
        visit_body(&mut body);
        self.found.push(Item {
            name,
            takes,
            holds,
            reads: body.reads,
            keyed: body.keyed,
            names: body.names,
            lookups: body.lookups,
        });
    }
}

/// The names a function's parameters bind (`self` is not one).
fn params(sig: &syn::Signature) -> BTreeSet<String> {
    struct Names(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Names {
        fn visit_pat_ident(&mut self, i: &'ast syn::PatIdent) {
            self.0.insert(i.ident.to_string());
            visit::visit_pat_ident(self, i);
        }
    }
    let mut names = Names(BTreeSet::new());
    for arg in &sig.inputs {
        if let syn::FnArg::Typed(typed) = arg {
            names.visit_pat(&typed.pat);
        }
    }
    names.0
}

impl<'ast> Visit<'ast> for Items<'_> {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if compiled(&i.attrs, self.build) {
            self.item(i.sig.ident.to_string(), params(&i.sig), false, |b| {
                b.visit_block(&i.block)
            });
        }
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        if compiled(&i.attrs, self.build) {
            self.item(i.sig.ident.to_string(), params(&i.sig), false, |b| {
                b.visit_block(&i.block)
            });
        }
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        if compiled(&i.attrs, self.build)
            && let Some(block) = &i.default
        {
            self.item(i.sig.ident.to_string(), params(&i.sig), false, |b| {
                b.visit_block(block)
            });
        }
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        if compiled(&i.attrs, self.build) {
            self.item(i.ident.to_string(), BTreeSet::new(), true, |b| {
                b.visit_expr(&i.expr)
            });
        }
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        if compiled(&i.attrs, self.build) {
            self.item(i.ident.to_string(), BTreeSet::new(), true, |b| {
                b.visit_expr(&i.expr)
            });
        }
    }
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        if compiled(&i.attrs, self.build) {
            self.item(i.ident.to_string(), BTreeSet::new(), true, |b| {
                b.visit_expr(&i.expr)
            });
        }
    }
    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        // `macro_rules! name`, or a macro at item level (`thread_local!`).
        if compiled(&i.attrs, self.build) {
            let rules = i.mac.path.is_ident("macro_rules");
            let name = match &i.ident {
                Some(ident) if rules => ident.to_string(),
                _ => format!("{}!", segments(&i.mac.path).join("::")),
            };
            // A `macro_rules!` takes its `$name`s, which no identifier equals.
            let params = BTreeSet::from_iter(rules.then(|| "$".to_owned()));
            self.item(name, params, false, |b| b.tokens(i.mac.tokens.clone()));
        }
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        if compiled(&i.attrs, self.build) {
            visit::visit_item_mod(self, i);
        }
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if compiled(&i.attrs, self.build) {
            visit::visit_item_impl(self, i);
        }
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        if compiled(&i.attrs, self.build) {
            visit::visit_item_trait(self, i);
        }
    }
}

/// One function, `const`, `static` or macro item the gate flags, with the
/// folder variables it names (on Windows) or the lookups it makes (on macOS
/// and Linux).
#[derive(Debug, PartialEq, Eq)]
struct Read {
    function: String,
    names: Vec<String>,
}

/// Every item of `files` outside [`READER`] that `build` flags:
/// `(file, read)`. A helper or a name-holding `const` in one file counts in
/// every other.
fn flagged(files: &[(String, &syn::File)], build: Build) -> Vec<(String, Read)> {
    let files: Vec<(&String, &syn::File, Imports)> = files
        .iter()
        .filter(|(_, file)| compiled(&file.attrs, build))
        .map(|(name, file)| (name, *file, Imports::of(file)))
        .collect();
    let mut known = Known::default();
    let items = loop {
        let items: Vec<Vec<Item>> = files
            .iter()
            .map(|(_, file, imports)| {
                let mut items = Items {
                    build,
                    imports,
                    known: &known,
                    found: Vec::new(),
                };
                items.visit_file(file);
                items.found
            })
            .collect();
        let mut next = Known::default();
        for item in items.iter().flatten() {
            if item.takes && item.keyed {
                next.helpers.insert(item.name.clone());
            }
            if item.holds && !item.reads && !item.names.is_empty() {
                next.names
                    .entry(item.name.clone())
                    .or_default()
                    .extend(item.names.iter().cloned());
            }
        }
        if next == known {
            break items;
        }
        known = next;
    };
    let mut found = Vec::new();
    for ((file, _, _), items) in files.iter().zip(items) {
        if file.as_str() == READER {
            continue;
        }
        for item in items {
            let names = match build {
                Build::Windows if item.reads => item.names,
                Build::Windows => BTreeSet::new(),
                Build::Linux | Build::Macos => item.lookups,
            };
            if !names.is_empty() {
                found.push((
                    (*file).clone(),
                    Read {
                        function: item.name,
                        names: names.into_iter().collect(),
                    },
                ));
            }
        }
    }
    found
}

/// Every item of one source that any build flags.
fn reads_in(file: &str, source: &str) -> Vec<Read> {
    let parsed = syn::parse_file(source).unwrap_or_else(|e| panic!("{file}: {e}"));
    let mut reads: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for build in BUILDS {
        for (_, read) in flagged(&[(file.to_owned(), &parsed)], build) {
            reads.entry(read.function).or_default().extend(read.names);
        }
    }
    reads
        .into_iter()
        .map(|(function, names)| Read {
            function,
            names: names.into_iter().collect(),
        })
        .collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the repository root")
}

/// Source files, each parsed once.
#[derive(Default)]
struct Sources(BTreeMap<PathBuf, syn::File>);

impl Sources {
    /// `file` and every file of a `mod` that `build` keeps below it; `dir`
    /// is the folder that `file`'s own `mod`s live in.
    fn walk(&mut self, file: &Path, dir: &Path, build: Build, out: &mut BTreeSet<PathBuf>) {
        let parsed = self.0.entry(file.to_path_buf()).or_insert_with(|| {
            let source =
                std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            syn::parse_file(&source).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
        });
        if !compiled(&parsed.attrs, build) || !out.insert(file.to_path_buf()) {
            return;
        }
        let mut children = Vec::new();
        mods(&parsed.items, dir, build, &mut children);
        for (child, child_dir) in children {
            self.walk(&child, &child_dir, build, out);
        }
    }
}

/// The file of every `mod name;` among `items` that `build` keeps, with the
/// folder its own `mod`s live in.
fn mods(items: &[syn::Item], dir: &Path, build: Build, out: &mut Vec<(PathBuf, PathBuf)>) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        if !compiled(&m.attrs, build) {
            continue;
        }
        let name = m.ident.to_string();
        assert!(
            !m.attrs.iter().any(|a| a.path().is_ident("path")),
            "mod {name} in {}: the gate does not follow #[path]",
            dir.display()
        );
        let sub = dir.join(&name);
        match &m.content {
            Some((_, items)) => mods(items, &sub, build, out),
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

/// The files the gate reads in `build`, from the root of every package it
/// scans: `(path from the repository root with /, file)`.
fn files<'s>(
    sources: &'s mut Sources,
    root: &Path,
    graph: &Graph,
    build: Build,
) -> Vec<(String, &'s syn::File)> {
    let mut paths = BTreeSet::new();
    for package in scanned(graph) {
        let member = &graph.members[&package];
        // A dependency links its library; the daemon is its binary too.
        let bins = if package == ROOT {
            &member.bin[..]
        } else {
            &[]
        };
        for start in member.lib.iter().chain(bins) {
            let dir = start.parent().expect("a crate root's folder");
            sources.walk(start, dir, build, &mut paths);
        }
    }
    let sources: &'s Sources = sources;
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(root)
                .unwrap_or_else(|_| panic!("{} is outside the repository", path.display()))
                .to_string_lossy()
                .replace('\\', "/");
            (name, &sources.0[&path])
        })
        .collect()
}

/// What `rows` leave unmatched in `reads` (`(build, file, read)`): a read
/// with no row of either kind for its build, a row, of either kind, whose
/// read is gone (stale), and a row whose text does not start with its
/// build's platform. One line each.
fn unmatched(reads: &[(Build, String, Read)], rows: &[KnownRow]) -> Vec<String> {
    let mut lines = Vec::new();
    for (build, file, read) in reads {
        if !rows
            .iter()
            .any(|(f, function, b, _)| f == file && *function == read.function && b == build)
        {
            lines.push(format!(
                "{build:?}: {file}: {} names {:?}",
                read.function, read.names
            ));
        }
    }
    for (file, function, build, row) in rows {
        let (Row::Right(text) | Row::Tracked(_, text)) = row;
        if !text
            .to_ascii_lowercase()
            .starts_with(&format!("{}: ", build.os()))
        {
            lines.push(format!(
                "a row whose text does not start with its platform: {build:?}: {file}: {function}"
            ));
        }
        if !reads
            .iter()
            .any(|(b, f, read)| b == build && f == file && read.function == *function)
        {
            let kind = match row {
                Row::Right(_) => "right".to_owned(),
                Row::Tracked(issue, _) => format!("tracked #{issue}"),
            };
            lines.push(format!("a stale {kind} row: {build:?}: {file}: {function}"));
        }
    }
    lines
}

/// The line the gate prints on every run: how many tracked rows `rows` hold
/// and the issues they name, with each one's count.
fn tracked(rows: &[KnownRow]) -> String {
    let mut issues: BTreeMap<u32, usize> = BTreeMap::new();
    for (_, _, _, row) in rows {
        if let Row::Tracked(issue, _) = row {
            *issues.entry(*issue).or_default() += 1;
        }
    }
    let named: Vec<String> = issues
        .iter()
        .map(|(issue, count)| format!("#{issue} ({count})"))
        .collect();
    format!(
        "folder gate: {} tracked rows, naming {}",
        issues.values().sum::<usize>(),
        if named.is_empty() {
            "no issue".to_owned()
        } else {
            named.join(", ")
        }
    )
}

#[test]
fn only_appdirs_reads_a_folder_variable() {
    let root = repo_root();
    let graph = Graph::read(&root);
    let mut sources = Sources::default();
    let mut reads = Vec::new();
    for build in BUILDS {
        let files = files(&mut sources, &root, &graph, build);
        assert!(
            files.iter().any(|(f, _)| f == READER),
            "the {build:?} scan reaches {READER}"
        );
        reads.extend(
            flagged(&files, build)
                .into_iter()
                .map(|(file, read)| (build, file, read)),
        );
    }
    // Written to the stream itself: the harness keeps back what a passing
    // test prints with `println!`, and this line belongs in every log.
    {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "{}", tracked(KNOWN));
    }
    let lines = unmatched(&reads, KNOWN);
    assert!(
        lines.is_empty(),
        "a folder read outside appdirs; take the folder from `appdirs` (the machine's \
         from the system, the service account's profile from the profile list), or give \
         the read a row in KNOWN for its build: right, with why, or tracked, with the \
         issue that removes it. A row whose read is gone is stale:\n{}",
        lines.join("\n")
    );
}

/// The row cases of [`the_gate_sees_what_it_is_for`].
fn rows_hold_their_reads() {
    let read = |build, function: &str| {
        (
            build,
            "a.rs".to_owned(),
            Read {
                function: function.to_owned(),
                names: vec!["BaseDirs::new".to_owned()],
            },
        )
    };
    const ROWS: &[KnownRow] = &[
        ("a.rs", "right", Build::Linux, Row::Right("Linux: why")),
        (
            "a.rs",
            "tracked",
            Build::Macos,
            Row::Tracked(1, "macOS: where"),
        ),
    ];
    // A tracked row with its read present passes, as a right row does.
    let both = [read(Build::Linux, "right"), read(Build::Macos, "tracked")];
    assert_eq!(unmatched(&both, ROWS), Vec::<String>::new());
    // A tracked row whose read is gone fails as stale.
    assert_eq!(
        unmatched(&[read(Build::Linux, "right")], ROWS),
        ["a stale tracked #1 row: Macos: a.rs: tracked"]
    );
    // A read with no row of either kind fails: a row for another build is
    // not its row.
    assert_eq!(
        unmatched(
            &[
                read(Build::Linux, "right"),
                read(Build::Macos, "tracked"),
                read(Build::Linux, "tracked"),
            ],
            ROWS
        ),
        [r#"Linux: a.rs: tracked names ["BaseDirs::new"]"#]
    );
    // A row's text states its platform: a row copied to another build fails.
    const COPIED: &[KnownRow] = &[("a.rs", "right", Build::Macos, Row::Right("Linux: why"))];
    assert_eq!(
        unmatched(&[read(Build::Macos, "right")], COPIED),
        ["a row whose text does not start with its platform: Macos: a.rs: right"]
    );
    // The line every run prints.
    assert_eq!(tracked(ROWS), "folder gate: 1 tracked rows, naming #1 (1)");
    assert_eq!(
        tracked(&ROWS[..1]),
        "folder gate: 0 tracked rows, naming no issue"
    );
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
    rows_hold_their_reads();
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
                // The form `repo_root` has (`\\?\C:\…` on Windows), so that a
                // file's name is its path from the root.
                let src = src.canonicalize().unwrap_or(src);
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

/// The packages the gate scans: every workspace member [`ROOT`] links, but
/// a [`NOT_SCANNED`] row.
fn scanned(graph: &Graph) -> BTreeSet<String> {
    graph
        .linked
        .iter()
        .filter(|p| graph.members.contains_key(*p) && !NOT_SCANNED.iter().any(|(n, _)| n == p))
        .cloned()
        .collect()
}

#[test]
fn the_scan_is_what_roomlerd_links() {
    let root = repo_root();
    let graph = Graph::read(&root);
    let scanned = scanned(&graph);
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
