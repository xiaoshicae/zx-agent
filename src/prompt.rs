//! System prompt: instructions followed by generated sections (pi: coding-agent system-prompt.ts).
//!
//! Facts the model needs, such as the operating system and the command policy, are written
//! into the prompt at startup instead of being left for the model to discover: discovering
//! them costs turns, and an allowlist may forbid the commands that would reveal them.

use std::path::Path;

use crate::ai::{PREAMBLE, SystemPrompt, now_ms};
use crate::executor::Platform;

/// `instructions` as the preamble, followed by every non-empty section. Sections are wrapped
/// in tags of their names, so the model can tell generated facts from instructions
/// (pi: `buildSystemPromptSections`).
pub fn build_system_prompt(instructions: &str, sections: &[(&str, &str)]) -> SystemPrompt {
    sections.iter().fold(
        SystemPrompt::default().with_section(PREAMBLE, instructions),
        |prompt, (name, content)| prompt.with_section(name, content),
    )
}

/// Facts about the machine; Claude Code, Codex, and Gemini CLI give the model similar ones.
/// Host and user names are left out because the prompt goes to a third-party model.
///
/// The start time is part of the prompt, so a resumed session gets a new prompt prefix and
/// misses the provider's prompt cache on its first request.
pub fn environment_section(cwd: &Path) -> String {
    let os = format!("{} ({})", os_release(), std::env::consts::ARCH);
    format!(
        "OS: {os}\nRun started: {}\nWorking directory: {}",
        run_started(),
        cwd.display()
    )
}

/// Facts about the container that commands run in, instead of this machine: its system,
/// the read-only workspace, and the isolation that limits what commands can do.
pub fn container_environment_section(cwd: &Path, image: &str, user: &str, platform: &Platform) -> String {
    let name = pretty_name(&platform.os_release).unwrap_or_else(|| "Linux".to_string());
    format!(
        "OS: {name}, Linux kernel {} ({}), in a container from image {image}\nRun started: {}\n\
         Working directory: {} (read-only)\n\
         Isolation: commands run as user {user} without network access; only /tmp is writable.",
        platform.kernel,
        platform.arch,
        run_started(),
        cwd.display()
    )
}

/// The current UTC time, with this machine's time zone when it is known.
fn run_started() -> String {
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
fn pretty_name(os_release: &str) -> Option<String> {
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

/// Seconds since the Unix epoch as `YYYY-MM-DD HH:MM UTC`, using Howard Hinnant's
/// days-to-civil algorithm (the standard library has no calendar).
fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let (hour, minute) = (secs % 86_400 / 3600, secs % 3600 / 60);
    // Count from 0000-03-01 so that the leap day is the last day of a year.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
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
    fn environment_names_the_os_start_time_and_directory() {
        let section = environment_section(Path::new("/srv/app"));
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(lines.len(), 3, "{section}");
        assert!(lines[0].starts_with("OS: "), "{section}");
        assert!(
            lines[1].starts_with("Run started: ") && lines[1].contains(" UTC"),
            "{section}"
        );
        assert_eq!(lines[2], "Working directory: /srv/app");
    }

    #[test]
    fn the_container_environment_describes_the_container_and_its_isolation() {
        let platform = Platform {
            os_release: "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n".into(),
            kernel: "6.10.14-linuxkit".into(),
            arch: "aarch64".into(),
        };
        let section =
            container_environment_section(Path::new("/srv/app"), "debian:bookworm-slim", "65534:65534", &platform);
        let lines: Vec<&str> = section.lines().collect();
        assert_eq!(
            lines[0],
            "OS: Debian GNU/Linux 12 (bookworm), Linux kernel 6.10.14-linuxkit (aarch64), in a container from image debian:bookworm-slim"
        );
        assert!(lines[1].starts_with("Run started: "), "{section}");
        assert_eq!(lines[2], "Working directory: /srv/app (read-only)");
        assert_eq!(
            lines[3],
            "Isolation: commands run as user 65534:65534 without network access; only /tmp is writable."
        );
        let unnamed = Platform::default();
        assert!(
            container_environment_section(Path::new("/"), "x", "0:0", &unnamed).starts_with("OS: Linux, Linux kernel")
        );
    }

    #[test]
    fn reads_os_names_from_release_files() {
        let os_release = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(os_release).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        let plist = "<dict>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n\t<key>ProductVersion</key>\n\t<string>15.5</string>\n</dict>";
        assert_eq!(plist_string(plist, "ProductVersion"), Some("15.5"));
    }

    #[test]
    fn wraps_non_empty_sections_in_tags() {
        let prompt = build_system_prompt(
            "Do the task.\n",
            &[("environment", "OS: linux"), ("command_policy", "")],
        );
        assert_eq!(
            prompt.render(),
            "Do the task.\n\n<environment>\nOS: linux\n</environment>"
        );
    }
}
