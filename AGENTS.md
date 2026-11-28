# Notes for agents

- Checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `npm test --prefix tests/ui`. After changing `ui/index.html`,
  run `node prototype/build-demo.cjs`.
- After every fix, build and install the latest version: `pwsh scripts/install-windows.ps1`.
  Don't ask for Card Intake to be closed: the script closes it itself. With a job running
  (import, backup, Send, delivery, Finish) it waits for the job to end first, then closes,
  installs and reopens it; with none, it closes it straight away. A wait can be long, so
  run it in the background.
- Push only to `main`: no pull requests, other branches or HTTPS. `origin` goes to port 22,
  which doesn't work, so name the URL. It sometimes times out: try again before changing anything.

  ```
  git -c core.sshCommand="ssh -i C:/Users/emmmer.SUPERXLB/.ssh/id_ed25519 -o IdentitiesOnly=yes" push ssh://git@ssh.github.com:443/SEmmmer/card_intake.git main
  ```
