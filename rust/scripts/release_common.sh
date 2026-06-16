#!/bin/bash

repo_name_from_remote_url() {
    local url="${1:-}"
    if [ -z "$url" ]; then
        return 1
    fi

    url="${url%/}"
    url="${url%.git}"

    case "$url" in
        *://*/*|git@*:*/*)
            printf '%s\n' "${url##*/}"
            ;;
        *)
            return 1
            ;;
    esac
}

infer_repo_name() {
    local repo_dir="$1"
    local remote url name top

    for remote in origin github upstream; do
        url="$(git -C "$repo_dir" config --get "remote.${remote}.url" 2>/dev/null || true)"
        name="$(repo_name_from_remote_url "$url" || true)"
        if [ -n "$name" ]; then
            printf '%s\n' "$name"
            return 0
        fi
    done

    top="$(git -C "$repo_dir" rev-parse --show-toplevel 2>/dev/null || printf '%s\n' "$repo_dir")"
    basename "$top"
}

current_npub() {
    local user_output

    user_output="$(htree user 2>&1 || true)"
    printf '%s\n' "$user_output" \
        | grep -oE 'npub1[023456789acdefghjklmnpqrstuvwxyz]+' \
        | head -n1
}

urlencode_path_segment() {
    local input="$1"
    local output=""
    local i ch

    LC_ALL=C
    for ((i = 0; i < ${#input}; i++)); do
        ch="${input:i:1}"
        case "$ch" in
            [a-zA-Z0-9.~_-])
                output+="$ch"
                ;;
            *)
                printf -v output '%s%%%02X' "$output" "'$ch"
                ;;
        esac
    done

    printf '%s\n' "$output"
}

urlencode_path() {
    local input="$1"
    local output=""
    local segment
    local first=1

    IFS='/' read -r -a segments <<<"$input"
    for segment in "${segments[@]}"; do
        if [ "$first" -eq 1 ]; then
            first=0
        else
            output+="/"
        fi
        output+="$(urlencode_path_segment "$segment")"
    done

    printf '%s\n' "$output"
}

gateway_tree_base_url() {
    local npub="$1"
    local tree_name="$2"
    printf '%s/%s/%s\n' "$(release_upload_server_url)" "$npub" "$(urlencode_path_segment "$tree_name")"
}

release_upload_server_url() {
    printf '%s\n' "${HTREE_RELEASE_UPLOAD_SERVER:-https://upload.iris.to}"
}

gateway_release_base_url() {
    local npub="$1"
    local tree_name="$2"
    local version_path="$3"
    printf '%s/%s\n' \
        "$(gateway_tree_base_url "$npub" "$tree_name")" \
        "$(urlencode_path "$version_path")"
}
