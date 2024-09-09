Needs Rust >= 1.80.

TODOS:

 - `Manager::set_revisions` urgently needs refactoring.
 - Need tests for cancellation of not-yet-started jobs.
 - Shutdown still does not happen cleanly on my kernel repo. At least one reason
   for this seems to be that child processes inherit the SIGINT. Another is that
   Ctrl-C just doesn't always kill the service.
 - Bug: Sometimes "Cancelled" test statuses get cached.
 - Bug: SIGINT from local-ci shutdown gets cached.
   - Workaround (Fish): `for f in (find ~/.local/share/local-ci/ -name result.json | xargs grep -l "terminated by"); rm -rf (dirname (dirname $f)); end`
 - Bug: Sometimes the system gets gummed up, I'm not sure if this is just a
   status reporting issue or if the system stops making progress at at all.
   Probably should fix all the simpler bugs first then look into this some more.
 - Bug: Bogus output directory names for `by_tree` tests (doesn't affect functionality).
 - Cache should also include hash of test config.
   is a status tracking bug or if the system is stuck somehow.
 - Bug: Status output doesn't seem to get updated when tested range shrinks?
 - Gather overall status and present it readably somehow to the user.
   - Present status with git DAG view.
 - Store output and artifacts. WIP but:
   - Location of this should be configurable.
   - Need to figure out how to represent internal errors and signals.
   - Probably need to split it up by tested repo.
   - Need to present it to the user in some convenient way
 - Support bailing out more quickly if the worktree teardown is too slow.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
 - Provide a way to quickly check that tests in your configuration actually work.
 - Support running tests that don't need worktrees.
 - Support other resources than worktrees and "tokens". Could e.g. be used for dev servers.
 - Support re-using worktrees.
 - Support saving artifacts so the user can reuse or analyze them later.
 - Provide a
   [jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html).
   Issue with this will be when test commands crash and leak job slots. I think
   a reasonable workaround for that would just be to reset the slot count when
   the test manager becomes `settled` (this assumes that all test scripts can
   make progress on a single thread when the job server starves them, as is the
   case for Make, since all jobs have one implicit job slot).
 - Provide a way to limit the size of the result cache.
 - Document config format.
 - Support multiple repos?
 - (Nice to have: avoid creating worktrees if they aren't actually to be used).
 - (Nice to have: let jobs that don't need worktrees start before worktrees are ready).
 - Unimportant bug: some tests get run twice by `cargo test`, because of
   `test_log`/`test_case` interaction.
 - Respect git's color configuration.

My janky test command:

```
RUST_LOG_STYLE=always RUST_LOG=debug cargo watch -- bash -c "cargo test --color=always -- |& less -R -F -c"
```