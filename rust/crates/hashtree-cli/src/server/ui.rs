use axum::response::Html;

pub fn root_page() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Hashtree - Content-Addressed Storage</title>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: #0a0a0a; color: #e0e0e0; line-height: 1.6; }
        .container { max-width: 900px; margin: 0 auto; padding: 40px 20px; }
        h1 { font-size: 2.5em; margin-bottom: 10px; background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); -webkit-background-clip: text; -webkit-text-fill-color: transparent; }
        .subtitle { color: #888; margin-bottom: 40px; }
        .card { background: #1a1a1a; border-radius: 12px; padding: 30px; margin-bottom: 30px; border: 1px solid #2a2a2a; }
        .upload-area { border: 2px dashed #444; border-radius: 8px; padding: 40px; text-align: center; transition: all 0.3s; cursor: pointer; }
        .upload-area:hover { border-color: #667eea; background: #1f1f1f; }
        input[type="file"] { width: 100%; padding: 12px; margin: 10px 0; background: #0a0a0a; border: 1px solid #333; border-radius: 6px; color: #e0e0e0; font-size: 14px; }
        button { background: linear-gradient(135deg, #667eea 0%, #764ba2 100%); color: white; padding: 12px 30px; border: none; border-radius: 6px; cursor: pointer; font-size: 16px; font-weight: 600; transition: transform 0.2s; }
        button:hover { transform: translateY(-2px); }
        .status { padding: 10px; margin: 10px 0; border-radius: 6px; display: none; }
        .status.success { background: #1a3a1a; color: #4ade80; border: 1px solid #2a5a2a; display: block; }
        .status.error { background: #3a1a1a; color: #f87171; border: 1px solid #5a2a2a; display: block; }
        .cid { font-family: monospace; color: #667eea; word-break: break-all; }
        a { color: #667eea; text-decoration: none; }
        a:hover { text-decoration: underline; }
    </style>
</head>
<body>
    <div class="container">
        <h1>Hashtree</h1>
        <p class="subtitle">Like Blossom, but with directories and chunking</p>

        <div class="card">
            <h2>Upload File</h2>
            <form id="uploadForm" enctype="multipart/form-data">
                <div class="upload-area" id="dropArea">
                    <input type="file" id="fileInput" name="file" required>
                </div>
                <button type="submit" id="uploadBtn">Upload</button>
            </form>
            <div id="status" class="status"></div>
        </div>

        <div class="card">
            <h2>Pinned CIDs</h2>
            <div id="pinList">Loading...</div>
            <button onclick="loadPins()">Refresh</button>
            <button onclick="runGC()">Run Garbage Collection</button>
        </div>

        <div class="card">
            <h2>Git Repositories</h2>
            <div id="gitRepos">Loading...</div>
            <button onclick="loadGitRepos()">Refresh</button>
        </div>

        <div class="card">
            <h2>Storage Stats</h2>
            <div id="stats">Loading...</div>
            <button onclick="loadStats()">Refresh</button>
        </div>

        <div class="card">
            <h2>Retrieve by CID</h2>
            <input type="text" id="cidInput" placeholder="Enter CID (e.g., bafireign7yfvtni25wlzwj6hm7zlrkq3ecxdlpifisu5y5d4kynug2bgyy)">
            <button onclick="retrieveCID()">Retrieve</button>
        </div>

        <div class="card">
            <h2>Nostr Resolver</h2>
            <p style="color: #888; margin-bottom: 15px;">Look up content by npub/treename (resolves to hash via Nostr relays)</p>
            <input type="text" id="npubInput" placeholder="npub1... or hex pubkey">
            <input type="text" id="treeInput" placeholder="tree name">
            <button onclick="resolveTree()">Resolve & View</button>
            <button onclick="listTrees()">List Trees</button>
            <div id="resolverStatus" class="status"></div>
            <div id="treeList" style="margin-top: 15px;"></div>
        </div>
    </div>

    <script>
        // Read auth from URL hash or localStorage
        let AUTH_USER = localStorage.getItem('hashtree_auth_user');
        let AUTH_PASS = localStorage.getItem('hashtree_auth_pass');

        // Check URL hash for credentials
        if (window.location.hash) {
            const hash = window.location.hash.substring(1);
            const parts = hash.split(':');
            if (parts.length === 2) {
                AUTH_USER = parts[0];
                AUTH_PASS = parts[1];

                // Save to localStorage
                localStorage.setItem('hashtree_auth_user', AUTH_USER);
                localStorage.setItem('hashtree_auth_pass', AUTH_PASS);

                // Clear hash from URL
                history.replaceState(null, '', window.location.pathname + window.location.search);
            }
        }

        function getAuthHeaders() {
            if (AUTH_USER && AUTH_PASS) {
                const credentials = btoa(`${AUTH_USER}:${AUTH_PASS}`);
                return { 'Authorization': `Basic ${credentials}` };
            }
            return {};
        }

        async function loadPins() {
            const response = await fetch('/api/pins');
            const data = await response.json();
            const pinList = document.getElementById('pinList');
            if (data.pins && data.pins.length > 0) {
                pinList.innerHTML = data.pins.map(pin => {
                    const icon = pin.is_directory ? '📁' : '📄';
                    return `<div style="padding: 10px; margin: 5px 0; background: #0a0a0a; border-radius: 6px; display: flex; align-items: center; justify-content: space-between;">
                        <div style="flex: 1;">
                            <div style="margin-bottom: 5px;">
                                <span style="font-size: 18px;">${icon}</span>
                                <a href="/${pin.cid}" style="font-weight: 600; margin-left: 8px;">${pin.name}</a>
                            </div>
                            <div class="cid" style="font-size: 12px; opacity: 0.7; margin-left: 26px;">${pin.cid}</div>
                        </div>
                        <button onclick="unpinCID('${pin.cid}')" style="padding: 5px 15px; font-size: 12px;">Unpin</button>
                    </div>`;
                }).join('');
            } else {
                pinList.innerHTML = '<p style="color: #666;">No pins yet</p>';
            }
        }

        async function loadStats() {
            const response = await fetch('/api/stats');
            const data = await response.json();
            const stats = document.getElementById('stats');
            stats.innerHTML = `
                <p>Total DAGs: ${data.total_dags}</p>
                <p>Pinned DAGs: ${data.pinned_dags}</p>
                <p>Total Size: ${(data.total_bytes / 1024).toFixed(2)} KB</p>
            `;
        }

        async function loadGitRepos() {
            const gitRepos = document.getElementById('gitRepos');
            try {
                const response = await fetch('/api/git/repos');
                if (!response.ok) {
                    gitRepos.innerHTML = '<p style="color: #666;">Git not configured</p>';
                    return;
                }
                const data = await response.json();
                if (data.error) {
                    gitRepos.innerHTML = `<p style="color: #666;">${data.error}</p>`;
                    return;
                }
                if (!data.has_repo || data.branches.length === 0) {
                    const cloneUrl = window.location.origin + data.clone_url;
                    gitRepos.innerHTML = `<p style="color: #666;">No repositories yet</p>
                        <p style="font-size: 12px; color: #888; margin-top: 10px;">Push a repo:</p>
                        <code style="display: block; background: #0a0a0a; padding: 10px; border-radius: 4px; font-size: 12px; word-break: break-all;">git remote add hashtree ${cloneUrl}<br>git push hashtree main</code>`;
                    return;
                }
                const cloneUrl = window.location.origin + data.clone_url;
                gitRepos.innerHTML = `
                    <div style="padding: 10px; margin: 5px 0; background: #0a0a0a; border-radius: 6px;">
                        <div style="margin-bottom: 10px;">
                            <span style="font-size: 18px;">📦</span>
                            <span style="font-weight: 600; margin-left: 8px;">Repository</span>
                        </div>
                        <div style="font-size: 12px; margin-bottom: 10px;">
                            <span style="color: #888;">Clone:</span>
                            <code class="cid" style="margin-left: 5px;">${cloneUrl}</code>
                        </div>
                        <div style="font-size: 12px;">
                            <span style="color: #888;">Branches:</span>
                            ${data.branches.map(b => `<span style="background: #667eea33; padding: 2px 8px; border-radius: 4px; margin-left: 5px;">${b.name}</span>`).join('')}
                        </div>
                    </div>`;
            } catch (e) {
                gitRepos.innerHTML = '<p style="color: #666;">Git not configured</p>';
            }
        }

        async function runGC() {
            const response = await fetch('/api/gc', {
                method: 'POST',
                headers: getAuthHeaders()
            });
            const data = await response.json();
            alert(`GC complete: Deleted ${data.deleted_dags} DAGs, freed ${(data.freed_bytes / 1024).toFixed(2)} KB`);
            loadPins();
            loadStats();
        }

        async function unpinCID(cid) {
            await fetch(`/api/unpin/${cid}`, {
                method: 'POST',
                headers: getAuthHeaders()
            });
            loadPins();
        }

        function retrieveCID() {
            const cid = document.getElementById('cidInput').value;
            if (cid) {
                window.open(`/${cid}`, '_blank');
            }
        }

        document.getElementById('uploadForm').addEventListener('submit', async (e) => {
            e.preventDefault();
            const formData = new FormData(e.target);
            const status = document.getElementById('status');

            try {
                const response = await fetch('/upload', {
                    method: 'POST',
                    headers: getAuthHeaders(),
                    body: formData
                });
                const data = await response.json();

                if (data.success) {
                    status.className = 'status success';
                    status.innerHTML = `Upload successful! CID: <a href="/${data.cid}" class="cid">${data.cid}</a>`;
                    loadPins();
                    loadStats();
                } else {
                    status.className = 'status error';
                    status.textContent = `Error: ${data.error}`;
                }
            } catch (error) {
                status.className = 'status error';
                status.textContent = `Error: ${error.message}`;
            }
        });

        async function resolveTree() {
            const npub = document.getElementById('npubInput').value;
            const tree = document.getElementById('treeInput').value;
            const status = document.getElementById('resolverStatus');

            if (!npub || !tree) {
                status.className = 'status error';
                status.textContent = 'Please enter both npub/pubkey and tree name';
                return;
            }

            status.className = 'status';
            status.style.display = 'block';
            status.style.background = '#1a1a3a';
            status.style.color = '#667eea';
            status.textContent = 'Resolving...';

            try {
                const response = await fetch(`/api/resolve/${encodeURIComponent(npub)}/${encodeURIComponent(tree)}`);
                const data = await response.json();

                if (data.error) {
                    status.className = 'status error';
                    status.textContent = `Error: ${data.error}`;
                } else {
                    status.className = 'status success';
                    status.innerHTML = `Resolved! Hash: <a href="/${data.hash}" class="cid">${data.hash}</a>`;
                    // Open in new tab
                    window.open(`/n/${encodeURIComponent(npub)}/${encodeURIComponent(tree)}`, '_blank');
                }
            } catch (error) {
                status.className = 'status error';
                status.textContent = `Error: ${error.message}`;
            }
        }

        async function listTrees() {
            const npub = document.getElementById('npubInput').value;
            const treeList = document.getElementById('treeList');
            const status = document.getElementById('resolverStatus');

            if (!npub) {
                status.className = 'status error';
                status.textContent = 'Please enter an npub or pubkey';
                return;
            }

            status.className = 'status';
            status.style.display = 'block';
            status.style.background = '#1a1a3a';
            status.style.color = '#667eea';
            status.textContent = 'Fetching trees...';

            try {
                const response = await fetch(`/api/trees/${encodeURIComponent(npub)}`);
                const data = await response.json();

                if (data.error) {
                    status.className = 'status error';
                    status.textContent = `Error: ${data.error}`;
                    treeList.innerHTML = '';
                } else {
                    status.style.display = 'none';
                    if (data.trees && data.trees.length > 0) {
                        treeList.innerHTML = data.trees.map(t => `
                            <div style="padding: 10px; margin: 5px 0; background: #0a0a0a; border-radius: 6px;">
                                <div style="margin-bottom: 5px;">
                                    <span style="font-size: 18px;">🌲</span>
                                    <a href="/n/${encodeURIComponent(npub)}/${encodeURIComponent(t.name)}" style="font-weight: 600; margin-left: 8px;">${t.name}</a>
                                </div>
                                <div class="cid" style="font-size: 12px; opacity: 0.7; margin-left: 26px;">${t.hash}</div>
                            </div>
                        `).join('');
                    } else {
                        treeList.innerHTML = '<p style="color: #666;">No trees found for this pubkey</p>';
                    }
                }
            } catch (error) {
                status.className = 'status error';
                status.textContent = `Error: ${error.message}`;
                treeList.innerHTML = '';
            }
        }

        // Load initial data
        loadPins();
        loadStats();
        loadGitRepos();
    </script>
</body>
</html>"#)
}
