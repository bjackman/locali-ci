Needs Rust >= 1.80.

To run this as a low priority on Linux, try prefixing the command with `chrt -i
0` which will run it as `SCHED_IDLE`. `nice -n 19` isn't really enough because
you probably have
[autogroups](https://man7.org/linux/man-pages/man7/sched.7.html) enabled. To run
at background priorities other than `SCHED_IDLE` you'll need to work around that
- the man page gives an example of running `echo 10 > /proc/self/autogroup` to
set `nice 10` for the current shell.

Bugs (high to low priority):

 - Tests don't work on my work computer. I think this is because I made false
   assumptions abuot the conditions for Git commit hashes to be deterministic.
 - `should_not_cache` test is flaky; occasionally the detector triggers that
   suggests two tests were sharing the same worktree. _Probably_ a bug in the
   test.

   I added some hacks to try and debug this. With

   ```
   RUST_LOG=info TMPDIR=/tmp/mytmp/ LCI_TESTS_LEAK_RESULT_DB=1 while cargo test -- --nocapture; continue ; end`
   ```

   I'm able to reproduce it and see the `-x` output of the test scripts but they
   don't make any sense to me, I got stuck and decided to work on something
   else.
 - Sometimes the system gets gummed up, I'm not sure if this is just a
   status reporting issue or if the system stops making progress at at all.
   Probably should fix all the simpler bugs first then look into this some more.
   I don't see this when running against this repo, only when running on my big fat kernel tree.
 - Status output doesn't seem to get updated when tested range shrinks?
 - No tests for checking config cache...
 - No tests for actual contents of config cache. (E.g: Nothing to catch bug
   where we deleted stdouts and stderrs).
 - Result database entries are stored with a hash of the test configuration. If
   the hash changes, the test needs to be re-run i.e. the cached is invalidated.
   But, this hash is not strong, this will break if there are collisions. We
   should store the whole config.
 - Unimportant bug: some tests get run twice by `cargo test`, because of
   `test_log`/`test_case` interaction.

Needed features (high to low priority):

 - Need a way for test command to report "error" as distinguished from failure.
 - Store output and artifacts. WIP but:
   - Provide a way to limit the size of the result cache.
   - Location of this should be configurable.
   - Probably need to split it up by tested repo.
   - Need to present it to the user in some convenient way
 - Make output results easier to reach. In particular at the moment if you have
   no hyperlinks support in your terminal you're basically out of luck.
 - Provide a way to quickly check that tests in your configuration actually work.
   Can't you just write them as a script and then test the script? Well if we
   rely on that then we need the result caching logic to be aware of when the
   script changes.
 - Need a way to delete stored results.

   (Or do we? If we had an error reporting
   mechanism then there would be no need for this since you'd just modify the
   configuration to adopt the error reporting, and in that case the cache would
   be invalidated anyway. But, also need to consider cases where something was
   wrong in the host system)
 - Need timeouts!
 - Support saving artifacts so the user can reuse or analyze them later.
 - Probably want a (default?) option to merge stderr and stdout.
 - Fix output format, probably have to implement a pager in `ratatui`.
 - Support bailing out more quickly if the worktree teardown is too slow.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
 - Support re-using worktrees.
 - Probably need to have the system handle cleaning the worktree for you. If
   your build system etc can't be trusted to avoid polluting the workspace/being
   resilient against a polluted workspace, you'll wanna put `git clean -fdx` in
   your test script. However, once we have the `test` subcommand we'll also be
   running in the "main" worktree where the user probably doesn't wanna do that.
   So we probably need a higher-level notion of "cleaning the worktree" that's
   aware of this.
 - Provide a
   [jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html).
   Issue with this will be when test commands crash and leak job slots. I think
   a reasonable workaround for that would just be to reset the slot count when
   the test manager becomes `settled` (this assumes that all test scripts can
   make progress on a single thread when the job server starves them, as is the
   case for Make, since all jobs have one implicit job slot).
 - Document config format.
 - Make it easier to share configs. At present the distinction between config
   file content and arg content may be a mit messy (e.g. `num_worktrees` is as
   much a property of the system running the service as the project being
   tested).
 - Support multiple repos?
 - Respect git's color configuration.
 - (Nice to have: avoid creating worktrees if they aren't actually to be used).
 - (Nice to have: let jobs that don't need worktrees start before worktrees are ready).

My janky test command:

```
RUST_LOG_STYLE=always RUST_LOG=debug cargo watch -- bash -c "cargo test --color=always -- |& less -R -F -c"
```