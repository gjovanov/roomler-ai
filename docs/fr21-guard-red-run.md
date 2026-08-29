# FR-21 guard — deliberate red run. NOT FOR MERGE.

FR-21's acceptance criterion asks that the guard be proven by a **linked red
run, not an assertion**. Every other proof in the FR was a mutation shown to
fail before it was trusted to pass; the guard itself was the exception, proven
only locally. This PR is that run.

The mutation is the next line — an unmarked reference to the retired daemon
name, with no anchor:

    the daemon used to be called roomler-agent

Expected: `Retired-name audit (FR-21)` fails with
`1 unclassified occurrence(s); strict mode requires 0`, and blocks this PR,
because P5 removed `continue-on-error`.

Close once the run is linked from #809.
