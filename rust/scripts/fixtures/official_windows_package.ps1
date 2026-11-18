New-Item -ItemType Directory -Force -Path dist/release-artifacts/hashtree
Copy-Item target/$Target/release/git-remote-htree.exe dist/release-artifacts/hashtree/
Copy-Item target/$Target/release/htree-cashu.exe dist/release-artifacts/hashtree/
Copy-Item target/$Target/release/htree.exe dist/release-artifacts/hashtree/

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
"@ | Out-File -FilePath dist/release-artifacts/hashtree/README.txt -Encoding UTF8

Compress-Archive -Path dist/release-artifacts/hashtree -DestinationPath dist/release-artifacts/hashtree-$Target.zip
Remove-Item -Recurse dist/release-artifacts/hashtree
