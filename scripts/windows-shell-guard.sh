#!/usr/bin/env bash
#
# windows-shell-guard.sh — every job of ci.yml that runs on a Windows runner declares bash
# as the default shell of its `run:` steps (#3352).
#
# WHY THIS EXISTS
#
#   On a Windows runner a `run:` step with no `shell:` runs in pwsh. GitHub's wrapper puts
#   `$ErrorActionPreference = 'stop'` in front of the script, which does not act on a native
#   command's exit code, and ends it with `exit $LASTEXITCODE`, so the LAST native command
#   decides the step. Six steps ran `cargo clippy … -D warnings` and then `cargo test`:
#   clippy printed its findings and the step passed with the tests. Each step had copied
#   the shape of the one before, so an exit check after one clippy line repairs one step and
#   the next step written that way starts again. The fix is the job's default instead: bash,
#   which GitHub runs with `-eo pipefail`, so the first failing command ends the step.
#
#   The default is set per job, not for the workflow: a workflow-wide bash would add
#   pipefail to every Linux step, and some of those were written without it. A step that
#   needs PowerShell names `shell: pwsh` itself and checks $LASTEXITCODE after each native
#   command. The release and installer workflows are PowerShell by design and follow that
#   second rule; they are not scanned here.
#
# The guard first proves it can see what it checks: a planted workflow must be refused
# exactly as expected, or it fails before it scans anything.
#
# Usage: bash scripts/windows-shell-guard.sh [workflow.yml ...]   (default: ci.yml)
# Needs yq (v4) and jq, both on GitHub's Ubuntu runners.
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in yq jq; do
  command -v "$tool" >/dev/null || { echo "::error::windows-shell-guard needs $tool"; exit 1; }
done

# One line per job that can run on Windows: its id, a tab, its default run shell. A job
# can run on Windows when its runs-on names windows, or names a matrix that does.
windows_jobs() {
  yq -o=json '.' "$1" | jq -r '
    (.jobs // {}) | to_entries[]
    | select(((.value["runs-on"] // "") | tostring | test("windows"; "i"))
          or (((.value["runs-on"] // "") | tostring | test("matrix"))
              and ((.value.strategy.matrix // {}) | tostring | test("windows"; "i"))))
    | "\(.key)\t\(.value.defaults.run.shell // "(none)")"'
}

# Self-test: two jobs pass (a bash default, a Linux job) and three are refused.
planted=$(mktemp --suffix=.yml)
trap 'rm -f "$planted"' EXIT
cat > "$planted" <<'YML'
jobs:
  bash-default:
    runs-on: windows-latest
    defaults:
      run:
        shell: bash
    steps: [{ run: "true" }]
  no-default:
    runs-on: windows-latest
    steps: [{ run: "true" }]
  pwsh-default:
    runs-on: [self-hosted, Windows]
    defaults: { run: { shell: pwsh } }
    steps: [{ run: "true" }]
  matrix-windows:
    runs-on: ${{ matrix.os }}
    strategy: { matrix: { os: [ubuntu-latest, windows-latest] } }
    steps: [{ run: "true" }]
  linux:
    runs-on: ubuntu-latest
    steps: [{ run: "true" }]
YML
refused=$(windows_jobs "$planted" | awk -F'\t' '$2 != "bash" { print $1 }' | sort | tr '\n' ' ')
if [ "$refused" != "matrix-windows no-default pwsh-default " ]; then
  echo "::error::windows-shell-guard self-test: expected to refuse 'matrix-windows no-default pwsh-default', refused '$refused'"
  exit 1
fi
echo "self-test: the planted workflow's three non-bash Windows jobs are refused"

bad=0
for wf in "${@:-.github/workflows/ci.yml}"; do
  jobs=$(windows_jobs "$wf")
  if [ -z "$jobs" ]; then
    echo "::error file=$wf::no job on a Windows runner found — if that is true now, drop this file from the guard"
    bad=1
    continue
  fi
  while IFS=$'\t' read -r job shell; do
    if [ "$shell" = bash ]; then
      echo "ok: $wf: $job defaults to bash"
    else
      echo "::error file=$wf::job '$job' runs on Windows and its default shell is $shell, not bash: add defaults.run.shell: bash to the job (#3352, scripts/windows-shell-guard.sh)"
      bad=1
    fi
  done <<< "$jobs"
done
exit "$bad"
