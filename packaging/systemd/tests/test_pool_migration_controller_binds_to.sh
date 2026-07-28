#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]] || ! command -v systemctl >/dev/null; then
  echo "skip: Linux systemd is required"
  exit 0
fi
if ! systemctl --system show --property=Version >/dev/null 2>&1; then
  echo "skip: a running system manager is required"
  exit 0
fi

suffix="$(tr -d '-' </proc/sys/kernel/random/uuid)"
instance="bind-probe-${suffix}"
controller_unit="hashtree-pool-migration-controller@${instance}.service"
worker_unit="hashtree-pool-migration-worker@${instance}.service"
controller_fragment="/run/systemd/system/${controller_unit}"
worker_fragment="/run/systemd/system/${worker_unit}"
generated_controller="$(mktemp)"
generated_worker="$(mktemp)"

cleanup() {
  sudo -n /usr/bin/systemctl --system stop "${worker_unit}" >/dev/null 2>&1 || true
  sudo -n /usr/bin/systemctl --system stop "${controller_unit}" >/dev/null 2>&1 || true
  sudo -n /usr/bin/rm -f -- "${worker_fragment}" "${controller_fragment}"
  sudo -n /usr/bin/systemctl --system daemon-reload
  sudo -n /usr/bin/systemctl --system reset-failed \
    "${worker_unit}" "${controller_unit}" >/dev/null 2>&1 || true
  /bin/rm -f -- "${generated_worker}" "${generated_controller}"
}
trap cleanup EXIT

printf '%s\n' \
  '[Unit]' \
  'Description=Generated root Pool migration controller bind probe' \
  '[Service]' \
  'Type=exec' \
  'ExecStart=/bin/sleep infinity' \
  'Restart=no' \
  'KillMode=control-group' \
  >"${generated_controller}"
printf '%s\n' \
  '[Unit]' \
  'Description=Generated Pool migration worker bind probe' \
  "BindsTo=${controller_unit}" \
  "After=${controller_unit}" \
  '[Service]' \
  'Type=oneshot' \
  'ExecStart=/bin/sleep infinity' \
  'Restart=no' \
  'KillMode=control-group' \
  >"${generated_worker}"
sudo -n /usr/bin/install -o root -g root -m 0644 \
  "${generated_controller}" "${controller_fragment}"
sudo -n /usr/bin/install -o root -g root -m 0644 \
  "${generated_worker}" "${worker_fragment}"
sudo -n /usr/bin/systemctl --system daemon-reload
[[ "$(systemctl --system show "${controller_unit}" --property=NeedDaemonReload --value)" == no ]]

sudo -n /usr/bin/systemctl --system start --no-block "${controller_unit}"
for _ in $(seq 1 100); do
  controller_pid="$(systemctl --system show "${controller_unit}" --property=MainPID --value)"
  controller_active="$(systemctl --system show "${controller_unit}" --property=ActiveState --value)"
  controller_substate="$(systemctl --system show "${controller_unit}" --property=SubState --value)"
  [[ "${controller_pid}" != 0 && "${controller_active}" == active && "${controller_substate}" == running ]] && break
  sleep 0.01
done
[[ "${controller_pid}" =~ ^[1-9][0-9]*$ ]]
[[ "${controller_active}" == active ]]
[[ "${controller_substate}" == running ]]
[[ "$(systemctl --system show "${controller_unit}" --property=Type --value)" == exec ]]
[[ "$(systemctl --system show "${controller_unit}" --property=Restart --value)" == no ]]
[[ "$(systemctl --system show "${controller_unit}" --property=NRestarts --value)" == 0 ]]
first_controller_invocation="$(systemctl --system show "${controller_unit}" --property=InvocationID --value)"
[[ "${first_controller_invocation}" =~ ^[0-9a-f]{32}$ ]]

# A manager reload must not silently restart or replace the active controller.
printf '%s\n' '# generated live daemon-reload probe' >>"${generated_controller}"
sudo -n /usr/bin/install -o root -g root -m 0644 \
  "${generated_controller}" "${controller_fragment}"
sudo -n /usr/bin/systemctl --system daemon-reload
[[ "$(systemctl --system show "${controller_unit}" --property=NeedDaemonReload --value)" == no ]]
[[ "$(systemctl --system show "${controller_unit}" --property=MainPID --value)" == "${controller_pid}" ]]
[[ "$(systemctl --system show "${controller_unit}" --property=InvocationID --value)" == "${first_controller_invocation}" ]]
[[ "$(systemctl --system show "${controller_unit}" --property=ActiveState --value)" == active ]]
[[ "$(systemctl --system show "${controller_unit}" --property=SubState --value)" == running ]]
[[ "$(systemctl --system show "${controller_unit}" --property=NRestarts --value)" == 0 ]]

# Death during the worker start transaction must leave no orphan.
sudo -n /usr/bin/systemctl --system start --no-block "${worker_unit}" >/dev/null 2>&1 || true
sudo -n /usr/bin/systemctl --system kill --kill-whom=main --signal=KILL \
  "${controller_unit}"
for _ in $(seq 1 200); do
  worker_pid="$(systemctl --system show "${worker_unit}" --property=MainPID --value)"
  worker_active="$(systemctl --system show "${worker_unit}" --property=ActiveState --value)"
  if [[ "${worker_pid}" == 0 && "${worker_active}" != activating && "${worker_active}" != active ]]; then
    break
  fi
  sleep 0.01
done
[[ "${worker_pid}" == 0 ]]
[[ "${worker_active}" != activating && "${worker_active}" != active ]]
[[ "$(systemctl --system show "${controller_unit}" --property=NRestarts --value)" == 0 ]]
[[ "$(systemctl --system show "${controller_unit}" --property=MainPID --value)" == 0 ]]
[[ -z "$(systemctl --system show "${controller_unit}" --property=Job --value)" ]]

sudo -n /usr/bin/systemctl --system stop "${worker_unit}" >/dev/null 2>&1 || true
sudo -n /usr/bin/systemctl --system stop "${controller_unit}" >/dev/null 2>&1 || true
sudo -n /usr/bin/systemctl --system reset-failed \
  "${controller_unit}" "${worker_unit}" >/dev/null 2>&1 || true
sudo -n /usr/bin/systemctl --system start --no-block "${controller_unit}"
for _ in $(seq 1 100); do
  controller_pid="$(systemctl --system show "${controller_unit}" --property=MainPID --value)"
  controller_active="$(systemctl --system show "${controller_unit}" --property=ActiveState --value)"
  controller_substate="$(systemctl --system show "${controller_unit}" --property=SubState --value)"
  [[ "${controller_pid}" != 0 && "${controller_active}" == active && "${controller_substate}" == running ]] && break
  sleep 0.01
done
[[ "${controller_pid}" =~ ^[1-9][0-9]*$ ]]
[[ "${controller_active}" == active ]]
[[ "${controller_substate}" == running ]]
second_controller_invocation="$(systemctl --system show "${controller_unit}" --property=InvocationID --value)"
[[ "${second_controller_invocation}" =~ ^[0-9a-f]{32}$ ]]
[[ "${second_controller_invocation}" != "${first_controller_invocation}" ]]
sudo -n /usr/bin/systemctl --system start --no-block "${worker_unit}"
for _ in $(seq 1 100); do
  worker_pid="$(systemctl --system show "${worker_unit}" --property=MainPID --value)"
  [[ "${worker_pid}" != 0 ]] && break
  sleep 0.01
done
[[ "${worker_pid}" =~ ^[1-9][0-9]*$ ]]
worker_starttime="$(awk '{print $22}' "/proc/${worker_pid}/stat")"
[[ "${worker_starttime}" =~ ^[1-9][0-9]*$ ]]

# Death while both services are running must synchronously converge to a
# process-free worker invocation with no restart and no queued job.
sudo -n /usr/bin/systemctl --system kill --kill-whom=main --signal=KILL \
  "${controller_unit}"
for _ in $(seq 1 200); do
  worker_pid_after="$(systemctl --system show "${worker_unit}" --property=MainPID --value)"
  worker_active="$(systemctl --system show "${worker_unit}" --property=ActiveState --value)"
  if [[ "${worker_pid_after}" == 0 && "${worker_active}" != activating && "${worker_active}" != active ]]; then
    break
  fi
  sleep 0.01
done
[[ "${worker_pid_after}" == 0 ]]
[[ "${worker_active}" != activating && "${worker_active}" != active ]]
[[ "$(systemctl --system show "${worker_unit}" --property=NRestarts --value)" == 0 ]]
[[ -z "$(systemctl --system show "${worker_unit}" --property=Job --value)" ]]
[[ "$(systemctl --system show "${controller_unit}" --property=NRestarts --value)" == 0 ]]
[[ "$(systemctl --system show "${controller_unit}" --property=MainPID --value)" == 0 ]]
[[ -z "$(systemctl --system show "${controller_unit}" --property=Job --value)" ]]
if [[ -e "/proc/${worker_pid}/stat" ]]; then
  [[ "$(awk '{print $22}' "/proc/${worker_pid}/stat")" != "${worker_starttime}" ]]
fi

echo "generated controller BindsTo worker kill propagation passed"
