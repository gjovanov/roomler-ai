#!/usr/bin/env bash
# Tests for scripts/job-steps-ended.sh, the installer smoke's re-run decision
# (#3810), on saved and made-up GitHub API replies. Run by ci.yml's
# job-steps-ended job; needs bash and jq, nothing else.
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
  printf '%s\n' '[{"annotation_level": "failure", "title": "", "message": "The runner has received a shutdown signal. This can happen when the runner service is stopped, or a manually started runner is canceled."}]' >"$o/ann-madeup-shutdown.json"
  printf '%s\n' '[]' >"$o/ann-madeup-none.json"
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
    rerun='Not every step reported an end, so the workflow installer-smoke-rerun re-runs this job once, as attempt 2 of this run, unless a newer run of the same branch exists by then.'
  else
    rerun="Not every step reported an end. This is attempt $att; only attempt 1 is re-run, so this job is not re-run again."
  fi
  [ "$(tail -n 1 <<<"$out")" = "$rerun" ] || why+='the last line does not say whether it is re-run as expected; '
  job=$(jq -c --arg n "$name" '[.jobs[] | select(.name == $n)] | last' "$jf")
  grep -qF -- "The job \"$name\" (job $(jq -r .id <<<"$job"), attempt $att) ended \"$(jq -r .conclusion <<<"$job")\"" <<<"$out" ||
    why+='the job line does not name the job, its id, the attempt and its conclusion; '
  while IFS= read -r s; do
    grep -qF -- "$s" <<<"$out" || why+="not named: $s; "
  done < <(jq -r '.steps[] | select(.status == "completed" and .conclusion != "success" and .conclusion != "skipped")
                  | "step \(.number) \"\(.name)\" \(.conclusion)"' <<<"$job"
           jq -r '.steps[] | select(.status != "completed") | "step \(.number) \"\(.name)\" (\(.status)"' <<<"$job"
           jq -r '.[] | .message | gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" ")' "$af")
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

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
