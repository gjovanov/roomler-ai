// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-2 — the services a checkout's manifests name, in the child,
//! as the root's account: compose files and Kubernetes workloads, each a name
//! and an image, through the one bounded YAML reader ([`super::yaml`]).
//!
//! | Bound | Value |
//! |---|---|
//! | depth below the checkout | 3 levels |
//! | files read | 200 a checkout, 256 KiB each |
//! | services | [`MAX_SERVICES`] a checkout |
//! | a Helm template | skipped: not YAML until rendered |

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

use roomler_ai_remote_control::knowhow::{MAX_PATH, MAX_SERVICES, MAX_TEXT, ManifestService};

use super::yaml::{self, Node};

/// How deep below a checkout manifests are looked for.
const MAX_DEPTH: usize = 3;
/// The most files read in one checkout.
const MAX_FILES: usize = 200;
/// The most bytes read of one file.
const MAX_FILE: u64 = 256 * 1024;
/// The kinds whose pods name images.
const WORKLOADS: &[&str] = &[
    "Deployment",
    "StatefulSet",
    "DaemonSet",
    "ReplicaSet",
    "Job",
    "CronJob",
    "Pod",
];

fn compose_name(name: &str) -> bool {
    matches!(
        name,
        "compose.yaml" | "compose.yml" | "docker-compose.yaml" | "docker-compose.yml"
    ) || (name.starts_with("docker-compose.")
        && (name.ends_with(".yml") || name.ends_with(".yaml")))
}

fn yaml_name(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".yml")
}

/// The YAML files under `checkout`, compose files first, sorted, bounded.
fn files(checkout: &Path, deadline: Instant) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(checkout.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if Instant::now() >= deadline || out.len() >= MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        names.sort();
        let mut below = Vec::new();
        for name in names {
            if super::walk::skipped(&name) {
                continue;
            }
            let path = dir.join(&name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() && depth + 1 < MAX_DEPTH {
                below.push(path);
            } else if meta.is_file() && yaml_name(&name.to_string_lossy()) {
                out.push(path);
                if out.len() >= MAX_FILES {
                    break;
                }
            }
        }
        stack.extend(below.into_iter().rev().map(|p| (p, depth + 1)));
    }
    out
}

/// The services `checkout`'s manifests name, each once.
pub(super) fn services(checkout: &Path, deadline: Instant) -> Vec<ManifestService> {
    let mut out: Vec<ManifestService> = Vec::new();
    for file in files(checkout, deadline) {
        if out.len() >= MAX_SERVICES || Instant::now() >= deadline {
            break;
        }
        let Some(text) = read_small(&file) else {
            continue;
        };
        // A Helm template is not YAML until it is rendered.
        if text.contains("{{") {
            continue;
        }
        let Ok(docs) = yaml::documents(&text, &[]) else {
            continue;
        };
        let rel = file
            .strip_prefix(checkout)
            .ok()
            .and_then(Path::to_str)
            .filter(|r| r.len() <= MAX_PATH)
            .map(str::to_string);
        let name = file.file_name().and_then(OsStr::to_str).unwrap_or_default();
        let found = if compose_name(name) {
            docs.iter().flat_map(compose).collect::<Vec<_>>()
        } else {
            docs.iter().flat_map(workload).collect()
        };
        for (service, image) in found {
            let s = ManifestService {
                name: service,
                image,
                file: rel.clone(),
            };
            if !out.iter().any(|o| o.name == s.name && o.image == s.image) {
                out.push(s);
                if out.len() >= MAX_SERVICES {
                    break;
                }
            }
        }
    }
    out
}

/// A compose file's services: each its name and image (none when it is
/// built).
fn compose(doc: &Node) -> Vec<(String, Option<String>)> {
    let Some(Node::Map(services)) = doc.get("services") else {
        return Vec::new();
    };
    services
        .iter()
        .filter(|(name, _)| fits(name))
        .map(|(name, svc)| {
            (
                name.clone(),
                svc.get("image")
                    .and_then(Node::text)
                    .filter(|i| fits(i))
                    .map(str::to_string),
            )
        })
        .collect()
}

/// A Kubernetes workload's containers: its name, or `<name>/<container>`
/// when its pod holds more than one, and each image.
fn workload(doc: &Node) -> Vec<(String, Option<String>)> {
    let Some(kind) = doc.get("kind").and_then(Node::text) else {
        return Vec::new();
    };
    if !WORKLOADS.contains(&kind) {
        return Vec::new();
    }
    let Some(name) = doc
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Node::text)
    else {
        return Vec::new();
    };
    let spec = doc.get("spec");
    let pod = match kind {
        "Pod" => spec,
        "CronJob" => spec
            .and_then(|s| s.get("jobTemplate"))
            .and_then(|j| j.get("spec"))
            .and_then(|s| s.get("template"))
            .and_then(|t| t.get("spec")),
        _ => spec
            .and_then(|s| s.get("template"))
            .and_then(|t| t.get("spec")),
    };
    let containers = pod
        .and_then(|p| p.get("containers"))
        .map(Node::items)
        .unwrap_or_default();
    let one = containers.len() == 1;
    containers
        .iter()
        .filter_map(|c| {
            let image = c
                .get("image")
                .and_then(Node::text)
                .filter(|i| fits(i))
                .map(str::to_string);
            let service = if one {
                name.to_string()
            } else {
                format!("{name}/{}", c.get("name").and_then(Node::text)?)
            };
            fits(&service).then_some((service, image))
        })
        .collect()
}

fn fits(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_TEXT
}

fn read_small(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_FILE {
        return None;
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn write(at: &Path, text: &str) {
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, text).unwrap();
    }

    fn found(dir: &Path) -> Vec<(String, Option<String>, Option<String>)> {
        services(dir, soon())
            .into_iter()
            .map(|s| (s.name, s.image, s.file))
            .collect()
    }

    /// A compose file's services, each with its image, or none when it is
    /// built.
    #[test]
    fn a_compose_file_names_its_services() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("compose.yaml"),
            "x-base: &base\n  restart: always\nservices:\n  web:\n    <<: *base\n    image: nginx:1.27\n  api:\n    build: .\n",
        );
        assert_eq!(
            found(tmp.path()),
            [
                (
                    "web".into(),
                    Some("nginx:1.27".into()),
                    Some("compose.yaml".into())
                ),
                ("api".into(), None, Some("compose.yaml".into())),
            ]
        );
    }

    /// A Kubernetes workload's containers: its own name for one, and
    /// `<name>/<container>` for several; what is not a workload is passed by.
    #[test]
    fn kubernetes_workloads_name_their_containers() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            &tmp.path().join("deploy").join("app.yaml"),
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\nspec:\n  template:\n    spec:\n      containers:\n      - name: api\n        image: ghcr.io/acme/api:1.2\n\
             ---\nkind: Service\nmetadata:\n  name: api\n\
             ---\nkind: CronJob\nmetadata:\n  name: nightly\nspec:\n  jobTemplate:\n    spec:\n      template:\n        spec:\n          containers:\n          - name: main\n            image: ghcr.io/acme/job:3\n          - name: sidecar\n            image: busybox\n",
        );
        assert_eq!(
            found(tmp.path()),
            [
                (
                    "api".into(),
                    Some("ghcr.io/acme/api:1.2".into()),
                    Some("deploy/app.yaml".into())
                ),
                (
                    "nightly/main".into(),
                    Some("ghcr.io/acme/job:3".into()),
                    Some("deploy/app.yaml".into())
                ),
                (
                    "nightly/sidecar".into(),
                    Some("busybox".into()),
                    Some("deploy/app.yaml".into())
                ),
            ]
        );
    }

    /// A Helm template is not YAML until rendered; a file that is not YAML,
    /// one too deep, and one in a dependency tree are passed by.
    #[test]
    fn what_cannot_be_read_as_a_manifest_is_passed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let deployment =
            "kind: Pod\nmetadata: {name: p}\nspec: {containers: [{name: c, image: i}]}\n";
        write(
            &tmp.path().join("chart").join("templates").join("pod.yaml"),
            &format!("{deployment}# {{{{ .Values.tag }}}}\n"),
        );
        write(&tmp.path().join("broken.yaml"), "kind: [unclosed\n");
        write(
            &tmp.path().join("a").join("b").join("c").join("deep.yaml"),
            deployment,
        );
        write(
            &tmp.path().join("node_modules").join("x").join("pod.yaml"),
            deployment,
        );
        assert!(found(tmp.path()).is_empty());

        write(&tmp.path().join("a").join("b").join("pod.yaml"), deployment);
        assert_eq!(
            found(tmp.path()),
            [("p".into(), Some("i".into()), Some("a/b/pod.yaml".into()))]
        );
    }
}
