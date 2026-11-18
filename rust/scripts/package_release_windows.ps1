# Shared by the official release workflow and the Haps source recipe.
param(
    [ValidateSet("x86_64-pc-windows-msvc")]
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$BinaryDirectory = "target/$Target/release",
    [string]$OutputDirectory = "dist/release-artifacts"
)
$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force -Path "$OutputDirectory/hashtree"
Copy-Item "$BinaryDirectory/git-remote-htree.exe" "$OutputDirectory/hashtree/"
Copy-Item "$BinaryDirectory/htree-cashu.exe" "$OutputDirectory/hashtree/"
Copy-Item "$BinaryDirectory/htree.exe" "$OutputDirectory/hashtree/"

@"
hashtree - Git over Nostr via Merkle trees
==========================================

Binaries included:
  htree.exe             - CLI and daemon for hashtree operations
  htree-cashu.exe       - Cashu wallet helper for htree cashu
  git-remote-htree.exe  - Git remote helper for htree:// URLs

Install:
  Copy all three .exe files to a directory in your PATH, e.g.:
  C:\Users\<you>\AppData\Local\Microsoft\WindowsApps\

Usage:
  htree add <file>                    # add file to hashtree
  htree get <hash>                    # download by hash
  htree start                         # start P2P daemon
  htree cashu balance                 # inspect Cashu wallet
  git clone htree://npub1.../repo     # clone git repo

More info: https://github.com/mmalmi/hashtree
"@ | Out-File -FilePath "$OutputDirectory/hashtree/README.txt" -Encoding UTF8

Compress-Archive -Path "$OutputDirectory/hashtree" -DestinationPath "$OutputDirectory/hashtree-$Target.zip"
Remove-Item -Recurse "$OutputDirectory/hashtree"
