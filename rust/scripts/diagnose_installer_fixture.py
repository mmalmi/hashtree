#!/usr/bin/env python3
"""Read-only diagnosis of the recorded public fixture address; no signer or publish."""
import json, os, pathlib, re, signal, subprocess, tempfile
ROOT = pathlib.Path(os.environ["RUNNER_TEMP"])
BINS = ROOT / "draft-qualification/linux-install/installed"
REMOTE = "htree://npub1ur8lswyw0l5gespud2w5ks2q0lmqrex6vwhvlau8rlz332ctxgcql975rw/hashtree-installer-fixture-4a48a74fee84186b"
WS = ["wss://upload.iris.to/ws", "wss://cdn.iris.to/ws"]
DEFAULTS = ["wss://temp.iris.to", "wss://nos.lol", "wss://relay.snort.social"]
assert not os.environ.get("GH_TOKEN") and not os.environ.get("GITHUB_TOKEN")
results=[]
for name,relays in [("failed-publication-inputs",WS+DEFAULTS),("available-normal-defaults",[DEFAULTS[0],DEFAULTS[2]]),("ws-only",WS)]:
    with tempfile.TemporaryDirectory(prefix="fixture-readonly-",dir=ROOT) as directory:
        work=pathlib.Path(directory);config=work/"config";data=work/"data";config.mkdir()
        (config/"config.toml").write_text("[storage]\nmax_size_gb = 1\n[updater]\nauto_check = false\n[server]\nenable_fips = false\nenable_fips_lan_discovery = false\n[nostr]\nrelays = "+json.dumps(relays)+"\nbootstrap_follows = []\n[blossom]\nread_servers = [\"https://upload.iris.to\", \"https://cdn.iris.to\"]\nwrite_servers = []\n")
        env={"PATH":str(BINS)+":/usr/bin:/bin","TMPDIR":directory,"LANG":"C.UTF-8","HTREE_CONFIG_DIR":str(config),"HTREE_DATA_DIR":str(data),"HTREE_PREFER_LOCAL_DAEMON":"0","NOSTR_PREFER_LOCAL":"0","NOSTR_RELAYS":",".join(relays),"GIT_CONFIG_GLOBAL":"/dev/null","GIT_CONFIG_NOSYSTEM":"1","GIT_TERMINAL_PROMPT":"0","RUST_LOG":"warn"}
        child=subprocess.Popen(["git","ls-remote",REMOTE],cwd=work,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
        timed_out=False
        try:out,err=child.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            timed_out=True;os.killpg(child.pid,signal.SIGTERM)
            try:out,err=child.communicate(timeout=5)
            except subprocess.TimeoutExpired:os.killpg(child.pid,signal.SIGKILL);out,err=child.communicate()
        text=err.decode("utf-8",errors="replace");lines=[]
        for line in text.splitlines():
            if not re.search(r"error|fatal|incomplete|unreadable|index|timed out|timeout|failed|refused|budget|WARN",line,re.I):continue
            line=re.sub(r"\x1b\[[0-9;]*m","",line).replace(directory,"<ephemeral-dir>")
            line=re.sub(r"(?:/home/runner|/tmp|/var/tmp)/[^\s,;]+","<ephemeral-path>",line)
            line=re.sub(r"nsec1[a-z0-9]+","<redacted-key>",line)
            lines.append(line[:1500])
        result={"profile":name,"relays":relays,"exit":child.returncode,"timeout":timed_out,"stdoutBytes":len(out),"diagnostics":lines[-12:]}
        results.append(result);print(json.dumps(result),flush=True)
        assert not (config/"keys").exists(),"Read-only lookup unexpectedly created a signing key"
(ROOT/"fixture-readonly-diagnosis.json").write_text(json.dumps({"remote":REMOTE,"results":results},indent=2)+"\n")
