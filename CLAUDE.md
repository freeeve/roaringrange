# roaringrange

## Task tracking
Tasks live in the central taskman store (~/.taskman), one dir per project;
the project resolves from the enclosing repo's basename (pin long sessions
with TASKMAN_PROJECT). The legacy repo-local `tasks/` ledger was migrated
there (task 079) and is gitignored.
- Pick work: `taskman top`, then `taskman start <n>` -> work -> commit ->
  append an Outcome section -> `taskman done <n>`.
- New work: `taskman new <desc>`. Ask another project:
  `taskman file <project> <desc>`.
- Never read ~/.taskman/<project>/screenshots/ -- images are for the web UI
  (`taskman serve`). Task bodies may link them; ignore the links.
