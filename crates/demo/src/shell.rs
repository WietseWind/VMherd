//! A pretend root shell: line editing, history, tab completion and a handful of commands whose
//! output fits the guest (its name, addresses, uptime). Slow things (boot, `apt update`,
//! shutdown) are returned as [`Step`]s with pauses, which the machine plays back.

use std::time::Duration;

use crate::Kind;
use crate::data::Spec;
use crate::term::Term;

/// One piece of a command's effect, played back in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Out(String),
    Wait(Duration),
    Clear,
    Prompt,
    /// The guest agent stops / starts answering (address lookups fail meanwhile).
    AgentDown,
    AgentUp,
    /// The VM is off: consoles close.
    PowerOff,
}

/// What a command may ask about its machine.
pub(crate) struct Env {
    pub uptime: u64,
    /// Seconds since 1970 (UTC).
    pub now: u64,
}

const COMMANDS: [&str; 19] = [
    "apt", "apt-get", "clear", "date", "echo", "exit", "help", "hostname", "id", "ip", "logout", "ls", "poweroff",
    "pwd", "reboot", "shutdown", "uname", "uptime", "whoami",
];

const KERNEL: &str = "6.1.0-vmherd-demo";
const OK: &str = "[\x1b[32m  OK  \x1b[0m]";

pub(crate) struct Shell {
    spec: &'static Spec,
    line: String,
    history: Vec<String>,
    /// Position while browsing the history with the arrow keys.
    browse: Option<usize>,
}

pub(crate) fn prompt(spec: &Spec) -> String {
    format!("\x1b[1;32mroot@{}\x1b[0m:\x1b[1;34m~\x1b[0m# ", spec.name)
}

impl Shell {
    pub fn new(spec: &'static Spec) -> Self {
        Self { spec, line: String::new(), history: Vec::new(), browse: None }
    }

    /// Forget the line being typed (a new boot or login).
    pub fn reset_line(&mut self) {
        self.line.clear();
        self.browse = None;
    }

    /// A key press (X11 keysym; `ctrl` = a Control key is held). Echo and line editing go
    /// straight to `term`; the returned steps (a command's effect) are for the machine to play.
    pub fn key(&mut self, keysym: u32, ctrl: bool, term: &mut Term, env: &Env) -> Vec<Step> {
        if ctrl {
            match char::from_u32(keysym).map(|c| c.to_ascii_lowercase()) {
                Some('c') => {
                    term.write("^C\n");
                    self.reset_line();
                    return vec![Step::Prompt];
                }
                Some('d') if self.line.is_empty() => {
                    term.write("\n");
                    return self.run("exit", env);
                }
                Some('l') => {
                    term.clear();
                    term.write(&prompt(self.spec));
                    term.write(&self.line);
                }
                Some('u') => self.replace_line(term, String::new()),
                _ => {}
            }
            return Vec::new();
        }
        match keysym {
            0xff0d | 0xff8d => {
                term.write("\n");
                let line = std::mem::take(&mut self.line);
                self.browse = None;
                if !line.trim().is_empty() && self.history.last() != Some(&line) {
                    self.history.push(line.clone());
                }
                return self.run(&line, env);
            }
            0xff08 => {
                if self.line.pop().is_some() {
                    term.backspace();
                }
            }
            0xff09 => {
                if !self.line.contains(' ') && !self.line.is_empty() {
                    let found: Vec<&str> = COMMANDS.iter().copied().filter(|c| c.starts_with(&self.line)).collect();
                    if let [one] = found[..] {
                        let rest = format!("{} ", &one[self.line.len()..]);
                        term.write(&rest);
                        self.line.push_str(&rest);
                    }
                }
            }
            0xff52 if !self.history.is_empty() => {
                let i = self.browse.unwrap_or(self.history.len()).saturating_sub(1);
                self.browse = Some(i);
                self.replace_line(term, self.history[i].clone());
            }
            0xff54 => {
                if let Some(i) = self.browse {
                    let next = self.history.get(i + 1).cloned();
                    self.browse = next.as_ref().map(|_| i + 1);
                    self.replace_line(term, next.unwrap_or_default());
                }
            }
            0x20..=0x7e => {
                if let Some(c) = char::from_u32(keysym)
                    && self.line.len() < 200
                {
                    self.line.push(c);
                    term.write(c.encode_utf8(&mut [0; 4]));
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn replace_line(&mut self, term: &mut Term, new: String) {
        for _ in self.line.chars() {
            term.backspace();
        }
        term.write(&new);
        self.line = new;
    }

    /// Run a command line (`a; b` and `a && b` run one after the other).
    pub fn run(&mut self, line: &str, env: &Env) -> Vec<Step> {
        let mut steps = Vec::new();
        for cmd in line.replace("&&", ";").split(';') {
            if !self.command(cmd.trim(), env, &mut steps) {
                return steps; // logged out, rebooting or powering off: no prompt now
            }
        }
        steps.push(Step::Prompt);
        steps
    }

    /// One command; false when it ends the session.
    fn command(&self, cmd: &str, env: &Env, steps: &mut Vec<Step>) -> bool {
        let spec = self.spec;
        let words: Vec<&str> = cmd.split_whitespace().collect();
        let Some((&name, args)) = words.split_first() else { return true };
        let text = match (name, args) {
            ("hostname", []) => format!("{}\n", spec.name),
            ("hostname", ["-I"]) => format!("{} {} \n", spec.ipv4(), spec.ipv6()),
            ("hostname", ["-i"]) => format!("{}\n", spec.ipv4()),
            ("hostname", ["-f"]) => format!("{}\n", spec.fqdn()),
            ("ip", ["a" | "addr" | "address"] | ["a" | "addr" | "address", "s" | "show"]) => ip_addr(spec),
            ("ip", ["-br" | "-brief", a]) if a.starts_with('a') => ip_brief(spec),
            ("ip", ["r" | "ro" | "route"]) => ip_route(spec),
            ("uptime", []) => uptime(env, spec.seed()),
            ("apt" | "apt-get", ["update"]) => {
                steps.extend(apt_update(spec));
                return true;
            }
            ("clear", _) => {
                steps.push(Step::Clear);
                return true;
            }
            ("whoami", []) => "root\n".into(),
            ("id", []) => "uid=0(root) gid=0(root) groups=0(root)\n".into(),
            ("uname", []) => "Linux\n".into(),
            ("uname", ["-r"]) => format!("{KERNEL}\n"),
            ("uname", ["-n"]) => format!("{}\n", spec.name),
            ("uname", ["-a"]) => format!("Linux {} {KERNEL} #1 SMP x86_64\n", spec.name),
            ("date", []) => format!("{}\n", date(env.now)),
            ("echo", _) => {
                let rest = cmd.strip_prefix("echo").unwrap_or_default().trim();
                format!("{}\n", rest.replace(['"', '\''], ""))
            }
            ("pwd", []) => "/root\n".into(),
            ("ls", _) => String::new(),
            ("help", _) => format!("Commands in this demo image:\n  {}\n", COMMANDS.join(" ")),
            ("exit" | "logout", _) => {
                steps.extend([
                    Step::Out("logout\n".into()),
                    Step::Wait(Duration::from_millis(600)),
                    Step::Clear,
                    Step::Out(banner(spec)),
                    Step::Prompt,
                ]);
                return false;
            }
            ("reboot", []) => {
                steps.extend(shutdown(spec, true));
                return false;
            }
            ("poweroff" | "halt", []) | ("shutdown", ["now"] | ["-h", "now"]) => {
                steps.extend(shutdown(spec, false));
                return false;
            }
            (name, _) if COMMANDS.contains(&name) => format!("{name}: the demo image does not simulate that\n"),
            (name, _) => format!("-bash: {name}: command not found\n"),
        };
        steps.push(Step::Out(text));
        true
    }
}

pub(crate) fn banner(spec: &Spec) -> String {
    let kind = match spec.kind {
        Kind::Qemu => "VM",
        Kind::Lxc => "CT",
    };
    let b = |s: &str| format!("\x1b[1m{s}\x1b[0m");
    format!(
        "\x1b[1;32mVMherd demo image\x1b[0m — simulated VM\n\
         \x1b[2m{} · {} · {kind} {} on {}\x1b[0m\n\n\
         Nothing here is real: this VM lives inside VMherd's built-in demo cluster.\n\
         Try: {}  {}  {}  {}  {}  {}\n\n",
        spec.fqdn(),
        spec.ipv4(),
        spec.vmid,
        spec.node,
        b("hostname"),
        b("ip -br a"),
        b("uptime"),
        b("apt update"),
        b("reboot"),
        b("poweroff"),
    )
}

fn ms(ms: u64) -> Step {
    Step::Wait(Duration::from_millis(ms))
}

/// A pause of about `base` ms, 70-130 % depending on the guest (and `k`, so lines differ).
fn jitter(spec: &Spec, base: u64, k: u32) -> Step {
    ms(base * u64::from(70 + spec.seed().rotate_right(k * 3) % 61) / 100)
}

/// Kernel and service lines, then the login banner.
pub(crate) fn boot(spec: &Spec) -> Vec<Step> {
    let mut steps = vec![Step::Clear, Step::AgentDown, ms(300)];
    let time = |t: f64| format!("\x1b[2m[{t:>12.6}]\x1b[0m");
    if spec.kind == Kind::Qemu {
        let kernel = [
            (0.0, format!("Linux version {KERNEL} (VMherd demo image) #1 SMP")),
            (0.0, "Command line: BOOT_IMAGE=/boot/vmlinuz root=/dev/vda1 ro quiet".into()),
            (0.184211, "smpboot: CPU0: Virtual CPU (family: 0x6, model: 0x6)".into()),
            (0.412345, "ACPI: Core revision 20220331".into()),
            (0.873112, "virtio_blk virtio2: [vda] 67108864 512-byte blocks (34.4 GB)".into()),
            (1.204551, format!("virtio_net virtio1 {}: renamed from eth0", spec.iface())),
            (1.604518, "EXT4-fs (vda1): mounted filesystem with ordered data mode.".into()),
            (2.010077, format!("systemd[1]: Hostname set to <{}>.", spec.name)),
        ];
        for (i, (t, line)) in kernel.into_iter().enumerate() {
            steps.push(Step::Out(format!("{} {line}\n", time(t))));
            steps.push(jitter(spec, 160, i as u32));
        }
    }
    let services = [
        "Started Journal Service.",
        "Finished Load Kernel Modules.",
        "Reached target Local File Systems.",
        "Started Network Configuration.",
        "Reached target Network.",
        "Started SSH server.",
        "Started QEMU Guest Agent.",
        "Reached target Multi-User System.",
    ];
    // Containers have no QEMU guest agent (Proxmox reads their interfaces directly).
    let services = services.into_iter().filter(|l| spec.kind == Kind::Qemu || !l.contains("QEMU"));
    for (i, line) in services.enumerate() {
        steps.push(Step::Out(format!("{OK} {line}\n")));
        steps.push(jitter(spec, 190, i as u32 + 8));
    }
    steps.extend([ms(700), Step::Clear, Step::Out(banner(spec)), Step::AgentUp, Step::Prompt]);
    steps
}

/// Services stopping, then power off (or, for `reboot`, a new boot).
pub(crate) fn shutdown(spec: &Spec, reboot: bool) -> Vec<Step> {
    let mut steps = vec![Step::AgentDown];
    let target = if reboot { "System Reboot" } else { "System Power Off" };
    let agent = (spec.kind == Kind::Qemu).then(|| "Stopped QEMU Guest Agent.".to_owned());
    let lines = [
        Some("Stopped target Multi-User System.".to_owned()),
        agent,
        Some("Stopped SSH server.".into()),
        Some("Stopped Network Configuration.".into()),
        Some("Stopped Journal Service.".into()),
        Some(format!("Reached target {target}.")),
    ];
    for (i, line) in lines.into_iter().flatten().enumerate() {
        steps.push(Step::Out(format!("{OK} {line}\n")));
        steps.push(jitter(spec, 260, i as u32));
    }
    let what = if reboot { "Restarting system" } else { "Power down" };
    steps.push(Step::Out(format!("\x1b[2m[{:>12.6}]\x1b[0m reboot: {what}\n", 12.402188)));
    steps.push(ms(400));
    if reboot {
        steps.push(ms(600));
        steps.extend(boot(spec));
    } else {
        steps.push(Step::PowerOff);
    }
    steps
}

/// Package lists "downloading", with a progress line that updates in place.
fn apt_update(spec: &Spec) -> Vec<Step> {
    let s = spec.seed();
    let mirror = "http://mirror.example.org/demo";
    let total = 8_200 + s % 900;
    let mut steps = vec![
        Step::Out(format!("Hit:1 {mirror} stable InRelease\n")),
        jitter(spec, 300, 1),
        Step::Out(format!("Get:2 {mirror} stable-updates InRelease [55.4 kB]\n")),
        jitter(spec, 250, 2),
        Step::Out("Get:3 http://security.example.org/demo stable-security InRelease [48.0 kB]\n".into()),
        jitter(spec, 350, 3),
    ];
    for (i, pct) in [12, 34, 58, 81, 97].into_iter().enumerate() {
        let done = total * pct / 100;
        steps.push(Step::Out(format!(
            "\r\x1b[1m{pct}%\x1b[0m [4 Packages {} kB/{} kB {pct}%]\x1b[K",
            thousands(done),
            thousands(total)
        )));
        steps.push(jitter(spec, 260, 4 + i as u32));
    }
    let upgradable = s % 7;
    let summary = match upgradable {
        0 => "All packages are up to date.".to_owned(),
        1 => "1 package can be upgraded. Run 'apt list --upgradable' to see it.".to_owned(),
        n => format!("{n} packages can be upgraded. Run 'apt list --upgradable' to see them."),
    };
    steps.extend([
        Step::Out(format!("\r\x1b[KGet:4 {mirror} stable/main amd64 Packages [{} kB]\n", thousands(total))),
        Step::Out(format!("Fetched {} kB in 2s ({} kB/s)\n", thousands(total + 103), thousands((total + 103) / 2))),
        jitter(spec, 400, 10),
        Step::Out("Reading package lists... Done\n".into()),
        jitter(spec, 250, 11),
        Step::Out("Building dependency tree... Done\n".into()),
        Step::Out("Reading state information... Done\n".into()),
        jitter(spec, 150, 12),
        Step::Out(format!("{summary}\n")),
        Step::Prompt,
    ]);
    steps
}

fn thousands(n: u32) -> String {
    if n >= 1000 { format!("{},{:03}", n / 1000, n % 1000) } else { n.to_string() }
}

fn ip_addr(spec: &Spec) -> String {
    let dev = spec.iface();
    let (state, qdisc) = match spec.kind {
        Kind::Qemu => ("", "fq_codel"),
        Kind::Lxc => ("@if12", "noqueue"),
    };
    let v4 = spec.ipv4();
    let brd = format!("{}.255", v4.rsplit_once('.').map_or("", |(net, _)| net));
    let forever = "       valid_lft forever preferred_lft forever\n";
    format!(
        "1: lo: <LOOPBACK,UP,LOWER_UP> mtu 65536 qdisc noqueue state UNKNOWN group default qlen 1000\n\
         \x20   link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00\n\
         \x20   inet 127.0.0.1/8 scope host lo\n{forever}\
         \x20   inet6 ::1/128 scope host noprefixroute\n{forever}\
         2: {dev}{state}: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc {qdisc} state UP group default\n\
         \x20   link/ether {} brd ff:ff:ff:ff:ff:ff\n\
         \x20   inet {v4}/24 brd {brd} scope global {dev}\n{forever}\
         \x20   inet6 {}/64 scope global\n{forever}\
         \x20   inet6 {}/64 scope link\n{forever}",
        spec.mac(),
        spec.ipv6(),
        spec.link_local(),
    )
}

/// Without the link-local address, so a line fits the 80-column console.
fn ip_brief(spec: &Spec) -> String {
    format!(
        "{:<16} {:<14} 127.0.0.1/8 ::1/128\n{:<16} \x1b[32m{:<14}\x1b[0m {}/24 {}/64\n",
        "lo",
        "UNKNOWN",
        spec.iface(),
        "UP",
        spec.ipv4(),
        spec.ipv6(),
    )
}

fn ip_route(spec: &Spec) -> String {
    let v4 = spec.ipv4();
    let net = v4.rsplit_once('.').map_or("", |(net, _)| net);
    format!(
        "default via {} dev {dev} proto static\n{net}.0/24 dev {dev} proto kernel scope link src {v4}\n",
        spec.gateway(),
        dev = spec.iface()
    )
}

fn uptime(env: &Env, seed: u32) -> String {
    let (h, m, s) = ((env.now / 3600) % 24, (env.now / 60) % 60, env.now % 60);
    let up = env.uptime;
    let (days, hours, mins) = (up / 86_400, (up / 3600) % 24, (up / 60) % 60);
    let since = match (days, hours) {
        (0, 0) => format!("{mins} min"),
        (0, _) => format!("{hours:>2}:{mins:02}"),
        (1, _) => format!("1 day, {hours:>2}:{mins:02}"),
        (d, _) => format!("{d} days, {hours:>2}:{mins:02}"),
    };
    let load = |k: u32| format!("0.{:02}", seed.rotate_left(k * 5) % 60);
    format!(" {h:02}:{m:02}:{s:02} up {since},  1 user,  load average: {}, {}, {}\n", load(1), load(2), load(3))
}

/// `date` output for a Unix time, in UTC: "Sun Sep 27 14:02:11 UTC 2026".
fn date(unix: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = (unix / 86_400) as i64;
    // civil date from days since 1970-01-01 (H. Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{} {} {day:>2} {:02}:{:02}:{:02} UTC {year}",
        DAYS[(days + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        (unix / 3600) % 24,
        (unix / 60) % 60,
        unix % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::spec;

    const ENV: Env = Env { uptime: 23 * 86_400 + 4 * 3600 + 7 * 60, now: 1_790_517_731 };

    /// Type `text` (then Enter) and play the result without pauses.
    fn type_line(shell: &mut Shell, term: &mut Term, text: &str) -> Vec<Step> {
        let mut steps = Vec::new();
        for c in text.chars() {
            let keysym = if c == '\n' { 0xff0d } else { c as u32 };
            steps.extend(shell.key(keysym, false, term, &ENV));
        }
        for s in &steps {
            match s {
                Step::Out(t) => term.write(t),
                Step::Prompt => term.write(&prompt(shell.spec)),
                Step::Clear => term.clear(),
                _ => {}
            }
        }
        steps
    }

    fn fresh() -> (Shell, Term) {
        let spec = spec(101).unwrap();
        let mut term = Term::default();
        term.write(&prompt(spec));
        (Shell::new(spec), term)
    }

    #[test]
    fn commands_print_what_fits_the_guest() {
        let (mut sh, mut t) = fresh();
        type_line(&mut sh, &mut t, "hostname\n");
        type_line(&mut sh, &mut t, "hostname -I\n");
        type_line(&mut sh, &mut t, "whoami; uname -a\n");
        type_line(&mut sh, &mut t, "echo \"hi there\" && nope\n");
        let text = t.text();
        for want in [
            "root@web-01:~# hostname\nweb-01\n",
            "192.0.2.11 2001:db8:10::11\n",
            "root\nLinux web-01 6.1.0-vmherd-demo #1 SMP x86_64\n",
            "hi there\n-bash: nope: command not found\nroot@web-01:~#",
        ] {
            assert!(text.contains(want), "{want:?} not in\n{text}");
        }
        type_line(&mut sh, &mut t, "clear\n");
        assert_eq!(t.text(), "root@web-01:~#");
        type_line(&mut sh, &mut t, "ip -br a\n");
        let text = t.text();
        assert!(text.contains("ens18            UP             192.0.2.11/24 2001:db8:10::11/64"), "{text}");
        type_line(&mut sh, &mut t, "uptime\n");
        assert!(t.text().contains(" up 23 days,  4:07,  1 user,  load average: 0."), "{}", t.text());
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "Thu Jan  1 00:00:00 UTC 1970");
        assert_eq!(date(1_790_517_731), "Sun Sep 27 14:02:11 UTC 2026");
        assert_eq!(date(951_782_400), "Tue Feb 29 00:00:00 UTC 2000");
    }

    #[test]
    fn line_editing_history_and_control_keys() {
        let (mut sh, mut t) = fresh();
        type_line(&mut sh, &mut t, "whoami\n");
        for k in [0x6e, 0x6f, 0xff08, 0xff08] {
            sh.key(k, false, &mut t, &ENV); // "no", then erased
        }
        sh.key(0xff52, false, &mut t, &ENV); // arrow up: "whoami"
        assert!(t.text().ends_with("# whoami"), "{}", t.text());
        sh.key(0x75, true, &mut t, &ENV); // ^U
        assert!(t.text().ends_with('#'));
        for c in "upt".chars() {
            sh.key(c as u32, false, &mut t, &ENV);
        }
        sh.key(0xff09, false, &mut t, &ENV); // tab completes "uptime "
        assert!(t.text().ends_with("# uptime"));
        assert_eq!(sh.key(0x63, true, &mut t, &ENV), vec![Step::Prompt]); // ^C
        assert!(t.text().ends_with("# uptime ^C"));
        sh.key(0x6c, true, &mut t, &ENV); // ^L
        assert_eq!(t.text(), "root@web-01:~#");
        assert!(matches!(sh.key(0x64, true, &mut t, &ENV).first(), Some(Step::Out(s)) if s == "logout\n"));
    }

    #[test]
    fn slow_commands_are_steps() {
        let (mut sh, mut t) = fresh();
        let apt = type_line(&mut sh, &mut t, "apt update\n");
        let waited: Duration = apt.iter().map(|s| if let Step::Wait(d) = s { *d } else { Duration::ZERO }).sum();
        assert!(waited > Duration::from_secs(2) && waited < Duration::from_secs(6), "{waited:?}");
        assert!(t.text().contains("Reading package lists... Done"));
        assert_eq!(apt.last(), Some(&Step::Prompt));
        let off = type_line(&mut sh, &mut t, "poweroff\n");
        assert_eq!(off.first(), Some(&Step::AgentDown));
        assert_eq!(off.last(), Some(&Step::PowerOff));
        let reboot = type_line(&mut sh, &mut t, "reboot\n");
        assert!(reboot.contains(&Step::AgentUp) && !reboot.contains(&Step::PowerOff));
        assert!(t.text().contains("Try: hostname  ip -br a"), "{}", t.text());
    }

    /// Every address a guest shows is a reserved documentation value.
    #[test]
    fn only_documentation_addresses() {
        let v4_ok = |t: &str| ["192.0.2.", "198.51.100.", "203.0.113.", "127.0.0.1"].iter().any(|p| t.starts_with(p));
        let mut seen = Vec::new();
        for spec in crate::data::GUESTS.iter().filter(|g| !g.template) {
            let mut shell = Shell::new(spec);
            let mut out = banner(spec);
            for cmd in ["ip a", "ip -br a", "ip r", "hostname -I", "hostname -f", "apt update"] {
                for step in shell.run(cmd, &ENV) {
                    if let Step::Out(t) = step {
                        out.push_str(&t);
                    }
                }
            }
            for token in out.split(|c: char| c.is_whitespace() || c == '/' || c == '[' || c == ']') {
                let parts: Vec<&str> = token.split('.').collect();
                if parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
                    assert!(v4_ok(token), "{} shows {token}", spec.name);
                }
                if token.contains("::") {
                    assert!(["2001:db8:", "fe80::", "::1"].iter().any(|p| token.starts_with(p)), "{token}");
                }
                if token.len() == 17 && token.matches(':').count() == 5 {
                    assert!(
                        token.starts_with("00:00:5e:00:53:")
                            || token == "ff:ff:ff:ff:ff:ff"
                            || token == "00:00:00:00:00:00",
                        "{token}"
                    );
                }
                // host names: only reserved example domains
                let host = token.trim_end_matches(['.', ',']);
                if host.contains('.')
                    && host
                        .split('.')
                        .next_back()
                        .is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_lowercase()))
                {
                    assert!(host.ends_with(".example") || host.ends_with(".example.org"), "{host}");
                }
            }
            assert!(out.contains("mirror.example.org") && out.contains(&format!("{}.lab.example", spec.name)));
            seen.push((spec.ipv4(), spec.mac()));
        }
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "addresses are unique");
    }
}
