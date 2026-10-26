# Git for humans and agents

Hashtree Git lets you publish and clone repositories without a hosting account.
Your local key controls your published repositories. Use the same Git commands
from a terminal, a coding agent, or a script; browse repositories in
[Iris Git](https://git.iris.to/).

For Codex, Claude, Grok Build, and Hermes setup, see the
[agent plugin](../plugins/hashtree-git/README.md). The walkthrough below also works without a
plugin. It uses a POSIX shell on macOS or Linux.

## Install

Install the prebuilt `htree` CLI and Git helper (this example pins `v0.2.150`):

```sh
curl -fsSL https://github.com/mmalmi/hashtree/releases/download/v0.2.150/install.sh -o /tmp/install-hashtree.sh
# Inspect the installer, then run it:
sh /tmp/install-hashtree.sh
export PATH="$HOME/.local/bin:$PATH"
htree --version
command -v git-remote-htree
```

Or use `cargo install hashtree-cli git-remote-htree`. See
[installation](../rust/crates/git-remote-htree/README.md#installation) for Windows
archives and other options. A daemon is optional for this Git workflow.

## Give an agent its own identity

An existing human installation normally uses `~/.hashtree`. For a separate
agent, choose a persistent private directory outside every repository:

```sh
export HTREE_CONFIG_DIR="$HOME/.local/share/hashtree-agent/config"
export HTREE_DATA_DIR="$HOME/.local/share/hashtree-agent/data"
umask 077
mkdir -p "$HTREE_CONFIG_DIR" "$HTREE_DATA_DIR"
htree user
```

The first call generates a key if needed and prints public identities. The first
line identifies `self`; its first field is the shareable `npub…` public key.
Save these environment settings in the agent's launch configuration. Keep its
`keys` file private and backed up; replacing or losing it changes who can publish
updates. Never put it in Git or paste it into a prompt. Each writer should have
its own identity.

## Publish a small public repository

This example intentionally publishes a new repository named `hello-hashtree`
under your identity. Choose a different unused name for another demonstration.
Public Git history includes committed files and author metadata.

```sh
mkdir hello-hashtree
cd hello-hashtree
git init -b main
printf 'Hello from a human or an agent.\n' > hello.txt
git add hello.txt
git -c user.name='Hashtree demo' -c user.email='demo@localhost' \
  commit -m 'First commit'
git remote add htree htree://self/hello-hashtree
git push htree main
```

`demo@localhost` is a Git author label, not a mailbox or signup. Use your preferred
public attribution for real work. In an existing repository, review its history
and add the `htree` remote without replacing existing remotes.

After a successful push, obtain the shareable addresses:

```sh
PUBLISHER=$(htree user | awk 'NR == 1 { print $1 }')
printf 'Clone: htree://%s/hello-hashtree\n' "$PUBLISHER"
printf 'Browse: https://git.iris.to/#/%s/hello-hashtree\n' "$PUBLISHER"
git rev-parse HEAD
```

Use the full public identity in shared URLs. `self` always means the current
machine's configured identity.

## Verify from another machine

Install the helper there, then use the clone URL from the previous step:

```sh
git clone 'htree://PASTE_PUBLIC_NPUB/hello-hashtree' hello-clone
git -C hello-clone rev-parse HEAD
cat hello-clone/hello.txt
```

The commit must match the publisher's `HEAD`, and the file must contain the
expected text. No publisher secret is needed. For a same-machine check, use
different `HTREE_CONFIG_DIR` and `HTREE_DATA_DIR` directories so the reader cannot
reuse the writer's key or cached objects.

## Collaborate with separate identities

Clone the author's public URL, make your changes locally, and publish a fork
under your own identity:

```sh
git remote add mine htree://self/hello-hashtree-fork
git push mine HEAD
```

Share your fork's public URL and branch with the author. They can fetch it, review
the diff, and merge it with ordinary Git. `htree pr create --help` describes the
optional pull-request announcement command. Publishing a fork does not grant you
write access to the original repository. Do not share a signing key between
writers or force-push to resolve unexpected remote state.

## Choose visibility before publishing

| Remote URL ending | Who can read |
| --- | --- |
| No fragment | Anyone; default public publication |
| `#link-visible` | People with the generated secret URL |
| `#private` | The publishing identity |

For link-visible repositories, the first push prints setup instructions: save
the generated `#k=…` URL as the remote and push again. That setup step alone is
not a completed publication. The secret URL grants read access; keep it out of
public logs and posts. Removing a URL fragment does not revoke previously shared
data. For private repositories, another device needs the author's securely
provisioned identity; this is not the independent-reader demo above.

See the [Git helper guide](../rust/crates/git-remote-htree/README.md) for details.

## When something fails

- **Git cannot find the helper:** put `git-remote-htree` on `PATH` in the actual
  agent process, not only your interactive shell.
- **A URL points at the wrong author:** check `htree user` and the profile
  environment. Use public identities when sharing URLs.
- **A push is rejected or a fresh clone cannot finish:** inspect the error and
  configured relay/storage availability. Do not treat a timeout as an empty
  repository or hide it with a force push.
- **An ephemeral agent loses its identity:** attach persistent private storage
  and restore its key through the host's secret mechanism before publishing.

The protocol requires no provider signup, but relay and storage operators set
their own access, pricing, and retention policies. Keep a local repository and
arrange storage or mirroring for data you need to remain available.
