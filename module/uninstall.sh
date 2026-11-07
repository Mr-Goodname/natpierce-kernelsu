#!/system/bin/sh
MODDIR=${0%/*}
DATA=${NP_DATA:-/data/adb/natpierce-rs}
export NP_DATA="$DATA"
mkdir -p "$DATA"
printf 'stopped\n' >"$DATA/stop"
"$MODDIR/bin/npctl" stop >/dev/null 2>&1
# The supervisor can be asleep in its bounded backoff. Stop only the shell
# whose command line points to this exact module's watchdog, then wait briefly.
pid=$(cat "$DATA/run/watchdog.lock/pid" 2>/dev/null)
case "$pid" in
  ''|*[!0-9]*) ;;
  *)
    command_line=$(tr '\000' ' ' <"/proc/$pid/cmdline" 2>/dev/null)
    case "$command_line" in
      *"$MODDIR/bin/watchdog.sh"*) kill -TERM "$pid" 2>/dev/null ;;
    esac
    ;;
esac
remaining=8
while [ "$remaining" -gt 0 ]; do
  pid=$(cat "$DATA/run/daemon.pid" 2>/dev/null)
  [ -z "$pid" ] && break
  kill -0 "$pid" 2>/dev/null || break
  sleep 1
  remaining=$((remaining-1))
done
# Re-check PID/executable identity in Rust before cleaning. No firewall table
# flush or forwarding sysctl change is performed.
"$MODDIR/bin/npctl" cleanup >/dev/null 2>&1
# Keep the user's saved credentials and settings for reinstall.
