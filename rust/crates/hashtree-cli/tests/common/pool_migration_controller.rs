use super::*;
use clap::Parser;
use serde::Deserialize;

#[derive(Parser)]
struct ControllerFixtureArgs {
    #[command(flatten)]
    options: crate::app::args::PoolMigrationControllerArgs,
}

#[derive(Deserialize)]
struct RequestMutation {
    pointer: String,
    value: Value,
}

pub(super) fn before_publication(
    request: PoolMigrationLaunchRequestV3,
    worker_uid: u32,
    worker_gid: u32,
) -> Result<PoolMigrationLaunchRequestV3> {
    let Some(path) = std::env::var_os("HTREE_POOL_CONTROLLER_TEST_MUTATIONS") else {
        return Ok(request);
    };
    let options: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if options.get("directWorker") == Some(&Value::Bool(true)) {
        return reject_direct_worker(request, worker_uid, worker_gid);
    }
    let mutations: Vec<RequestMutation> = serde_json::from_value(options)?;
    let mut value = serde_json::to_value(request)?;
    for mutation in mutations {
        *value.pointer_mut(&mutation.pointer).with_context(|| {
            format!(
                "request mutation pointer does not exist: {}",
                mutation.pointer
            )
        })? = mutation.value;
    }
    Ok(serde_json::from_value(value)?)
}

fn reject_direct_worker(
    mut request: PoolMigrationLaunchRequestV3,
    worker_uid: u32,
    worker_gid: u32,
) -> Result<PoolMigrationLaunchRequestV3> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    request.nonce = fresh_nonce();
    let attempt = request.attempt_namespace.join(&request.nonce);
    create_attempt_directory(&attempt, worker_gid)?;
    request.attempt_identity = file_identity(&attempt, "direct-worker test attempt")?;
    let request_path = attempt.join("launch-request.json");
    let request_index = request
        .argv
        .iter()
        .position(|arg| arg == "--launch-request")
        .context("generated worker argv has no launch request")?
        + 1;
    request.argv[request_index] = request_path.display().to_string();
    let child = Command::new(&request.binary.path)
        .args(&request.argv[1..])
        .uid(worker_uid)
        .gid(worker_gid)
        .env_clear()
        .env("INVOCATION_ID", &request.systemd_invocation_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn direct production worker under the live controller")?;
    request.main_pid = child.id();
    request.proc_start_time_ticks = process_start_time(child.id())?;
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    durable_create_atomic(&request_path, &bytes, 0o640, 0, worker_gid, &request.nonce)?;
    let output = child
        .wait_with_output()
        .context("wait for direct worker rejection")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success()
        || !stderr.contains("is not in the exact requested systemd service cgroup")
    {
        bail!("direct worker did not fail at systemd admission: {stderr}");
    }
    if attempt.join("launch-ack.json").exists() {
        bail!("direct worker wrote an acknowledgement before systemd admission");
    }
    // Stop before publishing the ordinary worker's request: only the attempted
    // direct worker ran, while the genuine controller remained its live broker.
    bail!("direct worker rejected before launch acknowledgement: {stderr}")
}

#[test]
#[ignore = "fixture process: launched only by the Linux systemd integration harness"]
fn controller_process() {
    let path = std::env::var_os("HTREE_POOL_CONTROLLER_TEST_ARGUMENTS")
        .expect("controller fixture arguments path");
    let argv: Vec<String> =
        serde_json::from_slice(&std::fs::read(path).expect("controller fixture arguments"))
            .expect("controller fixture argv");
    let parsed = ControllerFixtureArgs::try_parse_from(
        std::iter::once("controller-fixture".to_owned()).chain(argv.into_iter().skip(3)),
    )
    .expect("parse production controller arguments");
    let arguments = parsed.options;
    let result = run(PoolMigrationControllerOptions {
        preflight: arguments.preflight,
        rollout_dir: arguments.rollout_dir,
        rollout_id: arguments.rollout_id,
        phase: arguments.phase,
        controller_executable: arguments.controller_executable,
        controller_systemd_unit: arguments.controller_systemd_unit,
        controller_systemd_fragment: arguments.controller_systemd_fragment,
        controller_systemd_environment_file: arguments.controller_systemd_environment_file,
        controller_state_input: arguments.controller_state_input,
        source_baseline_input: arguments.source_baseline_input,
        pool_topology_input: arguments.pool_topology_input,
        additional_cas: arguments.additional_cas,
        writer_units: arguments.writer_units,
        systemd_unit: arguments.systemd_unit,
        systemctl: arguments.systemctl,
        systemd_fragment: arguments.systemd_fragment,
        systemd_environment_file: arguments.systemd_environment_file,
        service_gid: arguments.service_gid,
        migration_binary: arguments.migration_binary,
        target_data_dir: arguments.target_data_dir,
        pool: arguments.pool,
        delete_protection_lease_id: arguments.delete_protection_lease_id,
        delete_protection_record_sha256: arguments.delete_protection_record_sha256,
        source: arguments.source,
        source_external_dir: arguments.source_external_dir,
        state_file: arguments.state_file,
        batch_size: arguments.batch_size,
        max_buffer_mib: arguments.max_buffer_mib,
        source_read_concurrency: arguments.source_read_concurrency,
        reopen_batches: arguments.reopen_batches,
        max_items: arguments.max_items,
        launch_request_wait: Duration::from_secs(arguments.launch_request_wait_seconds),
        acknowledgement_wait: Duration::from_secs(arguments.acknowledgement_wait_seconds),
    });
    if let Err(error) = result {
        panic!("controller fixture failed: {error:#}");
    }
}
