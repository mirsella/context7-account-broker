# Agent Notes

## Deployment

- npm publishing is handled by GitHub Actions in `.github/workflows/publish.yml`.
- The `release` workflow creates a GitHub release from the version in `package.json`:
  `gh workflow run release.yml`
- Publishing runs automatically when that GitHub release is published.
- Manual publish runs default to a dry run; use `gh workflow run publish.yml -f dry_run=false` to publish.
- npm trusted publishing uses GitHub OIDC. Do not add npm tokens to the repository or workflow.
