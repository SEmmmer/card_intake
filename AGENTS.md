# Notes for agents

## Pushing

- Always push straight to `main`. No pull requests, no other branches, no HTTPS.
- GitHub is reached over SSH on port 443 with this key. This command checks the
  connection (it answers "Hi SEmmmer! You've successfully authenticated"):

  ```
  ssh -T -p 443 git@ssh.github.com -i C:\Users\emmmer.SUPERXLB\.ssh\id_ed25519
  ```

- To push:

  ```
  git -c core.sshCommand="ssh -i C:/Users/emmmer.SUPERXLB/.ssh/id_ed25519 -o IdentitiesOnly=yes" push ssh://git@ssh.github.com:443/SEmmmer/card_intake.git main
  ```

  Name the URL as above rather than `origin`: `origin` is `git@github.com:SEmmmer/card_intake.git`,
  which goes to port 22.
- The connection sometimes times out. Try again before changing anything.
