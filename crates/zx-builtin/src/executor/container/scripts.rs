//! The shell scripts that run inside the container, and the exit codes they answer with.

/// Exit codes of the start-up probe for an unusable container; other failures are retried.
pub(super) const PROBE_ROOT: i32 = 90;
pub(super) const PROBE_MISSING: i32 = 91;
pub(super) const PROBE_NO_WORKSPACE: i32 = 93;
pub(super) const PROBE_READ_ONLY_WORKSPACE: i32 = 98;

/// Checks that commands run as a non-root user, that the image has the required programs
/// and that the user can read the workspace, and write it when it is mounted writable; prints
/// the platform. Arguments: the shell commands run with, the workspace, and `rw` or `ro`.
pub(super) const PROBE: &str = r#"shell=$1 workspace=$2 access=$3
uid=
while read -r key value rest; do
  [ "$key" = "Uid:" ] && uid=$value
done < /proc/self/status
[ -n "$uid" ] && [ "$uid" != 0 ] || { echo "commands must not run as root (uid ${uid:-unknown})" >&2; exit 90; }
missing=
for program in "$shell" cat head uname mkdir; do
  command -v "$program" >/dev/null 2>&1 || missing="$missing $program"
done
[ -z "$missing" ] || { echo "the image lacks:$missing" >&2; exit 91; }
{ [ -r "$workspace" ] && [ -x "$workspace" ]; } || exit 93
[ "$access" = ro ] || [ -w "$workspace" ] || exit 98
echo "arch=$(uname -m)"
echo "kernel=$(uname -r)"
echo "--- os-release"
cat /etc/os-release 2>/dev/null
exit 0"#;

/// Becomes the shell. Arguments: the shell and the command.
pub(super) const RUN: &str = r#"exec "$1" -c "$2""#;

/// Kills every process of the user it runs as, except itself (kill(2) with pid -1). Run as the
/// command user, this stops a command's whole tree, including children in another session.
/// `-s KILL -- -1` is the form Debian's dash accepts.
pub(super) const SWEEP: &str = r#"kill -s KILL -- -1 2>/dev/null
exit 0"#;

/// Exit codes of the read and write scripts for each refusal.
pub(super) const EXIT_DIRECTORY: i32 = 94;
pub(super) const EXIT_MISSING: i32 = 95;
pub(super) const EXIT_SPECIAL: i32 = 96;
pub(super) const EXIT_DENIED: i32 = 97;

/// Refuses directories and special files, creates missing parent directories, then replaces
/// the file's contents with its standard input. Arguments: the absolute path.
pub(super) const WRITE: &str = r#"case $1 in */?*) dir=${1%/*}; [ -z "$dir" ] || [ -d "$dir" ] || mkdir -p -- "$dir" || exit 99 ;; esac
if [ -d "$1" ]; then exit 94; fi
if [ -e "$1" ] && [ ! -f "$1" ]; then exit 96; fi
exec cat > "$1""#;

/// Exit code of the write script when it cannot create a parent directory.
pub(super) const EXIT_NO_DIRECTORY: i32 = 99;

/// Refuses directories, missing, special and unreadable files; then reads a bounded prefix.
/// Arguments: the path and the byte limit.
pub(super) const READ: &str = r#"if [ -d "$1" ]; then exit 94; fi
if [ ! -e "$1" ]; then exit 95; fi
if [ ! -f "$1" ]; then exit 96; fi
if [ ! -r "$1" ]; then exit 97; fi
exec head -c "$2" -- "$1""#;
