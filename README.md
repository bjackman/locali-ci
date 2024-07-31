TODOS:

 - Gather overall status and present it readably somehow to the user.
   - Present status with git DAG view.
 - Store output and artifacts, present them to the user in a convenient way.
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
