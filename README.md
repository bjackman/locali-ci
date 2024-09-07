TODOS:

 - integration_test fails if you haven't already built the binary, because the
   test_bin package I'm using assumes it's already been built, because it
   assumes that it's being run as an integration test (in which case Cargo would
   take care of building the binary).
 - Shutdown still does not happen cleanly on my kernel repo. At least one reason
   for this seems to be that child processes inherit the SIGINT.
 - Still getting some `cargo stress` failures that need to be debugged. I think
   these are due to the TODO in integration_test.rs about how we reuse the
   actaul project repo for integration testing, in combination with the notify
   crate not handling file deletion happening concurrently with setup.
 - Bug: I don't see any "Started" statuses in my status render. Not sure if this
   is a status tracking bug or if the system is stuck somehow.
 - Bug: Status output doesn't seem to get updated when tested range shrinks?
 - Gather overall status and present it readably somehow to the user.
   - Present status with git DAG view.
 - Store output and artifacts. WIP but:
   - Location of this should be configurable.
   - Need to figure out how to represent internal errors and signals.
   - Probably need to split it up by tested repo.
   - Need to present it to the user in some convenient way
 - Cache results, configurable whether this is by commit or by tree.
 - Support bailing out more quickly if the worktree teardown is too slow.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
 - Provide a way to quickly check that tests in your configuration actually work.
 - Support running tests that don't need worktrees.
 - Support re-using worktrees.
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