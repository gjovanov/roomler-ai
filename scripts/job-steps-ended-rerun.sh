#!/usr/bin/env bash
# Ask GitHub to re-run the installer smoke's Windows job once, when that job
# ended without every step reporting an end (#3810).
#
# Run by the workflow installer-smoke-rerun
# (.github/workflows/installer-smoke-rerun.yml) when an installer-smoke run
# completes. That workflow's job holds `actions: write` and runs this file from
# the default branch's checkout, never from a pull request's code.
#
# In, from the environment: GITHUB_REPOSITORY; RUN_ID, the smoke run's id (the
# only value of that run the workflow passes); SMOKE_JOB and CHECK_JOB, the
# names of the smoke's Windows job and of its check job, matched by equality
# (scripts/job-steps-ended-test.sh holds the workflow's copies equal to the
# jobs' `name:` in installer-smoke.yml); GITHUB_STEP_SUMMARY; GH_TOKEN for gh.
#
# In this order, reading attempt 1's jobs once:
#   G1  the run reads attempt 1 now (its live attempt, read from the API, not
#       the event's): nobody has re-run it meanwhile.
#   G2  the smoke's check job is listed in attempt 1 and reads neither
#       `skipped` nor `cancelled`. GitHub skips that job in a run cancelled
#       before it started, by a person or by a newer push. A cancel that comes
#       after it ended is not seen: the API does not say who cancelled.
#       Then the Windows job is looked up in the same list; not listed, no
#       re-run.
#   The decision, scripts/job-steps-ended.sh, on that same list: `complete`
#       ends here. Its lines are printed; its annotations line is not (this
#       job does not read the annotations).
#   G3  read last, right before the re-run is asked: no smoke run created
#       after this one exists for the same branch, event and head repository.
#       Whether a re-run enters the smoke's concurrency group again, GitHub's
#       documentation does not say; if it does, there it could cancel the
#       newer run. A push between this read and the re-run is not seen.
# Then ONE re-run of the Windows job and the jobs that need it:
#   POST /repos/{owner}/{repo}/actions/jobs/{job_id}/rerun
#   (https://docs.github.com/en/rest/actions/workflow-runs#re-run-a-job-from-a-workflow-run)
#
# What it prints: nothing that came from the run reaches a workflow command.
# Its first line stops the runner's workflow commands with a token random per
# run; everything up to `::<token>::` (the decision's lines, which carry step
# names a pull request's workflow file chose, and anything gh says) is plain
# log. After that comes ONE line, also written to the job summary, in this
# script's own words, with nothing of the run but numbers (its id, attempt and
# job ids, step counts) and GitHub's status words: what it did, and why. A
# read or a re-run that failed is also a `::warning`. It always exits 0: a red
# run of this workflow would sit on the default branch's head, where the
# smoke run's reader does not look, and the smoke's check job says where to
# look when no attempt 2 follows.
set -uo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=${GITHUB_REPOSITORY-} run=${RUN_ID-} smoke_job=${SMOKE_JOB-} check_job=${CHECK_JOB-}
summary=${GITHUB_STEP_SUMMARY:-/dev/null}
token=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
if ! [[ $token =~ ^[0-9a-f]{32}$ && $repo =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ && $run =~ ^[0-9]+$ ]] ||
  [ -z "$smoke_job" ] || [ -z "$check_job" ]; then
  line='Did not re-run: no random token, or no repository, run id or job names this script can use.'
  echo "::warning title=installer-smoke-rerun::$line"
  echo "$line"
  echo "$line" >>"$summary"
  exit 0
fi

tmp=$(mktemp -d)
closed=no
trap '[ "$closed" = yes ] || echo "::$token::"; rm -rf "$tmp"' EXIT
echo "::stop-commands::$token"
exec 2>&1

end() { # LINE [warning]
  closed=yes
  echo "::$token::"
  [ "${2-}" != warning ] || echo "::warning title=installer-smoke-rerun::$1"
  echo "$1"
  echo "$1" >>"$summary"
  exit 0
}
stop() { end "Did not re-run run $run: $1" "${2-}"; }
# One of GitHub's status words (`cancelled`, `failure`, ...), or `unreadable`.
word() { if [[ $1 =~ ^[a-z_]{1,32}$ ]]; then printf '%s' "$1"; else printf 'unreadable'; fi; }

# G1
gh api "repos/$repo/actions/runs/$run" >"$tmp/run.json" || stop 'the run could not be read.' warning
a=$(jq -r '.run_attempt' "$tmp/run.json" 2>/dev/null)
[[ $a =~ ^[0-9]+$ ]] || stop 'the run could not be read.' warning
[ "$a" = 1 ] || stop "it reads attempt $a now, so it has been re-run already."

# G2, then the Windows job in the same list.
gh api "repos/$repo/actions/runs/$run/attempts/1/jobs?per_page=100" >"$tmp/jobs.json" ||
  stop 'its jobs could not be read.' warning
jq -e '.jobs | type == "array"' "$tmp/jobs.json" >/dev/null 2>&1 || stop 'its jobs could not be read.' warning
c=$(jq -r --arg n "$check_job" '[.jobs[] | select(.name == $n)]
  | if length == 0 then "not listed" else last | .conclusion // "none" end' "$tmp/jobs.json") ||
  stop 'its jobs could not be read.' warning
case "$c" in
  'not listed') stop 'its check job is not listed in attempt 1.' ;;
  skipped | cancelled) stop "its check job reads \"$c\" in attempt 1: the run was cancelled, or its Windows job succeeded or was skipped." ;;
esac
jid=$(jq -r --arg n "$smoke_job" '[.jobs[] | select(.name == $n)] | last | .id // empty' "$tmp/jobs.json") ||
  stop 'its jobs could not be read.' warning
[ -n "$jid" ] || stop 'its Windows job is not listed in attempt 1.'
[[ $jid =~ ^[0-9]+$ ]] || stop 'its jobs could not be read.' warning

# The decision, on the list read above.
out=$(bash "$here/job-steps-ended.sh" --job "$smoke_job" --attempt 1 --jobs "$tmp/jobs.json")
rc=$?
grep -vxF "GitHub's annotations on the job: not read." <<<"$out"
[ "$rc" -eq 0 ] || stop "its Windows job could not be read as completed (the decision script exited $rc)." warning
case "$(head -n 1 <<<"$out")" in
  verdict=complete) stop "every step of its Windows job (job $jid) reported an end." ;;
  verdict=incomplete) ;;
  *) stop 'the decision script printed no verdict.' warning ;;
esac

# G3, last.
branch=$(jq -r '.head_branch // empty' "$tmp/run.json")
event=$(jq -r '.event // empty' "$tmp/run.json")
head_repo=$(jq -r '.head_repository.full_name // empty' "$tmp/run.json")
workflow=$(jq -r '.workflow_id // empty' "$tmp/run.json")
created=$(jq -r '.created_at // empty' "$tmp/run.json")
[ -n "$branch" ] && [ -n "$event" ] && [ -n "$head_repo" ] && [[ $workflow =~ ^[0-9]+$ ]] &&
  [[ $created =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
  stop 'its branch, event, head repository, workflow or creation time could not be read.' warning
gh api -X GET "repos/$repo/actions/workflows/$workflow/runs" -f branch="$branch" -f event="$event" \
  -f created=">$created" -f per_page=100 >"$tmp/newer.json" ||
  stop 'the newer runs of its branch could not be read.' warning
newer=$(jq -r --arg r "$head_repo" --arg c "$created" --argjson id "$run" \
  '[.workflow_runs[] | select(.id != $id and .head_repository.full_name == $r and .created_at > $c)] | first | .id // empty' \
  "$tmp/newer.json") || stop 'the newer runs of its branch could not be read.' warning
if [ -n "$newer" ]; then
  [[ $newer =~ ^[0-9]+$ ]] || stop 'the newer runs of its branch could not be read.' warning
  stop "a newer run of the same branch, event and head repository exists (run $newer)."
fi

# The re-run of the one job.
gh api -X POST "repos/$repo/actions/jobs/$jid/rerun" >/dev/null ||
  stop "GitHub refused the re-run of its Windows job (job $jid)." warning
n=$(jq -r --arg n "$smoke_job" '[.jobs[] | select(.name == $n)] | last
  | "\([.steps[]? | select(.status != "completed")] | length) \(.steps // [] | length) \(.conclusion)"' "$tmp/jobs.json")
read -r open steps concl <<<"$n"
end "Asked GitHub to re-run job $jid of run $run once, and GitHub accepted: the job ended \"$(word "$concl")\" and $open of its $steps steps reported no end."
