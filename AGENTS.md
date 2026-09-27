# Sunrise

Managed by Bun workspace. Core logic must be isolated to make it possible to deterministically unit test in isolation. UI is modern is efficient and thoughtfully laid out.

CI does not run the Apple jobs on pull requests. Before merging a change that reaches what the Apple apps are built from (the path list is in the README's Apple section), run `mise run apple-app` on a Mac at the pull request's head. It is the same task CI runs after the merge.

