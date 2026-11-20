#!/bin/sh
set -eu

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$project/target/release/tandem-mail"
environment="$project/target/tandem-launch.env"

if [ "${1:-}" = "--prepare" ] || [ "${1:-}" = "--build-and-open" ]; then
    cd "$project"
    cargo build --release --locked
    config=$(mktemp "$project/target/.tandem-launch.XXXXXX")
    printf '%s\n' "$LD_LIBRARY_PATH" "$TANDEM_MAIL_BOLD_FONT" > "$config"
    mv "$config" "$environment"
    if [ "$1" = "--prepare" ]; then exit 0; fi
    exec "$binary"
fi

if [ -z "${TANDEM_MAIL_DIR:-}" ] && [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] &&
    command -v hyprctl >/dev/null && command -v jq >/dev/null; then
    client=$(hyprctl clients -j 2>/dev/null | jq -c '
        first(.[] | select(.class == "io.tandem_mail.TandemMail" and .mapped)) // empty
    ' 2>/dev/null) || client=
    if [ -n "$client" ]; then
        window=$(printf '%s\n' "$client" | jq -r .address)
        workspace=$(printf '%s\n' "$client" | jq -r .workspace.name)
        if [ "${1:-}" = "--scratchpad" ] && [ "$workspace" != "special:email" ]; then
            hyprctl dispatch movetoworkspacesilent "special:email,address:$window" >/dev/null
            workspace=special:email
        fi
        case "$workspace" in
            special:*)
                if ! hyprctl monitors -j | jq -e --arg name "$workspace" '
                    any(.[]; .specialWorkspace.name == $name)
                ' >/dev/null; then
                    hyprctl dispatch togglespecialworkspace "${workspace#special:}" >/dev/null
                fi
                ;;
        esac
        if hyprctl dispatch focuswindow "address:$window" >/dev/null 2>&1; then
            exit 0
        fi
    fi
fi

if [ -x "$binary" ] && [ -r "$environment" ]; then
    {
        IFS= read -r LD_LIBRARY_PATH
        IFS= read -r TANDEM_MAIL_BOLD_FONT
    } < "$environment"
    runtime_ok=true
    saved_ifs=$IFS
    IFS=:
    for path in $LD_LIBRARY_PATH; do
        if [ ! -d "$path" ]; then runtime_ok=false; break; fi
    done
    IFS=$saved_ifs
    if [ "$runtime_ok" = true ] && [ -r "$TANDEM_MAIL_BOLD_FONT" ]; then
        export LD_LIBRARY_PATH TANDEM_MAIL_BOLD_FONT
        exec "$binary"
    fi
fi

cd "$project"
exec nix-shell "$project/shell.nix" --run 'exec ./scripts/launch.sh --build-and-open'
