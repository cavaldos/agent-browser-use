#!/bin/sh
# Run from source: viewer http://localhost:3000, MCP http://localhost:3000/mcp
# Extra flags are passed through, e.g. scripts/run.sh -port 8080
set -e
ROOT=$(cd "$(dirname "$0")/.." && pwd)

# Colors only on a terminal (and not when NO_COLOR is set).
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RST=$(printf '\033[0m'); C_BOLD=$(printf '\033[1m'); C_DIM=$(printf '\033[2m')
    C_CYAN=$(printf '\033[36m'); C_GREEN=$(printf '\033[32m'); C_RED=$(printf '\033[31m')
else
    C_RST=""; C_BOLD=""; C_DIM=""; C_CYAN=""; C_GREEN=""; C_RED=""
fi

version_info() {
    v=$(git -C "$ROOT" describe --tags --always --dirty 2>/dev/null || echo dev)
    echo "agent-browser-use $v (source)"
}

# Update a source checkout: fast-forward from origin. Installed binaries: scripts/install.sh.
do_update() {
    if [ ! -d "$ROOT/.git" ]; then
        echo "Not a git checkout. To update an installed binary run: scripts/install.sh"
        exit 1
    fi
    before=$(git -C "$ROOT" rev-parse --short HEAD)
    git -C "$ROOT" pull --ff-only || { echo "${C_RED}Update failed${C_RST} (local changes or diverged branch?)"; exit 1; }
    after=$(git -C "$ROOT" rev-parse --short HEAD)
    if [ "$before" = "$after" ]; then
        echo "${C_GREEN}Already up to date${C_RST} ($after)"
    else
        echo "${C_GREEN}Updated${C_RST} $before -> $after"
    fi
    exit 0
}

print_help() {
    cat <<'EOF'
Usage: ./run [-p PORT] [app flags]
       make run [p=PORT] [pw=PASSWORD] [-- app flags]

The app starts immediately (any server already on the port is replaced), prints
its URLs (with -tunnel: public domain and password), and shows a menu:
  Restart            restart the app
  Run in background  leave it running and return to the shell
  Terminate          stop the app and exit
Leave the menu open and the app just keeps running; Ctrl+C terminates it.

Commands:
  -h, --help
      Show this help.
  -v, --version
      Print the version.
  -u, --update
      Update this checkout (git pull --ff-only).

Examples:
  ./run
  make run p=8080
  ./run -h
  ./run --help
  ./bin/agent-browser-use -h      # all Go app flags
EOF
}

port=3000
user_pw=""
prev=""
for arg in "$@"; do
    case "$prev" in
        -p|-port) port="$arg" ;;
        -pw|--pw) user_pw="$arg" ;;
    esac
    case "$arg" in
        -h|--help|help)
            print_help
            exit 0
            ;;
        -v|--version|-version)
            version_info
            exit 0
            ;;
        -u|--update|-update)
            do_update
            ;;
        -p=*|-port=*) port="${arg#*=}" ;;
    esac
    prev="$arg"
done

cd "$ROOT/app"

pids_on_port() {
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null || true
    elif command -v ss >/dev/null 2>&1; then
        ss -ltnp "sport = :$1" 2>/dev/null | sed -n '2,$p' | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u || true
    fi
}

# PIDs of any instance of this app (any port) that uses this project's Chrome profile.
pids_of_instances() {
    ps -axo pid=,command= 2>/dev/null |
        grep -F -- "-profile $ROOT/chrome-profile" |
        grep -v grep |
        awk '{print $1}' |
        grep -vx "$$" || true
}

all_pids() {
    printf '%s\n%s\n' "$(pids_on_port "$port")" "$(pids_of_instances)" | grep -v '^$' | sort -u || true
}

# Politely stop the old server (same port, or any instance sharing our Chrome profile):
# SIGTERM, then SIGKILL after 5s.
stop_existing() {
    pids=$(all_pids)
    [ -z "$pids" ] && return 0
    for pid in $pids; do
        [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true
    done
    n=0
    while [ "$n" -lt 20 ]; do
        [ -z "$(all_pids)" ] && return 0
        sleep 0.25
        n=$((n + 1))
    done
    for pid in $(all_pids); do
        kill -9 "$pid" 2>/dev/null || true
    done
    sleep 0.5
    return 0
}

# Show what is already listening on the port, so the user knows what "Terminate" will stop.
show_status() {
    echo "A server is already running on port $port:"
    for pid in $(pids_on_port "$port"); do
        up=$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ' || true)
        name=$(ps -o comm= -p "$pid" 2>/dev/null | sed 's#.*/##' || true)
        echo "  PID ${pid}  ${name:-?}  up ${up:-?}"
    done
    echo "  Local:      http://localhost:$port"
    echo "  MCP:        http://localhost:$port/mcp"
    # The tunnel API reports the public URL only while the tunnel is on.
    pub=$(curl -s -m 2 "http://127.0.0.1:$port/api/tunnel" 2>/dev/null | grep -o '"url":"[^"]*"' | head -n 1 | cut -d'"' -f4 || true)
    if [ -n "$pub" ]; then
        echo "  Public:     $pub"
        echo "  Public MCP: $pub/mcp"
    fi
    echo
}

interactive() {
    [ -z "${AGENT_BROWSER_USE_NO_MENU:-}" ] && [ -t 0 ] && [ -t 1 ] && [ -r /dev/tty ] && [ -w /dev/tty ]
}

saved_tty=""

restore_tty() {
    [ -n "$saved_tty" ] && stty "$saved_tty" </dev/tty 2>/dev/null || true
    printf '\033[?25h'
}

read_byte() {
    dd if=/dev/tty bs=1 count=1 2>/dev/null |
    od -An -tx1 | tr -d ' \n'
}

# Returns: 0=up, 1=down, 2=enter, 3=quit, 4=other
read_key() {
    key=$(read_byte)
    case "$key" in
        0d|0a|04) return 2 ;;
        03) return 3 ;;
        71|51) return 4 ;;
        1b)
            stty -echo -icanon min 0 time 5 </dev/tty
            bracket=$(read_byte)
            case "$bracket" in
                "") stty -echo -icanon min 1 time 0 </dev/tty; return 4 ;;
                5b|4f)
                    key=$(read_byte)
                    stty -echo -icanon min 1 time 0 </dev/tty
                    case "$key" in
                        41) return 0 ;;
                        42) return 1 ;;
                        *) return 4 ;;
                    esac
                    ;;
                *) stty -echo -icanon min 1 time 0 </dev/tty; return 4 ;;
            esac
            ;;
        *) return 4 ;;
    esac
}

ITEMS="restart background terminate"

label_of() {
    case "$1" in
        restart) echo "Restart" ;;
        background) echo "Run in background" ;;
        terminate) echo "Terminate" ;;
    esac
}

# Entry at index $1 of $ITEMS (subshell, so the script's own "$@" is untouched).
item_at() (
    idx=$1
    set -- $ITEMS
    shift "$idx"
    echo "$1"
)

draw_menu() {
    if [ "$menu_drawn" = 1 ]; then
        printf '\033[%dA' "$menu_lines"
    fi
    menu_drawn=1
    printf '\033[2K%s%sServer is running%s %s(port %s)%s\r\n' "$C_BOLD" "$C_CYAN" "$C_RST" "$C_DIM" "$port" "$C_RST"
    i=0
    for key in $ITEMS; do
        label=$(label_of "$key")
        if [ "$i" = "$1" ]; then
            printf '\033[2K  %s%s❯ %-18s%s\r\n' "$C_BOLD" "$C_GREEN" "$label" "$C_RST"
        else
            printf '\033[2K    %-18s\r\n' "$label"
        fi
        i=$((i + 1))
    done
    printf '\033[2K%s  ↑↓ move · Enter select · Ctrl+C terminate%s' "$C_DIM" "$C_RST"
}

log="$ROOT/agent-browser-use.log"
app_pid=""

want_tunnel=0
for arg in "$@"; do
    case "$arg" in
        -tunnel|--tunnel|-tunnel=true|--tunnel=true) want_tunnel=1 ;;
    esac
done

# Start the app detached from the terminal; output goes to the log.
start_app() {
    : >"$log"
    AGENT_BROWSER_USE_NO_MENU=1 nohup go run . -profile "$ROOT/chrome-profile" "$@" >"$log" 2>&1 </dev/null &
    app_pid=$!
}

# Wait until the app printed its URLs (and the tunnel URL, when -tunnel is on).
wait_ready() {
    n=0
    while [ "$n" -lt 240 ]; do
        if ! kill -0 "$app_pid" 2>/dev/null; then
            cat "$log"
            echo "The app exited."
            return 1
        fi
        if grep -q "is running" "$log"; then
            if [ "$want_tunnel" = 0 ] || grep -q "Public URL\|Tunnel failed" "$log"; then
                break
            fi
        fi
        sleep 0.25
        n=$((n + 1))
    done
    cat "$log"
    if [ "$want_tunnel" = 1 ] && grep -q "Public URL" "$log" && ! grep -q "Password (generated" "$log" && [ -n "$user_pw" ]; then
        echo "  Password: $user_pw"
    fi
    echo
}

terminate_app() {
    stop_existing
    [ -n "$app_pid" ] && kill "$app_pid" 2>/dev/null || true
}

choose_with_arrows() {
    count=$(set -- $ITEMS; echo $#)
    menu_lines=$((count + 1))
    saved_tty=$(stty -g </dev/tty 2>/dev/null) || saved_tty=""
    stty -echo -icanon min 1 time 0 </dev/tty
    printf '\033[?25l'
    trap 'restore_tty; printf "\n"; terminate_app; echo "Terminated."; exit 0' INT TERM HUP
    selected=0
    menu_drawn=0
    while true; do
        draw_menu "$selected"
        if read_key; then
            status=0
        else
            status=$?
        fi
        case "$status" in
            0)
                selected=$((selected - 1))
                [ "$selected" -lt 0 ] && selected=$((count - 1))
                ;;
            1)
                selected=$((selected + 1))
                [ "$selected" -ge "$count" ] && selected=0
                ;;
            2)
                restore_tty
                printf '\033[%dA\033[J' "$menu_lines"
                case "$(item_at "$selected")" in
                    restart)
                        terminate_app
                        start_app "$@"
                        wait_ready || exit 1
                        stty -echo -icanon min 1 time 0 </dev/tty
                        printf '\033[?25l'
                        menu_drawn=0
                        ;;
                    background)
                        trap - INT TERM HUP
                        echo "Running in background (log: $log). Stop it with: ./run, then Terminate."
                        exit 0
                        ;;
                    terminate)
                        trap - INT TERM HUP
                        terminate_app
                        echo "Terminated the server on port $port."
                        exit 0
                        ;;
                esac
                ;;
        esac
    done
}

stop_existing

if interactive; then
    start_app "$@"
    wait_ready || exit 1
    choose_with_arrows "$@"
else
    exec go run . -profile "$ROOT/chrome-profile" "$@"
fi
