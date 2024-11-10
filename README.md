Needs Rust >= 1.80.

To run this as a low priority on Linux, try prefixing the command with `chrt -i
0` which will run it as `SCHED_IDLE`. `nice -n 19` isn't really enough because
you probably have
[autogroups](https://man7.org/linux/man-pages/man7/sched.7.html) enabled. To run
at background priorities other than `SCHED_IDLE` you'll need to work around that
- the man page gives an example of running `echo 10 > /proc/self/autogroup` to
set `nice 10` for the current shell.

To run the tests very hard, follow
[this](https://askubuntu.com/questions/162229/how-do-i-increase-the-open-files-limit-for-a-non-root-user)
then run `ulimit -Sn 524288` to embiggen the file descriptor limit. Now you can
try using `cargo-stress`.

Bugs (high to low priority):

 - BLOCKER: Aside from generally being crappy and needing to be rewritten to do the
   terminal commands properly (e.g. at present it fundamentally doesn't work if
   the output buffer has more lines than your terminal) (this probably means
   using Ratatui), there's some dumb bug in the terminal UI logic that means it
   leaks lines.
   I spent some time looking into adopting Ratatui, but that [doesn't support
   hyperlinks](https://github.com/ratatui/ratatui/issues/1028). I think the next
   best thing is probably to do this manually instead of looking for a way to
   work around that. All I really need is to implement a pager. Then if I wanna
   get passtionate about this the right answer is probably to contribute to
   Ratatui.
 - Sometimes when I've run this thing overnight, the next day I noticed that it
   was no longer updating the terminal UI. It still seems to actually be running
   the tests. I suspect some task somewhere is panicking, and I haven't done the
   error handling properly to cause this to feed back to crashing the main
   thread. I didn't think of this possibility before overwriting the logs that
   would have had the panic details in there, so I'll just have to wait and see
   until it happens again.
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

   OK update, I can't reproduce it like that now that I fixed a bunch of related
   bugs, but I still sometimes can with `cargo stress`. I don't fukken know this
   is driving me mad.
 - Sometimes I see issues where my `git clean -fdx` command at the beginning of
   my test script fails like `fatal: Cannot lstat
   'arch/x86/platform/intel/.iosf_mbi.o.d': No such file or directory`.

   I've looked into one case where I see this happen, and I observed that the
   prior user of the workspace had been a make command that got cancelled, and
   the cancellation timed out.

   Could the issue just be that when you `SIGKILL` make (which is what happens
   when cancellation times out), it leaks child processes? What can we do about
   that - try to block until all child processes of the job we ran have
   terminated?

   Hacky temporary fix: massively increase timeout before we SIGKILL.
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

 - BLOCKER: Document config format (and everything else).
 - BLOCKER: Store output artifacts. 
   - Provide a way to limit the size of the result cache.
   - Location of this should be configurable.
 - BLOCKER: Need to clean up the logging situation. User should at least get
   some feedback about the process starting up and shutting down.
 - BLOCKER: Rename it.
 - Need a way for test command to report "error" as distinguished from failure.
 - Make output results easier to reach. In particular at the moment if you have
   no hyperlinks support in your terminal you're basically out of luck.
 - Need a way to delete stored results.

   (Or do we? If we had an error reporting
   mechanism then there would be no need for this since you'd just modify the
   configuration to adopt the error reporting, and in that case the cache would
   be invalidated anyway. But, also need to consider cases where something was
   wrong in the host system)
 - Need timeouts! (There is a shutdown grace period, so we don't just leak
   resources if tests get stuck forever, they'll get cancelled when te user needs
   to run a new test. But we should also notify the user if they don't seem to
   be getting any test results.)
 - Probably want a (default?) option to merge stderr and stdout.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
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
 - Sometimes you have a test that needs access to the worktree but not
   exclusive. In that case we could run multiple jobs in parallel in the same
   worktree.
   
   ACTUALLY - I bet we could do the same thing with overlayfs even if the jobs
   _did_ need exclusive access to the worktree.
 - Presumably via cgroups it's reasonably to ensure that jobs don't leak child
   processes. If you have a backdoor like the Docker daemon then that isn't
   possible but normally it should be fine I think?
 - Make it easier to share configs. At present the distinction between config
   file content and arg content may be a mit messy (e.g. `num_worktrees` is as
   much a property of the system running the service as the project being
   tested).
 - Maybe people who are sharing configs would want to be able to make them more
   hermetic, so it might make sense to integrate this with some sort of
   container runtime. However, probably it's also just easy to put a `podman
   run` command in your config or whatever? That way your hermetic configuration
   can also easily be reused in proper CI.
 - Support re-using existing worktrees, instead of blocking startup while they
   are created? I dunno though maybe that's pointless with the `test` comamnd.
   Starting the tool up shouldn't need to be something you do very often after
   we have that.
 - Support multiple repos?
 - Respect git's color configuration.

Probably sketchy design choices:

 - Test scripts are shut down by sending `SIGTERM` to the _whole process group_.
   The rationale for this is that it's kinda annoying to write a bash script
   that actually shuts down cleanly on `SIGTERM` otherwise - if you do any
   background stuff then it will just leak.

   I think this is probably just a symptom of me not really knowing how to do
   Unix programming properly, I dunno. It probably needs to change.

My janky test command:

```
RUST_LOG_STYLE=always RUST_LOG=debug cargo watch -- bash -c "cargo test --color=always -- |& less -R -F -c"
```