#!/system/bin/sh
umask 077
MODDIR=${0%/bin/*}
DATA=${NP_DATA:-/data/adb/natpierce-rs}
mkdir -p "$DATA/run" "$DATA/logs" || exit 1
chmod 700 "$DATA" "$DATA/run" "$DATA/logs" || exit 1
printf '%s\n' -1000 2>/dev/null >"/proc/$$/oom_score_adj" || :
GUARD="$DATA/run/watchdog.guard"
# An older module version had only a directory/PID lock. Keep a live legacy
# supervisor during an in-place upgrade until it is stopped or the phone reboots.
if [ ! -f "$GUARD" ]; then
  pid=$(cat "$DATA/run/watchdog.lock/pid" 2>/dev/null)
  case "$pid" in
    ''|*[!0-9]*) ;;
    *)
      if [ "$pid" != "$$" ] && kill -0 "$pid" 2>/dev/null; then
        command_line=$(tr '\000' ' ' <"/proc/$pid/cmdline" 2>/dev/null)
        case "$command_line" in *"$MODDIR/bin/watchdog.sh"*) exit 0 ;; esac
      fi
      ;;
  esac
fi
# The kernel lock is acquired before publishing the PID; a competing startup
# cannot mistake that initialization window for an abandoned owner. Keep the
# guard file itself in place so all startups always lock the same inode.
exec 9>"$GUARD" || exit 1
chmod 600 "$GUARD" 9>&- || exit 1
lock_supervisor() {
  if [ -x /system/bin/flock ]; then
    # Android mksh closes exec-created descriptors for external commands unless
    # the descriptor is explicitly passed in that command's redirections.
    /system/bin/flock -n 9 9>&9
  elif command -v flock >/dev/null 2>&1; then
    flock -n 9 9>&9
  else
    for busybox in /data/adb/ksu/bin/busybox /data/adb/ksu/busybox; do
      if [ -x "$busybox" ]; then "$busybox" flock -n 9 9>&9; return $?; fi
    done
    printf '%s\n' 'No flock implementation is available; supervisor not started.' >>"$DATA/logs/supervisor.log"
    return 127
  fi
}
lock_supervisor 2>>"$DATA/logs/supervisor.log"
lock_status=$?
if [ "$lock_status" -ne 0 ]; then
  [ "$lock_status" -eq 1 ] && exit 0
  printf 'Supervisor lock failed: %s\n' "$lock_status" >>"$DATA/logs/supervisor.log"
  exit "$lock_status"
fi
mkdir -p "$DATA/run/watchdog.lock" || exit 1
echo $$ >"$DATA/run/watchdog.lock/pid"
trap 'if [ "$(cat "$DATA/run/watchdog.lock/pid" 9>&- 2>/dev/null)" = "$$" ]; then rm -f "$DATA/run/watchdog.lock/pid" 9>&-; rmdir "$DATA/run/watchdog.lock" 9>&- 2>/dev/null; fi' EXIT
delay=2
while [ ! -f "$DATA/stop" ] && [ ! -f "$DATA/safe_mode" ] && [ ! -f "$MODDIR/disable" ] && [ ! -f "$MODDIR/remove" ]; do
  started=$(date +%s 9>&-)
  "$MODDIR/bin/npctl" daemon 9>&- >>"$DATA/logs/supervisor.log" 2>&1
  elapsed=$(($(date +%s 9>&-)-started))
  [ "$elapsed" -gt 60 ] && delay=2
  [ -f "$DATA/stop" ] && break
  sleep "$delay" 9>&-
  delay=$((delay*2))
  [ "$delay" -gt 60 ] && delay=60
done
