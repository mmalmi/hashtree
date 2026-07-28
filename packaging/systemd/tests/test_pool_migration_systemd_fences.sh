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
template_unit="hashtree-pool-migrate-fence-probe-${suffix}@.service"
instance_unit="hashtree-pool-migrate-fence-probe-${suffix}@preloaded.service"
installed_template="/etc/systemd/system/${template_unit}"
runtime_template_mask="/run/systemd/system/${template_unit}"
runtime_instance_mask="/run/systemd/system/${instance_unit}"
generated_fragment="$(mktemp)"

cleanup() {
  sudo -n /usr/bin/systemctl --system stop "${instance_unit}" >/dev/null 2>&1 || true
  sudo -n /usr/bin/rm -f -- \
    "${runtime_instance_mask}" \
    "${runtime_template_mask}" \
    "${installed_template}"
  sudo -n /usr/bin/systemctl --system daemon-reload
  sudo -n /usr/bin/systemctl --system reset-failed "${instance_unit}" >/dev/null 2>&1 || true
  /bin/rm -f -- "${generated_fragment}"
}
trap cleanup EXIT

printf '%s\n' \
  '[Unit]' \
  'Description=Generated Pool migration fence probe (%i)' \
  '[Service]' \
  'Type=oneshot' \
  'ExecStart=/bin/true' \
  >"${generated_fragment}"
sudo -n /usr/bin/install -o root -g root -m 0644 \
  "${generated_fragment}" "${installed_template}"
sudo -n /usr/bin/systemctl --system daemon-reload

# Force systemd to instantiate and cache the generated unit before the
# activation template is masked.
systemctl --system show "${instance_unit}" \
  --property=LoadState \
  --property=FragmentPath |
  grep -Fx "LoadState=loaded" >/dev/null
systemctl --system show "${instance_unit}" \
  --property=FragmentPath |
  grep -Fx "FragmentPath=${installed_template}" >/dev/null
sudo -n /usr/bin/systemctl --system start "${instance_unit}"

sudo -n /usr/bin/ln -s /dev/null "${runtime_template_mask}"
sudo -n /usr/bin/systemctl --system daemon-reload

# systemd retains the already instantiated unit's loaded fragment even after a
# daemon-reloaded template mask. Prove that this legacy instance remains
# startable: the template mask alone is not an activation fence.
systemctl --system show "${instance_unit}" \
  --property=LoadState |
  grep -Fx "LoadState=loaded" >/dev/null
systemctl --system show "${instance_unit}" \
  --property=FragmentPath |
  grep -Fx "FragmentPath=${installed_template}" >/dev/null
if ! sudo -n /usr/bin/systemctl --system start "${instance_unit}" >/dev/null 2>&1; then
  echo "preloaded instance behavior changed: template mask now inhibited start" >&2
  exit 1
fi
sudo -n /usr/bin/systemctl --system stop "${instance_unit}"

sudo -n /usr/bin/ln -s /dev/null "${runtime_instance_mask}"
sudo -n /usr/bin/systemctl --system daemon-reload
systemctl --system show "${instance_unit}" \
  --property=LoadState \
  --property=UnitFileState \
  --property=ActiveState \
  --property=SubState \
  --property=MainPID \
  --property=ControlPID \
  --property=Job \
  --property=NeedDaemonReload \
  --property=FragmentPath |
  grep -Fx "LoadState=masked" >/dev/null
systemctl --system show "${instance_unit}" --property=UnitFileState |
  grep -Fx "UnitFileState=masked-runtime" >/dev/null
systemctl --system show "${instance_unit}" --property=ActiveState |
  grep -Fx "ActiveState=inactive" >/dev/null
systemctl --system show "${instance_unit}" --property=SubState |
  grep -Fx "SubState=dead" >/dev/null
systemctl --system show "${instance_unit}" --property=MainPID |
  grep -Fx "MainPID=0" >/dev/null
systemctl --system show "${instance_unit}" --property=ControlPID |
  grep -Fx "ControlPID=0" >/dev/null
systemctl --system show "${instance_unit}" --property=Job |
  grep -Fx "Job=" >/dev/null
systemctl --system show "${instance_unit}" --property=NeedDaemonReload |
  grep -Fx "NeedDaemonReload=no" >/dev/null
systemctl --system show "${instance_unit}" --property=FragmentPath |
  grep -Fx "FragmentPath=${runtime_instance_mask}" >/dev/null

loaded="$(
  systemctl --system --no-pager --plain --no-legend list-units \
    --all --type=service "${template_unit/@.service/@*.service}" |
    awk '{print $1}'
)"
[[ -z "${loaded}" ]]
unit_files="$(
  systemctl --system --no-pager --plain --no-legend list-unit-files \
    "${template_unit/@.service/@*.service}" |
    awk '{print $1}' |
    sort
)"
grep -Fx "${instance_unit}" <<<"${unit_files}" >/dev/null
grep -Fx "${template_unit}" <<<"${unit_files}" >/dev/null

echo "generated template-mask and preloaded-instance-mask lifecycle passed"
