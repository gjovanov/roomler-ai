#!/usr/bin/env bash
# Did every step of one job of a workflow run report an end? (#3810)
#
# The installer smoke's Windows job (.github/workflows/installer-smoke.yml) is
# re-run once, as attempt 2 of its run, when it ended without every step
# reporting an end. This script is that decision, and its only copy: the
# smoke's check job prints its lines in the run, and the workflow
# installer-smoke-rerun (.github/workflows/installer-smoke-rerun.yml, through
# scripts/job-steps-ended-rerun.sh) asks GitHub to re-run that one job when it
# reads `incomplete` on attempt 1.
#
# The rule:
#   complete    GitHub lists at least one step of the job, and the status of
#               every one of them is `completed`.
#   incomplete  anything else: a step whose status is not `completed`
#               (`in_progress`, `pending`, ...), or no step listed at all.
# Neither the job's conclusion nor GitHub's annotations on the job decide; both
# are printed. A re-run runs the real test again, and a failure that comes and
# goes can pass on it. When in doubt the job is re-run.
#
# The lines say what GitHub lists and nothing more. They name no cause: a step
# that reported no end is printed with the API's own status word. Every string
# the API gives (a step's name first) is printed with each control character,
# U+0000 to U+001F and U+007F, replaced by a space, so that no name can start a
# line of its own: a pull request's own workflow file names its steps.
#
# Usage:
#   job-steps-ended.sh --job NAME --attempt N --repo OWNER/REPO --run ID
#   job-steps-ended.sh --job NAME --attempt N --jobs FILE [--annotations FILE]
# The first form reads the API with `gh` (a token that can read the run's jobs
# and the job's check run). It reads the attempt's jobs up to
# JOB_STEPS_ENDED_READS times (6), JOB_STEPS_ENDED_WAIT seconds apart (10),
# until the job reads completed: the job that runs this starts when the job it
# reads is done, and the API can lag behind that. The second form reads saved
# API replies (scripts/job-steps-ended-test.sh).
#
# The job is the LAST one listed whose name EQUALS NAME, never a prefix match:
# another job's name can start with NAME.
#
# Output, on stdout: the line `verdict=complete` or `verdict=incomplete`, then
# six lines that say what was read and whether the job is re-run.
# Exit: 0 with a verdict. 2, with no verdict line and one line that says why,
# when the job cannot be read, is not listed by that name, or is not
# completed. 64 on a usage error.
set -uo pipefail

job_name='' attempt='' repo='' run='' jobs_file='' ann_file=''

usage() {
  echo "job-steps-ended.sh: $1" >&2
  echo 'usage: job-steps-ended.sh --job NAME --attempt N (--repo OWNER/REPO --run ID | --jobs FILE [--annotations FILE])' >&2
  exit 64
}

while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || usage "$1 needs a value"
  case "$1" in
    --job) job_name=$2 ;;
    --attempt) attempt=$2 ;;
    --repo) repo=$2 ;;
    --run) run=$2 ;;
    --jobs) jobs_file=$2 ;;
    --annotations) ann_file=$2 ;;
    *) usage "unknown argument $1" ;;
  esac
  shift 2
done

[ -n "$job_name" ] || usage '--job is required'
[[ $attempt =~ ^[1-9][0-9]*$ ]] || usage '--attempt must be a run attempt number (1, 2, ...)'
if [ -n "$jobs_file" ]; then
  [ -z "$repo$run" ] || usage '--jobs reads a file: leave out --repo and --run'
else
  [[ $repo =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || usage '--repo must be OWNER/REPO (or give --jobs FILE)'
  [[ $run =~ ^[0-9]+$ ]] || usage '--run must be a run id'
  [ -z "$ann_file" ] || usage '--annotations goes with --jobs'
fi

reads=${JOB_STEPS_ENDED_READS:-6}
wait=${JOB_STEPS_ENDED_WAIT:-10}
[ -z "$jobs_file" ] || reads=1 # a saved reply does not change
where="attempt $attempt${run:+ of run $run}"

# jq: `clean` makes each control character of a string a space; `clean_all`
# does it to every string in a value. The job is matched by its name as GitHub
# lists it, and cleaned once it is chosen, before anything of it is printed.
CLEAN='def clean: tostring | explode | map(if . < 32 or . == 127 then 32 else . end) | implode;
  def clean_all: if type == "string" then clean elif type == "array" then map(clean_all)
    elif type == "object" then map_values(clean_all) else . end;'

job='' got=no
for ((i = 1; i <= reads; i++)); do
  if [ -n "$jobs_file" ]; then
    all=$(cat -- "$jobs_file") || all=''
  else
    all=$(gh api "repos/$repo/actions/runs/$run/attempts/$attempt/jobs?per_page=100") || all=''
  fi
  if jq -e 'has("jobs")' <<<"$all" >/dev/null 2>&1; then
    got=yes
    job=$(jq -c --arg n "$job_name" "$CLEAN"' [.jobs[] | select(.name == $n)] | last // empty | clean_all' <<<"$all")
    [ -n "$job" ] && [ "$(jq -r .status <<<"$job")" = completed ] && break
  fi
  [ "$i" -lt "$reads" ] && sleep "$wait"
done

if [ "$got" = no ]; then
  echo "The jobs of $where could not be read, so there is no verdict."
  exit 2
fi
if [ -z "$job" ]; then
  echo "No job named \"$job_name\" is listed in $where, so there is no verdict."
  exit 2
fi
status=$(jq -r '.status' <<<"$job")
id=$(jq -r '.id' <<<"$job")
if [ "$status" != completed ]; then
  echo "The job \"$job_name\" (job $id, $where) reads \"$status\", not completed, after $reads read(s), so there is no verdict."
  exit 2
fi

# Lines 1 to 5: the verdict, the job, the steps' count, the steps that reported
# an end other than success or skipped, and the steps that reported no end.
lines=$(jq -r --arg attempt "$attempt" '
  def q: "\"" + tostring + "\"";
  def plural($n; $word): "\($n) \($word)\(if $n == 1 then "" else "s" end)";
  def t: try fromdateiso8601 catch null;
  (.steps // []) as $s
  | [$s[] | select(.status == "completed")] as $ended
  | [$s[] | select(.status != "completed")] as $open
  | [$ended[] | select(.conclusion != "success" and .conclusion != "skipped")] as $bad
  | (if (.started_at // "") != "" and (.completed_at // "") != "" then
       (.started_at | t) as $a | (.completed_at | t) as $b
       | if $a != null and $b != null then " after \((($b - $a + 30) / 60) | floor) min (\(.started_at) to \(.completed_at))" else " (\(.started_at) to \(.completed_at))" end
     else "" end) as $took
  | (if ($s | length) > 0 and ($open | length) == 0 then "complete" else "incomplete" end) as $verdict
  | "verdict=\($verdict)",
    "The job \(.name | q) (job \(.id), attempt \($attempt)) ended \((.conclusion // "with no conclusion") | q)\($took), runner \((.runner_name // "") | if . == "" then "none" else . end | q).",
    (if ($s | length) == 0 then "GitHub lists no step of this job."
     else "GitHub lists \(plural($s | length; "step")): \($ended | length) reported an end"
       + (if ($ended | length) > 0 then " (" + ($ended | group_by(.conclusion) | map("\(length) \(.[0].conclusion // "with no conclusion")") | join(", ")) + ")" else "" end)
       + ", \($open | length) did not."
     end),
    "Reported an end other than \"success\" or \"skipped\": "
      + (if ($bad | length) == 0 then "none." else ($bad | map("step \(.number) \(.name | q) \(.conclusion // "with no conclusion")") | join("; ")) + "." end),
    "Reported no end: "
      + (if ($open | length) == 0 then "none." else ($open | map("step \(.number) \(.name | q) (\(.status)\(if (.started_at // "") != "" then ", started \(.started_at)" else "" end))") | join("; ")) + "." end)
' <<<"$job") || {
  echo "The job \"$job_name\" (job $id, $where) could not be read, so there is no verdict."
  exit 2
}
verdict=$(head -n 1 <<<"$lines")
verdict=${verdict#verdict=}

# Line 6: GitHub's annotations on the job, printed as read. They never decide.
annotations() {
  local a
  if [ -n "$jobs_file" ]; then
    [ -n "$ann_file" ] || { echo 'not read'; return; }
    a=$(cat -- "$ann_file") || { echo 'could not be read'; return; }
  else
    a=$(gh api "repos/$repo/check-runs/$id/annotations?per_page=100") || { echo 'could not be read'; return; }
  fi
  jq -r "$CLEAN"' if type != "array" then error("not a list")
         elif length == 0 then "none"
         else map("\(.annotation_level // "annotation" | clean) \"\((.message // "") | clean | gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" "))\"") | join("; ")
         end' <<<"$a" 2>/dev/null || echo 'could not be read'
}

printf '%s\n' "$lines"
echo "GitHub's annotations on the job: $(annotations)."
# Line 7: whether the job is re-run.
if [ "$verdict" = complete ]; then
  echo 'Every step reported an end, so this job is not re-run.'
elif [ "$attempt" = 1 ]; then
  echo 'Not every step reported an end, so the workflow installer-smoke-rerun asks GitHub to re-run this job once, as attempt 2 of this run, unless one of its guards refuses. If no attempt 2 follows, its run among the installer-smoke-rerun runs in the Actions list says why.'
else
  echo "Not every step reported an end. This is attempt $attempt; only attempt 1 is re-run, so this job is not re-run again."
fi
exit 0
