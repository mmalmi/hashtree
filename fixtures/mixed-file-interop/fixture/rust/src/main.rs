//! Fixture only. Public crates, real authenticated FIPS, no application-source copy.
use fips_core::{config::{TransportInstances, WebSocketConfig}, FipsEndpoint, PeerIdentity};
use fips_file_transfer::{TransferEvent, TransferFile, TransferSender};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, io::{self, BufRead, Write}, path::{Path, PathBuf}, sync::Arc, time::Duration};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const VECTOR_SHA: &str = "39254c05fb2a2dc9bc4b6dd21e775f79c25f9da089685b62254f6afdde099cf9";
fn emit(value: Value) { println!("{value}"); io::stdout().flush().unwrap(); }
fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> { v[key].as_str().ok_or_else(|| format!("Missing {key}").into()) }
fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn contents(size: usize, salt: u8) -> Vec<(String, Vec<u8>)> {
    vec![("data.bin".into(), (0..size).map(|i| ((i + salt as usize) % 251) as u8).collect()),
        ("empty".into(), vec![]), ("café.txt".into(), b"mixed language durable receipt\n".to_vec())]
}
fn files(root: &Path, size: usize, salt: u8, source: bool) -> Result<Vec<TransferFile>> {
    if !(400_123..=2_097_275).contains(&size) { return Err("fixture size bound".into()); }
    contents(size, salt).into_iter().map(|(filename, bytes)| {
        let path = root.join("source").join(&filename);
        if source {
            if path.exists() { if fs::read(&path)? != bytes { return Err("source changed".into()); } }
            else { let mut f = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                f.write_all(&bytes)?; f.sync_all()?; }
        }
        Ok(TransferFile { filename, size_bytes: bytes.len() as u64, sha256: hash(&bytes), path })
    }).collect()
}
fn verify(root: &Path, size: usize, salt: u8) -> Result<Value> {
    let mut expected = vec![];
    for (name, bytes) in contents(size, salt) {
        let path = root.join("received").join(&name);
        let actual = fs::read(&path)?;
        if actual != bytes { return Err("receiver bytes differ".into()); }
        let meta = path.metadata()?;
        expected.push(json!({"filename":name,"bytes":actual.len(),"sha256":hash(&actual),
            "modifiedNs":meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos().to_string()}));
    }
    if fs::read_dir(root.join("received"))?.count() != expected.len() { return Err("extra receiver output".into()); }
    Ok(json!(expected))
}
fn event(value: TransferEvent) -> Value {
    match value {
        TransferEvent::Offered { transfer_id, .. } => json!({"event":"offered","id":transfer_id}),
        TransferEvent::Accepted { transfer_id, .. } => json!({"event":"accepted","id":transfer_id}),
        TransferEvent::Progress { transfer_id, transferred_bytes, total_bytes, .. } => json!({"event":"progress","id":transfer_id,"bytes":transferred_bytes,"total":total_bytes}),
        TransferEvent::Completed { transfer_id, .. } => json!({"event":"completed","id":transfer_id}),
        TransferEvent::Paused { transfer_id, .. } => json!({"event":"paused","id":transfer_id}),
        TransferEvent::Failed { transfer_id, error, .. } => json!({"event":"failed","id":transfer_id,"error":error}),
        TransferEvent::Cancelled { transfer_id, .. } => json!({"event":"cancelled","id":transfer_id}),
        TransferEvent::Declined { transfer_id, .. } => json!({"event":"declined","id":transfer_id}),
    }
}
fn vector_check(path: &Path) -> Result<Value> {
    let bytes=fs::read(path)?; if hash(&bytes)!=VECTOR_SHA { return Err("vector bytes changed".into()); }
    let v: Value=serde_json::from_slice(&bytes)?;
    let manifest: Vec<fips_file_transfer::ManifestFile>=serde_json::from_value(v["manifest"]["files"].clone())?;
    if fips_file_transfer::wire::manifest_hash(&manifest)? != text(&v["manifest"],"sha256")? { return Err("manifest vector".into()); }
    for row in v["controls"].as_array().ok_or("controls")? {
        let control=serde_json::from_value(row["control"].clone())?;
        let encoded=fips_file_transfer::wire::control(&control)?;
        let hex=encoded.iter().map(|v|format!("{v:02x}")).collect::<String>();
        if hex!=text(row,"frame_hex")? { return Err("control vector".into()); }
    }
    Ok(json!({"sha256":VECTOR_SHA,"controls":v["controls"].as_array().unwrap().len(),"manifest":true}))
}

#[tokio::main(flavor="multi_thread", worker_threads=2)]
async fn main() -> Result<()> {
    let args=std::env::args().collect::<Vec<_>>();
    let cfg: Value=serde_json::from_slice(&fs::read(args.get(1).ok_or("config required")?)?)?;
    let seed=text(&cfg,"seed")?;
    let port=seed.strip_prefix("ws://127.0.0.1:").and_then(|s|s.strip_suffix("/fips")).ok_or("loopback seed only")?.parse::<u16>()?;
    if port==0 { return Err("seed port".into()); }
    let root=PathBuf::from(text(&cfg,"directory")?);
    for p in [&root,&root.join("source"),&root.join("received")] { private_dir(p)?; }
    let mut config=fips_core::Config::new();
    config.node.control.enabled=false;
    config.node.discovery.lan.enabled=false;
    config.node.discovery.nostr.enabled=false;
    config.node.routing.mode=fips_core::config::RoutingMode::ReplyLearned;
    config.transports.udp=TransportInstances::default();
    config.transports.websocket=TransportInstances::Single(WebSocketConfig { seed_urls:vec![seed.into()],..Default::default() });
    let endpoint=Arc::new(FipsEndpoint::builder().config(config)
        .identity_nsec(text(&cfg,"testSecret")?).without_system_tun().bind().await?);
    let identity=PeerIdentity::from_npub(endpoint.npub())?;
    let (events_tx,events)=flume::bounded(256);
    let (sender,task)=fips_file_transfer::start(endpoint.clone(),root.join("state"),events_tx).await?;
    let mut sender=Some(sender);
    let (commands_tx,commands)=flume::bounded(8);
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line)=line else { break };
            if line.len()>16*1024 || commands_tx.send(line).is_err() { break; }
        }
    });
    emit(json!({"event":"ready","peer":fips_file_transfer::peer_hex(identity),"npub":endpoint.npub(),
        "vectors":vector_check(Path::new(text(&cfg,"vectors")?))?}));
    loop {
        tokio::select! {
            value=events.recv_async()=>{ if let Ok(value)=value {emit(event(value));} else {break;} },
            line=commands.recv_async()=>{
                let Ok(line)=line else {break};
                let command: Value=serde_json::from_str(&line)?;
                if command["op"]=="stop" {break;}
                let value=execute(&command,&root,sender.as_ref().unwrap());
                emit(match value {Ok(value)=>json!({"reply":command["request"],"value":value}),
                    Err(error)=>json!({"reply":command["request"],"error":error.to_string()})});
            },
        }
    }
    drop(sender.take()); // The published runtime checkpoints active v2 streams on command-channel close.
    tokio::time::timeout(Duration::from_secs(10),task).await??;
    while let Ok(value)=events.try_recv() {emit(event(value));}
    endpoint.shutdown().await?;
    emit(json!({"event":"stopped","graceful":true}));
    Ok(())
}
fn execute(command: &Value, root: &Path, sender: &TransferSender) -> Result<Value> {
    match text(command,"op")? {
        "snapshots"=>Ok(json!(sender.snapshots().into_iter().map(|s|json!({"id":s.id,"status":s.status,
            "sender":s.is_sender,"bytes":s.transferred_bytes,"total":s.total_bytes})).collect::<Vec<_>>())),
        "verify"=>verify(root,command["size"].as_u64().ok_or("size")? as usize,command["salt"].as_u64().ok_or("salt")? as u8),
        op @ ("offer"|"receive")=>{
            let peer=PeerIdentity::from_npub(text(command,"peerNpub")?)?;
            let files=files(root,command["size"].as_u64().ok_or("size")? as usize,command["salt"].as_u64().ok_or("salt")? as u8,op=="offer")?;
            let id=text(command,"id")?.into();let token=text(command,"token")?.into();
            if op=="offer" {sender.register_offer(id,token,vec![peer],files)?;}
            else {sender.receive(id,token,peer,files,root.join("received"),true)?;}
            Ok(json!(true))
        },
        _=>Err("unknown fixture operation".into()),
    }
}
