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
) -> Result<PoolMigrationLaunchRequestV3> {
    let Some(path) = std::env::var_os("HTREE_POOL_CONTROLLER_TEST_MUTATIONS") else {
        return Ok(request);
    };
    let mutations: Vec<RequestMutation> = serde_json::from_slice(&std::fs::read(path)?)?;
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
