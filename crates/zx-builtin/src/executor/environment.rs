//! What the `environment` section of a system prompt says about where commands run.
//!
//! Facts the model needs, such as the operating system and the date, are written into the
//! prompt when a session is opened instead of being left for the model to discover:
//! discovering them costs turns, and a policy may deny or ask for the commands that would
//! reveal them. [`machine_section`] describes this machine, for [`LocalExecutor`]; a container
//! describes itself with the same labels, time and OS name ([`current_time`], [`pretty_name`]).
//!
//! [`LocalExecutor`]: super::LocalExecutor

use std::path::Path;

use crate::ai::now_ms;
use crate::calendar::civil_from_days;

/// The label of the line that says what system commands run on.
pub(super) const OS_LABEL: &str = "OS: ";
/// The label of the line that says when the section was made.
pub(super) const OPENED_LABEL: &str = "Session opened: ";
/// The label of the line that names the working directory.
pub(super) const WORKING_DIRECTORY_LABEL: &str = "Working directory: ";

/// Facts about the machine; Claude Code, Codex, and Gemini CLI give the model similar ones.
/// Host and user names are left out because the prompt goes to a third-party model.
///
/// The section is made when the session is opened, and says so: the runs of a session that
/// stays open, as in a chat, come later. The time is part of the prompt, so a resumed session
/// gets a new prompt prefix and misses the provider's prompt cache on its first request.
pub(super) fn machine_section(cwd: &Path) -> String {
    let os = format!("{} ({})", os_release(), std::env::consts::ARCH);
    format!(
        "{OS_LABEL}{os}\n{OPENED_LABEL}{}\n{WORKING_DIRECTORY_LABEL}{}",
        current_time(),
        cwd.display()
    )
}

/// The current UTC time, with this machine's time zone when it is known, as an environment
/// section states it.
pub(super) fn current_time() -> String {
    let started = format_utc(now_ms() / 1000);
    match time_zone() {
        Some(zone) => format!("{started} (local time zone: {zone})"),
        None => started,
    }
}

/// The OS name and version, read from files rather than by running commands. Falls back to
/// the target OS name when the files are missing or unreadable.
fn os_release() -> String {
    // Read errors only make the description less specific, so they are dropped.
    let release = match std::env::consts::OS {
        "linux" => std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|text| pretty_name(&text))
            .map(|name| {
                let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
                match kernel {
                    Some(kernel) => format!("{name}, Linux kernel {}", kernel.trim()),
                    None => name,
                }
            }),
        "macos" => std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
            .ok()
            .and_then(|plist| {
                Some(format!(
                    "{} {}",
                    plist_string(&plist, "ProductName")?,
                    plist_string(&plist, "ProductVersion")?
                ))
            }),
        _ => None,
    };
    release.unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// `PRETTY_NAME` from an os-release file, such as `Ubuntu 24.04.1 LTS`.
pub(super) fn pretty_name(os_release: &str) -> Option<String> {
    let value = os_release.lines().find_map(|line| line.strip_prefix("PRETTY_NAME="))?;
    Some(value.trim().trim_matches('"').to_string())
}

/// The string value of `key` in an XML property list.
fn plist_string<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let (_, after_key) = plist.split_once(&format!("<key>{key}</key>"))?;
    let (_, after_open) = after_key.split_once("<string>")?;
    let (value, _) = after_open.split_once("</string>")?;
    Some(value.trim())
}

/// The IANA time zone name, such as `Asia/Shanghai`, from `TZ` or the `/etc/localtime` link.
fn time_zone() -> Option<String> {
    // An unset or non-Unicode `TZ` falls back to the system setting below.
    let from_env = std::env::var("TZ").ok().filter(|zone| !zone.trim().is_empty());
    if let Some(zone) = from_env {
        return Some(zone.trim_start_matches(':').to_string());
    }
    // No link (or not a link) means the zone is unknown; the prompt then omits it.
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let target = target.to_string_lossy();
    let (_, zone) = target.split_once("zoneinfo/")?;
    Some(zone.to_string())
}

/// Seconds since the Unix epoch as `YYYY-MM-DD HH:MM UTC`.
fn format_utc(secs: u64) -> String {
    let (hour, minute) = (secs % 86_400 / 3600, secs % 3600 / 60);
    let (year, month, day) = civil_from_days(secs / 86_400);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_times_including_leap_days() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn environment_names_the_os_the_time_the_session_was_opened_and_the_directory() {
        let section = machine_section(Path::new("/srv/app"));
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(lines.len(), 3, "{section}");
        assert!(lines[0].starts_with("OS: "), "{section}");
        assert!(
            lines[1].starts_with("Session opened: ") && lines[1].contains(" UTC"),
            "{section}"
        );
        assert_eq!(lines[2], "Working directory: /srv/app");
    }

    #[test]
    fn reads_os_names_from_release_files() {
        let os_release = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(os_release).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        let plist = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.5</string>\n</dict>";
        assert_eq!(plist_string(plist, "ProductVersion"), Some("15.5"));
    }
}
