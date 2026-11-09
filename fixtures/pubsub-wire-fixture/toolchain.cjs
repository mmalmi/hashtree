const fs=require('node:fs'),crypto=require('node:crypto'),cp=require('node:child_process');
const sha=p=>crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
fs.writeFileSync('/stage/evidence/toolchain.json',JSON.stringify({node:process.version,nodeSha256:sha(process.execPath),pnpm:cp.execFileSync('pnpm',['--version'],{encoding:'utf8'}).trim(),platform:process.platform,arch:process.arch},null,2)+'\n');
