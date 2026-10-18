//! Service definitions for each OS, generated as text so tests can check them anywhere.

use std::fmt::Write;
use std::path::PathBuf;

/// Task Scheduler task name (Windows).
pub const TASK_NAME: &str = "emusync";
/// LaunchAgent label (macOS).
pub const LAUNCHD_LABEL: &str = "io.github.ilyalissoboi.emusync";
/// systemd user unit (Linux).
pub const SYSTEMD_UNIT: &str = "emusync.service";
/// How long the service manager waits for the final sync after asking the watcher to stop.
pub const STOP_TIMEOUT_SECS: u64 = 90;

/// The command a service runs: `emusync[w] --data-dir … watch …`.
#[derive(Debug, Clone)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Command {
    /// The command line as a Windows program would see it.
    pub fn display(&self) -> String {
        std::iter::once(windows_quote(&self.program.display().to_string()))
            .chain(self.args.iter().map(|a| windows_quote(a)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Quotes one argument so `CommandLineToArgvW` (and the Rust runtime) read it back unchanged.
pub fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// Quotes one word of a systemd `ExecStart=` line. `%` (specifiers) and `$` (variables) are
/// escaped everywhere, since systemd expands them inside quotes too.
pub fn systemd_quote(arg: &str) -> String {
    let escaped = arg.replace('%', "%%").replace('$', "$$");
    let plain = !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ';'));
    if plain {
        return escaped;
    }
    format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `~/.config/systemd/user/emusync.service`. Restarts after a crash, not after a clean stop.
pub fn systemd_unit(cmd: &Command) -> String {
    let exec = std::iter::once(cmd.program.display().to_string())
        .chain(cmd.args.iter().cloned())
        .map(|a| systemd_quote(&a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "\
[Unit]
Description=emusync: sync emulator memory cards (emusync watch)
Documentation=https://github.com/ilyalissoboi/emusync

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec=30
# On stop, the watcher finishes its cycle and does a final sync.
TimeoutStopSec={STOP_TIMEOUT_SECS}

[Install]
WantedBy=default.target
"
    )
}

/// `~/Library/LaunchAgents/<label>.plist`. Starts at login, restarts after a crash, not after
/// a clean stop.
pub fn launchd_plist(cmd: &Command) -> String {
    let mut args = String::new();
    for a in std::iter::once(cmd.program.display().to_string()).chain(cmd.args.iter().cloned()) {
        let _ = writeln!(args, "\t\t<string>{}</string>", xml_escape(&a));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCHD_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{args}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ThrottleInterval</key>
	<integer>30</integer>
	<key>ExitTimeOut</key>
	<integer>{STOP_TIMEOUT_SECS}</integer>
</dict>
</plist>
"#
    )
}

/// A Task Scheduler task that runs at `user`'s logon with their normal rights (so no admin is
/// needed to register it), on battery too, without a time limit, restarting after a crash.
pub fn task_xml(user: &str, cmd: &Command) -> String {
    let user = xml_escape(user);
    let command = xml_escape(&windows_quote(&cmd.program.display().to_string()));
    let args = xml_escape(&cmd.args.iter().map(|a| windows_quote(a)).collect::<Vec<_>>().join(" "));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>emusync: sync emulator memory cards (emusync watch)</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// Task Scheduler reads XML files as UTF-16 (little-endian, with a byte order mark).
pub fn utf16_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(program: &str) -> Command {
        Command {
            program: PathBuf::from(program),
            args: vec![
                "--data-dir".into(),
                "/home/me/My Saves/50%".into(),
                "watch".into(),
                "--env-file".into(),
                r#"/home/me/"quoted" & <tagged>$HOME"#.into(),
            ],
        }
    }

    /// Splits a command line the way `CommandLineToArgvW` does (for arguments after the
    /// program name).
    fn windows_split(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut started = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\\' => {
                    let n = chars[i..].iter().take_while(|&&c| c == '\\').count();
                    i += n;
                    if chars.get(i) == Some(&'"') {
                        cur.push_str(&"\\".repeat(n / 2));
                        if n % 2 == 1 {
                            cur.push('"');
                            i += 1;
                        }
                    } else {
                        cur.push_str(&"\\".repeat(n));
                    }
                    started = true;
                    continue;
                }
                '"' => {
                    in_quotes = !in_quotes;
                    started = true;
                }
                ' ' | '\t' if !in_quotes => {
                    if started {
                        args.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                c => {
                    cur.push(c);
                    started = true;
                }
            }
            i += 1;
        }
        if started {
            args.push(cur);
        }
        args
    }

    #[test]
    fn windows_quoting_round_trips() {
        let cases = ["plain", "", "a b", r#"say "hi""#, r"C:\dir\", r"C:\my dir\", r#"x\"y"#, r"a\\b c", "tab\there"];
        for c in cases {
            let q = windows_quote(c);
            assert_eq!(windows_split(&q), [c], "{c:?} quoted as {q}");
        }
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote(r"C:\my dir\"), r#""C:\my dir\\""#);
    }

    #[test]
    fn systemd_unit_quotes_and_escapes() {
        let unit = systemd_unit(&cmd("/opt/emu sync/emusync"));
        let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart=")).unwrap();
        assert_eq!(
            exec,
            r#""/opt/emu sync/emusync" --data-dir "/home/me/My Saves/50%%" watch --env-file "/home/me/\"quoted\" & <tagged>$$HOME""#
        );
        for line in ["Restart=on-failure", "TimeoutStopSec=90", "WantedBy=default.target", "Type=simple"] {
            assert!(unit.lines().any(|l| l == line), "{line} missing:\n{unit}");
        }
        assert_eq!(systemd_quote("plain-arg"), "plain-arg");
        assert_eq!(systemd_quote(""), "\"\"");
        assert_eq!(systemd_quote(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn launchd_plist_lists_each_argument() {
        let plist = launchd_plist(&cmd("/Applications/emusync/emusync"));
        assert!(plist.contains(&format!("<string>{LAUNCHD_LABEL}</string>")));
        let args: Vec<&str> = plist
            .lines()
            .skip_while(|l| !l.contains("<array>"))
            .skip(1)
            .take_while(|l| !l.contains("</array>"))
            .map(|l| l.trim().strip_prefix("<string>").unwrap().strip_suffix("</string>").unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "/Applications/emusync/emusync",
                "--data-dir",
                "/home/me/My Saves/50%",
                "watch",
                "--env-file",
                "/home/me/&quot;quoted&quot; &amp; &lt;tagged&gt;$HOME"
            ]
        );
        for key in
            ["<key>RunAtLoad</key>\n\t<true/>", "<key>SuccessfulExit</key>\n\t\t<false/>", "<integer>90</integer>"]
        {
            assert!(plist.contains(key), "{key} missing:\n{plist}");
        }
    }

    #[test]
    fn task_xml_runs_at_this_users_logon() {
        let xml = task_xml(r"AzureAD\Me & Co", &cmd(r"C:\Program Files\emusync\emusyncw.exe"));
        assert_eq!(xml.matches(r"<UserId>AzureAD\Me &amp; Co</UserId>").count(), 2, "trigger and principal");
        assert!(xml.contains(r"<Command>&quot;C:\Program Files\emusync\emusyncw.exe&quot;</Command>"));
        let args = xml.lines().find_map(|l| l.trim().strip_prefix("<Arguments>")).unwrap();
        let args = args.strip_suffix("</Arguments>").unwrap();
        let unescaped = args.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
        assert_eq!(windows_split(&unescaped), cmd("x").args);
        for tag in [
            "<LogonType>InteractiveToken</LogonType>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
        ] {
            assert!(xml.contains(tag), "{tag} missing:\n{xml}");
        }
        let bytes = utf16_with_bom("<a/>");
        assert_eq!(bytes, [0xFF, 0xFE, b'<', 0, b'a', 0, b'/', 0, b'>', 0]);
    }
}
