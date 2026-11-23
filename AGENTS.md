# Notes for agents

- Checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `npm test --prefix tests/ui`. After changing `ui/index.html`,
  run `node prototype/build-demo.cjs`.
- After building the desktop app, install it: `pwsh scripts/install-windows.ps1`. If it says
  Card Intake is open, ask for it to be closed, then run it again with `-NoBuild`.
- Push only to `main`: no pull requests, other branches or HTTPS. `origin` goes to port 22,
  which doesn't work, so name the URL. It sometimes times out: try again before changing anything.

  ```
  git -c core.sshCommand="ssh -i C:/Users/emmmer.SUPERXLB/.ssh/id_ed25519 -o IdentitiesOnly=yes" push ssh://git@ssh.github.com:443/SEmmmer/card_intake.git main
  ```
