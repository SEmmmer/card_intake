# Notes for agents

- Checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `npm test --prefix tests/ui`. After changing `ui/index.html`,
  run `node prototype/build-demo.cjs`.
- Build after every fix. Use `pwsh scripts/install-windows.ps1` for every Windows
  release build so the new version is installed on this PC. Run it in the background
  and verify installation finishes. The script closes an idle Card Intake immediately.
  If jobs are running, it waits for all of them to finish, then closes the app,
  installs the update and reopens it if it was running.
- Push only to `main`: no pull requests, other branches or HTTPS. `origin` goes to port 22,
  which doesn't work, so name the URL. It sometimes times out: try again before changing anything.

  ```
  git -c core.sshCommand="ssh -i C:/Users/emmmer.SUPERXLB/.ssh/id_ed25519 -o IdentitiesOnly=yes" push ssh://git@ssh.github.com:443/SEmmmer/card_intake.git main
  ```
