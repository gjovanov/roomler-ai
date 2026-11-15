#!/usr/bin/env bash
# Tests for scripts/job-steps-ended.sh, the installer smoke's re-run decision
# (#3810), on saved and made-up GitHub API replies; for
# scripts/job-steps-ended-rerun.sh, which the workflow installer-smoke-rerun
# runs, against a stand-in for gh; and for the job names and the shape of
# that workflow. Run by ci.yml's job-steps-ended job; needs bash, jq and the
# usual awk, sed and od, nothing else.
#
#   bash scripts/job-steps-ended-test.sh          run every case
#   bash scripts/job-steps-ended-test.sh --write  rebuild the made-up fixtures
#
# The fixtures (scripts/job-steps-ended-fixtures/):
#   jobs-<run>.json  a run's jobs as GitHub's API listed them, projected to the
#                    fields the script reads (URLs dropped):
#                    jq '{total_count, jobs: [.jobs[] | {id, name, run_id, run_attempt,
#                        status, conclusion, started_at, completed_at, runner_name, steps}]}'
#   ann-<job>.json   a job's check-run annotations, projected:
#                    jq '[.[] | {annotation_level, title, message}]'
#   madeup-*.json    made from jobs-43420118889.json by madeup() below. This
#                    test rebuilds them and fails when the files differ.
#
# Every case checks: the verdict line; the line that says whether the job is
# re-run; that every step which reported an end other than success or skipped,
# every step which reported no end, and every annotation is named; that the
# output has its seven lines; and that no word of a cause appears in the
# script's own words (the lines minus the quoted names, and minus the
# annotations, which are GitHub's text).
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
S=$here/job-steps-ended.sh
F=$here/job-steps-ended-fixtures
WIN='per-User MSI install + uninstall (windows-latest)'
# The in-run retry job of the first shape of #3810: its name starts with WIN's.
RETRY='per-User MSI install + uninstall (windows-latest), once more after a lost runner'
CAUSE='lost|not the product|by itself|never ran|none of the|runner was|the product|the test'
CLEAN='def clean: tostring | explode | map(if . < 32 or . == 127 then 32 else . end) | implode;'

# The made-up replies, each from the real failed job of run 43420118889 (every
# step reported an end; its step 28 failed and the job went on):
#   s1a  the input a decision gets wrong when a step that ended badly and steps
#        that ran after it decide: "Build agent" cancelled, the steps after it
#        skipped, the evidence step success, the step after it still
#        in_progress, the rest pending, no "Complete job", and the annotation
#        of a runner shutdown. Not every step reported an end: incomplete.
#   s1b  the same annotation, and every step reported an end: complete. The
#        annotation does not decide.
#   s2   step 28 failed by itself, then the evidence step stayed in_progress
#        and the rest pending, cancelled at 45:00: incomplete, and the failed
#        step is named as well as the open ones.
#   timeout  "Build agent" cancelled, the steps after it skipped, the evidence
#        step in_progress past the 5 minutes' grace, the rest pending,
#        cancelled at 45:00 with the job-timeout annotation: incomplete.
#   no-steps  the job ended "failure" with no step listed: incomplete.
#   control-chars  a pull request's workflow file names its steps: a step that
#        failed is named "y\r##[error]z", the evidence step, left in_progress,
#        "x\n::error title=x::y", the runner and the annotation carry control
#        characters too. Incomplete, printed on its seven lines with no line
#        that starts a workflow command.
madeup() { # OUTDIR
  local o=$1 base=$F/jobs-43420118889.json
  local pre='
    def win: .jobs[] | select(.name == $w);
    def num($n): [.steps[] | select(.name == $n) | .number][0] | if . == null then error("no step \($n)") else . end;
    def open: .status = "in_progress" | .conclusion = null | .completed_at = null;
    def pending: .status = "pending" | .conclusion = null | .started_at = null | .completed_at = null;
    def at45: .completed_at = ((.started_at | fromdateiso8601) + 2700 | todateiso8601);'
  jq --arg w "$WIN" "$pre"'
    (win) |= (num("Build agent (release, minimal feature set)") as $b | num("Save the evidence of a failure (#3754)") as $e
      | num("Upload MSI logs on failure") as $l | .conclusion = "cancelled"
      | .steps |= (map(if .number < $b then .
          elif .number == $b then .conclusion = "cancelled"
          elif .number < $e then .conclusion = "skipped"
          elif .number == $e then .conclusion = "success"
          elif .number == $l then open
          else pending end) | map(select(.name != "Complete job"))))' "$base" >"$o/madeup-s1a-shutdown-step-open.json" || return 1
  jq --arg w "$WIN" "$pre"'
    (win) |= (num("Build agent (release, minimal feature set)") as $b | num("Save the evidence of a failure (#3754)") as $e
      | .conclusion = "cancelled"
      | .steps |= map(if .number < $b then .
          elif .number == $b then .conclusion = "cancelled"
          elif .number < $e then .conclusion = "skipped"
          else . end))' "$base" >"$o/madeup-s1b-shutdown-every-step-ended.json" || return 1
  jq --arg w "$WIN" "$pre"'
    (win) |= (num("Save the evidence of a failure (#3754)") as $e | .conclusion = "cancelled" | at45
      | .steps |= (map(if .number < $e then . elif .number == $e then open else pending end)
          | map(select(.name != "Complete job"))))' "$base" >"$o/madeup-s2-failed-step-then-evidence-open.json" || return 1
  jq --arg w "$WIN" "$pre"'
    (win) |= (num("Build agent (release, minimal feature set)") as $b | num("Save the evidence of a failure (#3754)") as $e
      | .conclusion = "cancelled" | at45
      | .steps |= (map(if .number < $b then .
          elif .number == $b then .conclusion = "cancelled"
          elif .number < $e then .conclusion = "skipped"
          elif .number == $e then open
          else pending end) | map(select(.name != "Complete job"))))' "$base" >"$o/madeup-timeout-evidence-past-grace.json" || return 1
  jq --arg w "$WIN" "$pre"'(win) |= (.conclusion = "failure" | .steps = [])' "$base" >"$o/madeup-no-steps.json" || return 1
  jq --arg w "$WIN" "$pre"'
    (win) |= (num("Build agent (release, minimal feature set)") as $b | num("Save the evidence of a failure (#3754)") as $e
      | .conclusion = "cancelled" | .runner_name = "GitHub Actions 9\n::warning::r"
      | .steps |= (map(if .number == $b then .conclusion = "failure" | .name = "y\r##[error]z"
          elif .number == $e then open | .name = "x\n::error title=x::y"
          elif .number > $e then pending else . end) | map(select(.name != "Complete job"))))' "$base" >"$o/madeup-control-chars.json" || return 1
  printf '%s\n' '[{"annotation_level": "failure", "title": "", "message": "The runner has received a shutdown signal. This can happen when the runner service is stopped, or a manually started runner is canceled."}]' >"$o/ann-madeup-shutdown.json"
  printf '%s\n' '[]' >"$o/ann-madeup-none.json"
  printf '%s\n' '[{"annotation_level": "failure\n::error::l", "title": "", "message": "m\n::error::a \u001b[31m b\u0000c\u007f"}]' >"$o/ann-madeup-control-chars.json"
}

pass=0 fail=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fail=$((fail + 1)); echo "FAIL $1: $2"; [ -z "${3-}" ] || printf '%s\n' "$3" | sed 's/^/     | /'; }

if [ "${1-}" = --write ]; then
  madeup "$F" || { echo 'could not write the made-up fixtures' >&2; exit 1; }
  echo "wrote the made-up fixtures in $F"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# 1. The made-up fixtures are what madeup() makes.
mkdir "$tmp/madeup"
if madeup "$tmp/madeup"; then
  for g in "$tmp"/madeup/*.json; do
    n=$(basename "$g")
    if [ ! -f "$F/$n" ]; then
      bad "made-up fixture $n" 'missing: run this script with --write'
    elif [ "$(jq -S -c . "$g")" != "$(jq -S -c . "$F/$n")" ]; then
      bad "made-up fixture $n" 'differs from what madeup() makes: run this script with --write'
    else
      ok "made-up fixture $n is what madeup() makes"
    fi
  done
else
  bad 'made-up fixtures' 'madeup() failed'
fi

# 2. The cases.
case_() { # label jobs-file job-name attempt annotations-file expected-verdict
  local label=$1 jf=$2 name=$3 att=$4 af=$5 want=$6 out rc job line rerun own s
  out=$(bash "$S" --job "$name" --attempt "$att" --jobs "$jf" --annotations "$af" 2>&1)
  rc=$?
  local why=''
  [ "$rc" -eq 0 ] || why+="exit $rc, not 0; "
  [ "$(head -n 1 <<<"$out")" = "verdict=$want" ] || why+="line 1 is not verdict=$want; "
  [ "$(wc -l <<<"$out")" -eq 7 ] || why+="$(wc -l <<<"$out") lines, not 7; "
  if [ "$want" = complete ]; then
    rerun='Every step reported an end, so this job is not re-run.'
  elif [ "$att" = 1 ]; then
    rerun='Not every step reported an end, so the workflow installer-smoke-rerun asks GitHub to re-run this job once, as attempt 2 of this run, unless one of its guards refuses. If no attempt 2 follows, its run among the installer-smoke-rerun runs in the Actions list says why.'
  else
    rerun="Not every step reported an end. This is attempt $att; only attempt 1 is re-run, so this job is not re-run again."
  fi
  [ "$(tail -n 1 <<<"$out")" = "$rerun" ] || why+='the last line does not say whether it is re-run as expected; '
  job=$(jq -c --arg n "$name" '[.jobs[] | select(.name == $n)] | last' "$jf")
  grep -qF -- "The job \"$name\" (job $(jq -r .id <<<"$job"), attempt $att) ended \"$(jq -r .conclusion <<<"$job")\"" <<<"$out" ||
    why+='the job line does not name the job, its id, the attempt and its conclusion; '
  # A name is expected as printed: each control character a space.
  while IFS= read -r s; do
    grep -qF -- "$s" <<<"$out" || why+="not named: $s; "
  done < <(jq -r "$CLEAN"' .steps[] | select(.status == "completed" and .conclusion != "success" and .conclusion != "skipped")
                  | "step \(.number) \"\(.name | clean)\" \(.conclusion)"' <<<"$job"
           jq -r "$CLEAN"' .steps[] | select(.status != "completed") | "step \(.number) \"\(.name | clean)\" (\(.status)"' <<<"$job"
           jq -r "$CLEAN"' .[] | .message | clean | gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" ")' "$af")
  if [ "$(jq '.steps | length' <<<"$job")" -eq 0 ]; then
    grep -qxF 'GitHub lists no step of this job.' <<<"$out" || why+='no line says that no step is listed; '
  else
    s=$(jq -r '(.steps | length) as $n | "GitHub lists \($n) step\(if $n == 1 then "" else "s" end): \([.steps[] | select(.status == "completed")] | length) reported an end"' <<<"$job")
    grep -qF -- "$s" <<<"$out" || why+="no line says: $s; "
  fi
  if [ "$(jq length "$af")" -eq 0 ]; then
    grep -qxF "GitHub's annotations on the job: none." <<<"$out" || why+='no line says there is no annotation; '
  fi
  own=$(grep -v "^GitHub's annotations on the job:" <<<"$out" | sed 's/"[^"]*"//g')
  line=$(grep -m 1 -inE "$CAUSE" <<<"$own") && why+="a cause in its own words: $line; "
  if [ -z "$why" ]; then ok "$label: $want"; else bad "$label" "$why" "$out"; fi
}

case_ 'real run 43599817590, a step failed and the job went on' \
  "$F/jobs-43599817590.json" "$WIN" 1 "$F/ann-132461970043.json" complete
case_ 'real run 43600992327, a step left in_progress' \
  "$F/jobs-43600992327.json" "$WIN" 1 "$F/ann-132465489201.json" incomplete
case_ 'real run 43600992327, its second Windows job (every step ended) read as attempt 2' \
  "$F/jobs-43600992327.json" "$RETRY" 2 "$F/ann-132465489201.json" complete
case_ 'real run 43606061432, a step left in_progress' \
  "$F/jobs-43606061432.json" "$WIN" 1 "$F/ann-132480840733.json" incomplete
case_ 'real run 43606061432, its second Windows job (a step left in_progress) read as attempt 2' \
  "$F/jobs-43606061432.json" "$RETRY" 2 "$F/ann-132493134775.json" incomplete
case_ 'real run 43275582546, a step left in_progress (no simulation)' \
  "$F/jobs-43275582546.json" "$WIN" 1 "$F/ann-131389720573.json" incomplete
case_ 'real run 43420118889, a step timed out and the job went on' \
  "$F/jobs-43420118889.json" "$WIN" 1 "$F/ann-131867721518.json" complete
case_ 'made-up s1a, a shutdown annotation and a step left in_progress' \
  "$F/madeup-s1a-shutdown-step-open.json" "$WIN" 1 "$F/ann-madeup-shutdown.json" incomplete
case_ 'made-up s1b, a shutdown annotation and every step ended' \
  "$F/madeup-s1b-shutdown-every-step-ended.json" "$WIN" 1 "$F/ann-madeup-shutdown.json" complete
case_ 'made-up s2, a step failed, then the evidence step left in_progress' \
  "$F/madeup-s2-failed-step-then-evidence-open.json" "$WIN" 1 "$F/ann-132465489201.json" incomplete
case_ 'made-up timeout, the evidence step in_progress past the grace' \
  "$F/madeup-timeout-evidence-past-grace.json" "$WIN" 1 "$F/ann-132465489201.json" incomplete
case_ 'made-up no steps' \
  "$F/madeup-no-steps.json" "$WIN" 1 "$F/ann-madeup-none.json" incomplete
case_ 'made-up no steps, read as attempt 2' \
  "$F/madeup-no-steps.json" "$WIN" 2 "$F/ann-madeup-none.json" incomplete
case_ 'made-up control characters in the names' \
  "$F/madeup-control-chars.json" "$WIN" 1 "$F/ann-madeup-control-chars.json" incomplete

# A name with a newline does not start a line of its own: still seven lines,
# none of them a workflow command, no control character left but the line
# ends, and each name printed with a space for each control character.
out=$(bash "$S" --job "$WIN" --attempt 1 --jobs "$F/madeup-control-chars.json" --annotations "$F/ann-madeup-control-chars.json" 2>&1)
why=''
[ "$(wc -l <<<"$out")" -eq 7 ] || why+="$(wc -l <<<"$out") lines, not 7; "
line=$(grep -m 1 -nE '^(::|##\[)' <<<"$out") && why+="a line starts a command: $line; "
[ "$(tr -d '\n' <<<"$out" | LC_ALL=C tr -d -c '\000-\037\177' | wc -c)" -eq 0 ] || why+='a control character is left; '
for s in '"y ##[error]z" failure' '"x ::error title=x::y" (in_progress' 'runner "GitHub Actions 9 ::warning::r"' \
  'failure ::error::l "m ::error::a [31m b c"'; do
  grep -qF -- "$s" <<<"$out" || why+="not printed: $s; "
done
if [ -z "$why" ]; then ok 'control characters in the names are spaces, and no line starts a command'; else bad 'control characters' "$why" "$out"; fi

# 3. No verdict, and usage.
expect() { # label expected-exit expected-substring command...
  local label=$1 want=$2 sub=$3 out rc
  shift 3
  out=$("$@" 2>&1)
  rc=$?
  if [ "$rc" -ne "$want" ]; then
    bad "$label" "exit $rc, not $want" "$out"
  elif grep -q '^verdict=' <<<"$out"; then
    bad "$label" 'printed a verdict' "$out"
  elif ! grep -qF -- "$sub" <<<"$out"; then
    bad "$label" "does not say: $sub" "$out"
  else
    ok "$label: exit $want"
  fi
}
A=$F/jobs-43606061432.json
expect 'a name that only starts the job names is not a match' 2 'No job named "per-User MSI install + uninstall" is listed' \
  bash "$S" --job 'per-User MSI install + uninstall' --attempt 1 --jobs "$A"
jq --arg w "$WIN" '(.jobs[] | select(.name == $w) | .status) = "in_progress"' "$A" >"$tmp/running.json"
expect 'a job that is not completed has no verdict' 2 'reads "in_progress", not completed' \
  bash "$S" --job "$WIN" --attempt 1 --jobs "$tmp/running.json"
printf 'not json\n' >"$tmp/garbage.json"
expect 'a reply that is not the jobs list has no verdict' 2 'could not be read' \
  bash "$S" --job "$WIN" --attempt 1 --jobs "$tmp/garbage.json"
expect 'no --job' 64 'usage:' bash "$S" --attempt 1 --jobs "$A"
expect 'attempt 0' 64 'usage:' bash "$S" --job "$WIN" --attempt 0 --jobs "$A"
expect 'a file and a run at once' 64 'usage:' bash "$S" --job "$WIN" --attempt 1 --jobs "$A" --run 1
expect 'a value missing' 64 'needs a value' bash "$S" --job "$WIN" --attempt

out=$(bash "$S" --job "$WIN" --attempt 1 --jobs "$A" --annotations "$tmp/absent.json" 2>/dev/null)
grep -qxF "GitHub's annotations on the job: could not be read." <<<"$out" && grep -qx 'verdict=incomplete' <<<"$out" &&
  ok 'annotations that cannot be read are said so, and the verdict stands' ||
  bad 'annotations that cannot be read' 'not said, or no verdict' "$out"
out=$(bash "$S" --job "$WIN" --attempt 1 --jobs "$A" 2>/dev/null)
grep -qxF "GitHub's annotations on the job: not read." <<<"$out" && grep -qx 'verdict=incomplete' <<<"$out" &&
  ok 'no annotations given are said so, and the verdict stands' ||
  bad 'no annotations given' 'not said, or no verdict' "$out"

# 4. The live form, against a stand-in for gh that serves the fixtures and logs
# what it was asked. The first read of the jobs finds the job still running.
mkdir "$tmp/bin"
cat >"$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$GH_LOG"
[ "$1" = api ] || exit 1
case "$2" in
  repos/o/r/actions/runs/7/attempts/1/jobs\?per_page=100)
    n=$(grep -c attempts "$GH_LOG")
    if [ "$n" -le "${GH_RUNNING_READS:-0}" ]; then cat "$GH_RUNNING"; else cat "$GH_JOBS"; fi ;;
  repos/o/r/check-runs/132480840733/annotations\?per_page=100) [ -n "${GH_ANN:-}" ] && cat "$GH_ANN" ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$tmp/bin/gh"
live() { # GH_* settings... -- script args
  env PATH="$tmp/bin:$PATH" GH_LOG="$tmp/gh.log" GH_JOBS="$A" GH_RUNNING="$tmp/running.json" \
    JOB_STEPS_ENDED_WAIT=0 "$@"
}
: >"$tmp/gh.log"
out=$(live GH_RUNNING_READS=2 GH_ANN="$F/ann-132480840733.json" bash "$S" --job "$WIN" --attempt 1 --repo o/r --run 7 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && grep -qx 'verdict=incomplete' <<<"$out" && [ "$(grep -c attempts "$tmp/gh.log")" -eq 3 ] &&
  grep -qF 'exceeded the maximum execution time of 40m0s' <<<"$out" &&
  [ "$out" = "$(bash "$S" --job "$WIN" --attempt 1 --jobs "$A" --annotations "$F/ann-132480840733.json")" ]; then
  ok 'live: reads again while the job runs, then prints what the saved reply prints'
else
  bad 'live: read again while the job runs' "exit $rc" "$out"$'\n'"$(cat "$tmp/gh.log")"
fi
: >"$tmp/gh.log"
out=$(live bash "$S" --job "$WIN" --attempt 1 --repo o/r --run 7 2>&1)
grep -qx 'verdict=incomplete' <<<"$out" && grep -qxF "GitHub's annotations on the job: could not be read." <<<"$out" &&
  ok 'live: annotations that cannot be read are said so, and the verdict stands' ||
  bad 'live: annotations that cannot be read' 'not said, or no verdict' "$out"
: >"$tmp/gh.log"
expect 'live: a job still running after every read has no verdict' 2 'not completed, after 3 read(s)' \
  live GH_RUNNING_READS=9 JOB_STEPS_ENDED_READS=3 bash "$S" --job "$WIN" --attempt 1 --repo o/r --run 7
expect 'live: jobs that cannot be read have no verdict' 2 'The jobs of attempt 2 of run 7 could not be read' \
  live JOB_STEPS_ENDED_READS=2 bash "$S" --job "$WIN" --attempt 2 --repo o/r --run 7

# 5. The two workflow files. The listener matches the smoke's Windows job and
# check job by name, by equality: a rename that forgets one copy must fail
# here, not end the re-runs in silence. And the listener keeps its shape: no
# permission but `actions: write` and `contents: read` on its one job, the
# default branch's scripts checked out with no credential kept, its one `run:`
# the re-run script, no expression of the run in it, and one listener at a
# time for one smoke run.
SMOKE_YML=$here/../.github/workflows/installer-smoke.yml
RERUN_YML=$here/../.github/workflows/installer-smoke-rerun.yml
job_name_of() { # FILE KEY: the `name:` of the job KEY, its first line at four spaces
  awk -v k="  $2:" '{ sub(/\r$/, "") } $0 == k { j = 1; next } j && /^  [A-Za-z0-9_-]+:/ { exit }
    j && sub(/^    name: /, "") { print; exit }' "$1"
}
env_of() { awk -v k="$2" '{ sub(/\r$/, "") } $0 ~ "^ +" k ": " { sub("^ +" k ": ", ""); print }' "$1"; } # FILE KEY
yval() { # a name as these files write it: plain, or in double quotes with no escape; else refused
  case "$1" in
    \"*\") local v=${1#\"}; v=${v%\"}; [[ $v != *[\"\\]* ]] && printf '%s' "$v" ;;
    \'* | *' #'* | '') return 1 ;;
    *) printf '%s' "$1" ;;
  esac
}
why=''
win_name=$(job_name_of "$SMOKE_YML" windows-msi-install-uninstall)
chk_name=$(job_name_of "$SMOKE_YML" windows-steps-check)
in_run=$(env_of "$SMOKE_YML" SMOKE_JOB)
L_WIN=$(env_of "$RERUN_YML" SMOKE_JOB)
L_CHK=$(env_of "$RERUN_YML" CHECK_JOB)
for v in win_name chk_name in_run L_WIN L_CHK; do
  [ -n "${!v}" ] && [ "$(wc -l <<<"${!v}")" -eq 1 ] && s=$(yval "${!v}") && printf -v "$v" '%s' "$s" ||
    why+="$v: not one plain or simply quoted name; "
done
[ "$L_WIN" = "$win_name" ] || why+="the listener's SMOKE_JOB \"$L_WIN\" is not the Windows job's name \"$win_name\"; "
[ "$L_CHK" = "$chk_name" ] || why+="the listener's CHECK_JOB \"$L_CHK\" is not the check job's name \"$chk_name\"; "
[ "$in_run" = "$win_name" ] || why+="the check job's SMOKE_JOB \"$in_run\" is not the Windows job's name \"$win_name\"; "
if [ -z "$why" ]; then ok "the listener's job names equal installer-smoke.yml's"; else bad 'job names' "$why"; fi

L=$(tr -d '\r' <"$RERUN_YML")
IF="    if: \${{ github.event.workflow_run.run_attempt == 1 && github.event.workflow_run.conclusion != 'success' }}"
GROUP='  group: installer-smoke-rerun-${{ github.event.workflow_run.id }}'
why=''
for s in 'permissions: {}' 'concurrency:' "$GROUP" '  cancel-in-progress: false' "$IF" '    permissions:' \
  '      actions: write' '      contents: read' '          persist-credentials: false' \
  '            scripts/job-steps-ended.sh' '            scripts/job-steps-ended-rerun.sh' \
  '        run: bash scripts/job-steps-ended-rerun.sh'; do
  [ "$(grep -cxF -- "$s" <<<"$L")" -eq 1 ] || why+="not exactly once: $s; "
done
[ "$(grep -cE '^ *(- )?(run|ref|repository|token|ssh-key):' <<<"$L")" -eq 1 ] || why+='a second run:, or a ref:, repository:, token: or ssh-key:; '
[ "$(grep -cE '^ +[a-z-]+: (read|write|none)$' <<<"$L")" -eq 2 ] || why+='a permission besides actions: write and contents: read; '
s=$(grep -F '${{' <<<"$L" | grep -vxF -e "$IF" -e "$GROUP" -e '          GH_TOKEN: ${{ github.token }}' \
  -e '          RUN_ID: ${{ github.event.workflow_run.id }}')
[ -z "$s" ] || why+="an expression elsewhere: $s; "
if [ -z "$why" ]; then ok 'the listener keeps its shape'; else bad 'the listener' "$why"; fi

# 6. scripts/job-steps-ended-rerun.sh, with the listener's job names, against a
# stand-in for gh that serves a run, its attempt 1 jobs and the newer runs,
# and logs what it was asked, in order.
RR=$here/job-steps-ended-rerun.sh
D=$tmp/rr
mkdir "$D" "$tmp/bin2"
cat >"$tmp/bin2/gh" <<'EOF'
#!/usr/bin/env bash
# As gh does, `--jq EXPR` filters the reply; the log keeps the request alone.
jqx='' args=()
while [ $# -gt 0 ]; do
  if [ "$1" = --jq ]; then jqx=$2; shift 2; else args+=("$1"); shift; fi
done
set -- "${args[@]}"
printf '%s\n' "$*" >>"$GH_LOG"
# A cancel or the job's timeout, as the runner sends it: SIGTERM to the script.
[ -z "${GH_TERM_PARENT-}" ] || { kill -TERM "$PPID"; exit 1; }
reply() { if [ -n "$jqx" ]; then jq -r "$jqx" "$1"; else cat "$1"; fi; }
[ "$1" = api ] || exit 1
shift
case "$*" in
  'repos/o/r/actions/runs/7') [ -z "${GH_FAIL_RUN-}" ] && reply "$GH_RUN" ;;
  'repos/o/r/actions/runs/7/attempts/1/jobs?per_page=100') [ -z "${GH_FAIL_JOBS-}" ] && reply "$GH_JOBS" ;;
  '-X GET repos/o/r/actions/workflows/'*'/runs '*) [ -z "${GH_FAIL_RUNS-}" ] && reply "$GH_RUNS" ;;
  '-X POST repos/o/r/actions/'*) [ -z "${GH_FAIL_POST-}" ] && echo '{}' ;;
  *) exit 1 ;;
esac
EOF
chmod +x "$tmp/bin2/gh"
printf '%s\n' '{"id": 7, "run_attempt": 1, "head_branch": "lab/BRANCH-MARK", "event": "pull_request",
  "head_repository": {"full_name": "fork/r"}, "workflow_id": 99, "created_at": "2026-11-14T20:51:18Z",
  "status": "completed", "conclusion": "failure", "display_title": "TITLE-MARK"}' >"$D/run.json"
jq '.run_attempt = 2' "$D/run.json" >"$D/run-attempt-2.json"
jq '.head_repository = null' "$D/run.json" >"$D/run-no-head-repo.json"
# Attempt 1's jobs: the macOS job FIRST, the Windows job under the listener's
# name, and the check job with a given conclusion (or none).
jobs_from() { # FIXTURE CHECK-CONCLUSION|none [no = no Windows job]
  jq --arg w "$WIN" --arg lw "$L_WIN" --arg lc "$L_CHK" --arg c "$2" --arg win "${3:-yes}" '
    .jobs = [.jobs[] | select(.name | startswith("macOS"))]
      + (if $win == "yes" then [.jobs[] | select(.name == $w) | .name = $lw] else [] end)
      + (if $c == "none" then [] else [{id: 900, name: $lc, status: "completed", conclusion: $c, steps: []}] end)' "$1"
}
jobs_from "$F/jobs-43606061432.json" failure >"$D/jobs.json"
jobs_from "$F/jobs-43599817590.json" failure >"$D/jobs-complete.json"
jobs_from "$F/madeup-control-chars.json" failure >"$D/jobs-control-chars.json"
for c in skipped cancelled none; do jobs_from "$F/jobs-43606061432.json" "$c" >"$D/jobs-check-$c.json"; done
jobs_from "$F/jobs-43606061432.json" failure no >"$D/jobs-no-windows.json"
jq --arg lw "$L_WIN" '(.jobs[] | select(.name == $lw) | .status) = "in_progress"' "$D/jobs.json" >"$D/jobs-running.json"
jq --arg lw "$L_WIN" '(.jobs[] | select(.name == $lw) | .id) = "9/../8"' "$D/jobs.json" >"$D/jobs-id-not-a-number.json"
printf 'not json\n' >"$D/jobs-garbage.json"
: >"$D/runs-empty.json"
printf '%s\n' '{"total_count": 3, "jobs": "not a list"}' >"$D/jobs-not-a-list.json"
printf '%s\n' '{"total_count": 0, "workflow_runs": []}' >"$D/runs-none.json"
printf '%s\n' '{"total_count": 1, "workflow_runs": [{"id": 8, "head_repository": {"full_name": "fork/r"},
  "created_at": "2026-11-14T20:59:00Z"}]}' >"$D/runs-newer.json"
jq '.workflow_runs[0].head_repository.full_name = "other/r"' "$D/runs-newer.json" >"$D/runs-newer-other-repo.json"
winjob() { # JOBS-FILE: the Windows job's id, its steps that reported no end, its steps, its conclusion
  jq -r --arg n "$L_WIN" '[.jobs[] | select(.name == $n)] | last
    | "\(.id) \([.steps[] | select(.status != "completed")] | length) \(.steps | length) \(.conclusion)"' "$1"
}
read -r WID WOPEN WSTEPS WCONCL < <(winjob "$D/jobs.json")
read -r CID _ _ _ < <(winjob "$D/jobs-complete.json")
read -r XID XOPEN XSTEPS XCONCL < <(winjob "$D/jobs-control-chars.json")
G1='api repos/o/r/actions/runs/7'
JOBS='api repos/o/r/actions/runs/7/attempts/1/jobs?per_page=100'
G3='api -X GET repos/o/r/actions/workflows/99/runs -f branch=lab/BRANCH-MARK -f event=pull_request -f created=>2026-11-14T20:51:18Z -f per_page=100'
lines() { printf '%s\n' "$@"; }
NO='Did not re-run run 7:'

rr() { # SETTINGS...: runs the script; sets out, rc, sum, log
  : >"$D/gh.log"
  : >"$D/summary.md"
  out=$(env PATH="$tmp/bin2:$PATH" GH_LOG="$D/gh.log" GITHUB_REPOSITORY=o/r RUN_ID=7 SMOKE_JOB="$L_WIN" CHECK_JOB="$L_CHK" \
    GITHUB_STEP_SUMMARY="$D/summary.md" JOB_STEPS_ENDED_WAIT=0 GH_RUN="$D/run.json" GH_JOBS="$D/jobs.json" \
    GH_RUNS="$D/runs-none.json" "$@" bash "$RR" 2>&1)
  rc=$?
  sum=$(cat "$D/summary.md")
  log=$(cat "$D/gh.log")
}
# Every case: exit 0; ONE summary line, exactly as given; gh asked exactly
# what is given, in that order; a warning only for an error; the first line
# stops workflow commands with a random token, which one line resumes, after
# which come only the warning and the summary line; no other line starts a
# command; the decision's verdict printed only when it was read; nothing of
# the run in the summary but numbers and status words; no annotations read.
rcase() { # LABEL DECIDED(0|1) WARNINGS(0|1) SUMMARY GH-LOG SETTINGS...
  local label=$1 decided=$2 warns=$3 text=$4 want=$5 why='' t n
  shift 5
  rr "$@"
  [ "$rc" -eq 0 ] || why+="exit $rc, not 0; "
  [ "$(wc -l <"$D/summary.md")" -eq 1 ] && [ "$sum" = "$text" ] || why+="the summary is not the one line: $text; "
  [ "$log" = "$want" ] || why+='gh was not asked exactly what was expected; '
  n=$(grep -c '^::warning ' <<<"$out")
  [ "$n" -eq "$warns" ] || why+="$n warnings, not $warns; "
  t=$(head -n 1 <<<"$out")
  t=${t#::stop-commands::}
  if [[ $t =~ ^[0-9a-f]{32}$ ]] && [ "$(grep -cxF "::$t::" <<<"$out")" -eq 1 ]; then
    [ "$(sed -n "/^::$t::\$/,\$p" <<<"$out" | tail -n +2)" = "$(if [ "$warns" -eq 1 ]; then
      echo "::warning title=installer-smoke-rerun::$text"; fi; echo "$text")" ] ||
      why+='after the block: not exactly the warning and the summary line; '
    n=$(grep -E '^(::|##\[)' <<<"$out" | grep -cvxF -e "::stop-commands::$t" -e "::$t::" -e "::warning title=installer-smoke-rerun::$text")
    [ "$n" -eq 0 ] || why+="$n other lines start a command; "
  else
    why+='line 1 does not stop commands with a random token that one line resumes; '
  fi
  [ "$(grep -c '^verdict=' <<<"$out")" -eq "$decided" ] || why+="the verdict printed $(grep -c '^verdict=' <<<"$out") times, not $decided; "
  ! grep -qE 'BRANCH-MARK|TITLE-MARK|::|##\[' <<<"$sum" || why+='the summary carries a branch, a title or a command; '
  ! grep -q "^GitHub's annotations" <<<"$out" || why+='an annotations line is printed; '
  if [ -z "$why" ]; then ok "re-run: $label"; else bad "re-run: $label" "$why" "$out"$'\n'"-- gh was asked:"$'\n'"$log"; fi
}
rcase 'G1, the run reads attempt 2: refused' 0 0 "$NO it reads attempt 2 now, so it has been re-run already." \
  "$G1" GH_RUN="$D/run-attempt-2.json"
rcase 'G2, the check job skipped: refused' 0 0 \
  "$NO its check job reads \"skipped\" in attempt 1: the run was cancelled, or its Windows job succeeded or was skipped." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-check-skipped.json"
rcase 'G2, the check job cancelled: refused' 0 0 \
  "$NO its check job reads \"cancelled\" in attempt 1: the run was cancelled, or its Windows job succeeded or was skipped." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-check-cancelled.json"
rcase 'G2, the check job not listed: refused' 0 0 "$NO its check job is not listed in attempt 1." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-check-none.json"
rcase 'the Windows job not listed: refused before the decision' 0 0 "$NO its Windows job is not listed in attempt 1." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-no-windows.json"
rcase 'a jobs reply jq cannot read: refused, with a warning' 0 1 "$NO its jobs could not be read." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-garbage.json"
rcase 'a jobs reply whose jobs are not a list: refused, with a warning' 0 1 "$NO its jobs could not be read." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-not-a-list.json"
rcase 'the decision reads complete: no G3, no re-run' 1 0 "$NO every step of its Windows job (job $CID) reported an end." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-complete.json"
rcase 'the Windows job not completed: no verdict, a warning' 0 1 \
  "$NO its Windows job could not be read as completed (the decision script exited 2)." \
  "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-running.json"
rcase 'G3, a newer run of the same branch, event and head repository: refused' 1 0 \
  "$NO a newer run of the same branch, event and head repository exists (run 8)." \
  "$(lines "$G1" "$JOBS" "$G3")" GH_RUNS="$D/runs-newer.json"
rcase 'G3, a newer run of another head repository: re-run' 1 0 \
  "Asked GitHub to re-run job $WID of run 7 once, and GitHub accepted: the job ended \"$WCONCL\" and $WOPEN of its $WSTEPS steps reported no end." \
  "$(lines "$G1" "$JOBS" "$G3" "api -X POST repos/o/r/actions/jobs/$WID/rerun")" GH_RUNS="$D/runs-newer-other-repo.json"
rcase 'every guard passes and the decision reads incomplete: ONE re-run, of the Windows job, after G3' 1 0 \
  "Asked GitHub to re-run job $WID of run 7 once, and GitHub accepted: the job ended \"$WCONCL\" and $WOPEN of its $WSTEPS steps reported no end." \
  "$(lines "$G1" "$JOBS" "$G3" "api -X POST repos/o/r/actions/jobs/$WID/rerun")"
t1=$(head -n 1 <<<"$out")
rcase 'control characters in the names: the same, and no line starts a command' 1 0 \
  "Asked GitHub to re-run job $XID of run 7 once, and GitHub accepted: the job ended \"$XCONCL\" and $XOPEN of its $XSTEPS steps reported no end." \
  "$(lines "$G1" "$JOBS" "$G3" "api -X POST repos/o/r/actions/jobs/$XID/rerun")" GH_JOBS="$D/jobs-control-chars.json"
[ "$t1" != "$(head -n 1 <<<"$out")" ] && ok 're-run: the token differs from run to run' || bad 're-run: the token' 'the same twice' "$t1"
rcase 'the run cannot be read: a warning' 0 1 "$NO the run could not be read." "$G1" GH_FAIL_RUN=1
rcase 'the jobs cannot be read: a warning' 0 1 "$NO its jobs could not be read." "$(lines "$G1" "$JOBS")" GH_FAIL_JOBS=1
rcase 'the newer runs cannot be read: a warning' 1 1 "$NO the newer runs of its branch could not be read." \
  "$(lines "$G1" "$JOBS" "$G3")" GH_FAIL_RUNS=1
rcase 'G3, an empty reply: refused, with a warning, no re-run' 1 1 "$NO the newer runs of its branch could not be read." \
  "$(lines "$G1" "$JOBS" "$G3")" GH_RUNS="$D/runs-empty.json"
rcase 'G3, a reply that is not JSON: refused, with a warning, no re-run' 1 1 \
  "$NO the newer runs of its branch could not be read." "$(lines "$G1" "$JOBS" "$G3")" GH_RUNS="$D/jobs-garbage.json"
rcase 'a run with no head repository: refused before G3, with a warning' 1 1 \
  "$NO its branch, event, head repository, workflow or creation time could not be read." \
  "$(lines "$G1" "$JOBS")" GH_RUN="$D/run-no-head-repo.json"
rcase 'a Windows job id that is not a number: refused before the decision, with a warning' 0 1 \
  "$NO its jobs could not be read." "$(lines "$G1" "$JOBS")" GH_JOBS="$D/jobs-id-not-a-number.json"
# A cancel or the job's timeout ends the script with SIGTERM: the EXIT trap
# still closes the block, so no later line of the step is swallowed.
rr GH_TERM_PARENT=1
t=$(head -n 1 <<<"$out")
t=${t#::stop-commands::}
[ "$rc" -ne 0 ] && [[ $t =~ ^[0-9a-f]{32}$ ]] && [ "$(tail -n 1 <<<"$out")" = "::$t::" ] &&
  [ "$(grep -cxF "::$t::" <<<"$out")" -eq 1 ] && [ -z "$sum" ] && [ "$log" = "$G1" ] &&
  ok 're-run: ended by SIGTERM: the EXIT trap closes the block' ||
  bad 're-run: ended by SIGTERM' "exit $rc, summary \"$sum\"" "$out"$'\n'"-- gh was asked:"$'\n'"$log"
rcase 'GitHub refuses the re-run: a warning' 1 1 "$NO GitHub refused the re-run of its Windows job (job $WID)." \
  "$(lines "$G1" "$JOBS" "$G3" "api -X POST repos/o/r/actions/jobs/$WID/rerun")" GH_FAIL_POST=1
rr RUN_ID='7;x'
[ "$rc" -eq 0 ] && [ -z "$log" ] && [ "$sum" = 'Did not re-run: no random token, or no repository, run id or job names this script can use.' ] &&
  [ "$(grep -c '^::warning ' <<<"$out")" -eq 1 ] && ok 're-run: no usable run id: a warning, nothing read' ||
  bad 're-run: no usable run id' "exit $rc" "$out"$'\n'"$log"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
