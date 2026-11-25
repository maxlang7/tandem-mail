#!/bin/sh
set -eu

project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary="$project/target/release/tandem-mail"
environment="$project/target/tandem-launch.env"

run_tandem() {
    # With compositor scaling disabled, XWayland exposes physical pixels.
    # Use the monitor's Wayland scale instead of letting winit infer it from DPI.
    if [ -z "${WINIT_X11_SCALE_FACTOR:-}" ] &&
        [ -n "${HYPRLAND_INSTANCE_SIGNATURE:-}" ] &&
        [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${DISPLAY:-}" ] &&
        command -v hyprctl >/dev/null && command -v jq >/dev/null &&
        hyprctl -j getoption xwayland:force_zero_scaling 2>/dev/null |
            jq -e '.int == 1' >/dev/null 2>&1 &&
        tandem_scale=$(hyprctl monitors -j 2>/dev/null |
            jq -er 'first(.[] | select(.focused) | .scale | numbers | select(. > 0))' 2>/dev/null)
    then
        export WINIT_X11_SCALE_FACTOR="$tandem_scale"
    fi
    exec "$binary"
}

if [ "${1:-}" = "--prepare" ] || [ "${1:-}" = "--build-and-open" ]; then
    cd "$project"
    cargo build --release --locked
    config=$(mktemp "$project/target/.tandem-launch.XXXXXX")
    printf '%s\n' "$LD_LIBRARY_PATH" "$TANDEM_MAIL_BOLD_FONT" > "$config"
    mv "$config" "$environment"
    if [ "$1" = "--prepare" ]; then exit 0; fi
    run_tandem
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
        run_tandem
    fi
fi

cd "$project"
exec nix-shell "$project/shell.nix" --run 'exec ./scripts/launch.sh --build-and-open'
