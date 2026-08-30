use super::{
    BTreeMap, COMPANION_BINARY_NAME, CliFailure, HostUpdateEvidence, HostUpdatePlan,
    MIN_PAIRED_BINARY_TARGET, Path, REMOTE_AFTER_COMPANION_SHA256_PREFIX,
    REMOTE_AFTER_COMPANION_VERSION_PREFIX, REMOTE_AFTER_PRIMARY_SHA256_PREFIX,
    REMOTE_AFTER_PRIMARY_VERSION_PREFIX, REMOTE_AFTER_STATUS_PREFIX, REMOTE_AUTHORITY_ID_PREFIX,
    REMOTE_BEFORE_COMPANION_SHA256_PREFIX, REMOTE_BEFORE_COMPANION_VERSION_PREFIX,
    REMOTE_BEFORE_PRIMARY_SHA256_PREFIX, REMOTE_BEFORE_PRIMARY_VERSION_PREFIX,
    REMOTE_BEFORE_STATUS_PREFIX, REMOTE_MINIMAL_PATH, REMOTE_REFRESH_PREFIX,
    REMOTE_RELEASE_ASSET_SHA256_PREFIX, REMOTE_SUPERVISOR, REMOTE_UPDATE_TIMEOUT, ReleaseAuthority,
    Value, Write, auth_support, home_dir, shlex_quote, tag_requires_companion,
    tag_supports_auth_resolver, unattended_tool_path, write_json_envelope,
};

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn remote_update_command(
    binary: &Path,
    companion_binary: &Path,
    target: &str,
    authority: &ReleaseAuthority,
    auth_wrapper: &Path,
    auth_helper: &Path,
    mode: &str,
    global_dir: &Path,
    state_dir: &Path,
) -> String {
    let install_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let version = target.strip_prefix('v').unwrap_or(target);
    let installer_url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        authority.repository, authority.commit_oid, authority.installer.path
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/{}/releases/assets/{}",
        authority.repository, authority.platform_asset.id
    );
    let (auth_helper_url, auth_wrapper_url) = auth_support::source_urls(authority);
    let before_auth = auth_support::probe(auth_helper, auth_wrapper, "before");
    let after_auth = auth_support::probe(auth_helper, auth_wrapper, "after");
    let auth_contract = auth_support::wrapper_helper_contract_probe(auth_helper);
    let binary_install_command = format!(
        "SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR={} SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null",
        shlex_quote(version),
        shlex_quote(&install_dir.display().to_string())
    );
    let auth_transaction = auth_support::install_transaction(
        auth_helper,
        auth_wrapper,
        binary,
        companion_binary,
        tag_requires_companion(target),
        tag_supports_auth_resolver(target),
        "\"$auth_helper_source\"",
        "\"$auth_wrapper_source\"",
        &binary_install_command,
        mode,
        global_dir,
        state_dir,
        &authority.repository,
        authority,
        false,
    );
    let before_pair = remote_pair_probe(binary, companion_binary, "before", None, false);
    let after_pair = remote_pair_probe(
        binary,
        companion_binary,
        "after",
        Some(version),
        tag_requires_companion(target),
    );
    let exact_asset_curl_shim = exact_asset_curl_shim(&authority.platform_asset.name);
    let auth_token_command = if tag_supports_auth_resolver(target) {
        resolver_auth_token_command(
            binary,
            mode,
            global_dir,
            target,
            &authority.repository,
            auth_wrapper,
        )
    } else {
        auth_token_command(&authority.repository, auth_wrapper)
    };
    let script = format!(
        "set -euo pipefail\n{}\n{}\n{}\n\
         before_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         token=\"$({})\"\n\
         installer=\"$(/usr/bin/mktemp)\"; staging_dir=\"$(/usr/bin/mktemp -d)\"\n\
         trap '/bin/rm -f \"$installer\"; /bin/rm -rf \"$staging_dir\"' EXIT\n\
         /usr/bin/curl -fsSL --output \"$installer\" {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         release_asset=\"$staging_dir/release-asset\"\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- -H 'Accept: application/octet-stream' --output \"$release_asset\" {}\n\
         release_asset_sha256=\"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\"; test \"$release_asset_sha256\" = {}\n\
         auth_helper_source=\"$staging_dir/shipyard-github-app-token\"; auth_wrapper_source=\"$staging_dir/ghapp\"\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- --output \"$auth_helper_source\" {}\n\
         /usr/bin/printf 'Authorization: Bearer %s\\n' \"$token\" | /usr/bin/curl -fsSL -H @- --output \"$auth_wrapper_source\" {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$auth_helper_source\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         test \"$(/usr/bin/shasum -a 256 \"$auth_wrapper_source\" | /usr/bin/awk '{{print $1}}')\" = {}\n\
         curl_shim=\"$staging_dir/curl-exact-asset\"; /usr/bin/printf '%s\\n' {} > \"$curl_shim\"; /bin/chmod 700 \"$curl_shim\"\n\
         SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" SHIPYARD_GITHUB_TOKEN=\"$token\" SHIPYARD_VERSION={} SHIPYARD_INSTALL_DIR=\"$staging_dir\" SHIPYARD_CURL_BIN=\"$curl_shim\" /bin/bash \"$installer\" >/dev/null\n\
         staged_binary=\"$staging_dir/shipyard\"; test \"$(\"$staged_binary\" --version)\" = {}\n\
         \"$staged_binary\" --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         {}\nunset token\n{}\n{}\n\
         {} --mode {} --global-dir {} --state-dir {} update --to {} --check --unattended-fleet >/dev/null\n\
         after_status=\"$({} --mode {} --global-dir {} --state-dir {} --json daemon status | /usr/bin/tr -d '\\n')\"\n\
         printf '%s%s\\n' {} \"$before_primary_sha256\"; printf '%s%s\\n' {} \"$before_primary_version\"\n\
         printf '%s%s\\n' {} \"$before_companion_sha256\"; printf '%s%s\\n' {} \"$before_companion_version\"\n\
         printf '%s%s\\n' {} \"$after_primary_sha256\"; printf '%s%s\\n' {} \"$after_primary_version\"\n\
         printf '%s%s\\n' {} \"$after_companion_sha256\"; printf '%s%s\\n' {} \"$after_companion_version\"\n\
         printf '%s%s\\n' {} \"$before_auth_helper_sha256\"; printf '%s%s\\n' {} \"$before_auth_helper_mode\"\n\
         printf '%s%s\\n' {} \"$before_auth_wrapper_sha256\"; printf '%s%s\\n' {} \"$before_auth_wrapper_mode\"\n\
         printf '%s%s\\n' {} \"$after_auth_helper_sha256\"; printf '%s%s\\n' {} \"$after_auth_helper_mode\"\n\
         printf '%s%s\\n' {} \"$after_auth_wrapper_sha256\"; printf '%s%s\\n' {} \"$after_auth_wrapper_mode\"\n\
         printf '%s%s\\n' {} \"$before_status\"; printf '%s%s\\n' {} \"$refresh_receipt\"; printf '%s%s\\n' {} \"$after_status\"\n\
         printf '%s%s\\n' {} {}; printf '%s%s\\n' {} \"$release_asset_sha256\"",
        before_pair,
        before_auth,
        auth_contract,
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        auth_token_command,
        shlex_quote(&installer_url),
        shlex_quote(&authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&authority.platform_asset.sha256),
        shlex_quote(&auth_helper_url),
        shlex_quote(&auth_wrapper_url),
        shlex_quote(&authority.auth_helper.sha256),
        shlex_quote(&authority.auth_wrapper.sha256),
        shlex_quote(&exact_asset_curl_shim),
        shlex_quote(version),
        shlex_quote(&format!("shipyard {version}")),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        auth_transaction,
        after_pair,
        after_auth,
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(target),
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&state_dir.display().to_string()),
        shlex_quote(REMOTE_BEFORE_PRIMARY_SHA256_PREFIX),
        shlex_quote(REMOTE_BEFORE_PRIMARY_VERSION_PREFIX),
        shlex_quote(REMOTE_BEFORE_COMPANION_SHA256_PREFIX),
        shlex_quote(REMOTE_BEFORE_COMPANION_VERSION_PREFIX),
        shlex_quote(REMOTE_AFTER_PRIMARY_SHA256_PREFIX),
        shlex_quote(REMOTE_AFTER_PRIMARY_VERSION_PREFIX),
        shlex_quote(REMOTE_AFTER_COMPANION_SHA256_PREFIX),
        shlex_quote(REMOTE_AFTER_COMPANION_VERSION_PREFIX),
        shlex_quote(auth_support::BEFORE_HELPER_SHA_PREFIX),
        shlex_quote(auth_support::BEFORE_HELPER_MODE_PREFIX),
        shlex_quote(auth_support::BEFORE_WRAPPER_SHA_PREFIX),
        shlex_quote(auth_support::BEFORE_WRAPPER_MODE_PREFIX),
        shlex_quote(auth_support::AFTER_HELPER_SHA_PREFIX),
        shlex_quote(auth_support::AFTER_HELPER_MODE_PREFIX),
        shlex_quote(auth_support::AFTER_WRAPPER_SHA_PREFIX),
        shlex_quote(auth_support::AFTER_WRAPPER_MODE_PREFIX),
        shlex_quote(REMOTE_BEFORE_STATUS_PREFIX),
        shlex_quote(REMOTE_REFRESH_PREFIX),
        shlex_quote(REMOTE_AFTER_STATUS_PREFIX),
        shlex_quote(REMOTE_AUTHORITY_ID_PREFIX),
        shlex_quote(&authority.identity_sha256),
        shlex_quote(REMOTE_RELEASE_ASSET_SHA256_PREFIX),
    );
    format!(
        "/usr/bin/env -i HOME=\"$HOME\" PATH={} /usr/bin/perl -e {} {} /bin/bash -c {}",
        REMOTE_MINIMAL_PATH,
        shlex_quote(REMOTE_SUPERVISOR),
        REMOTE_UPDATE_TIMEOUT.as_secs(),
        shlex_quote(&script),
    )
}

pub(super) fn auth_token_command(repository: &str, auth_wrapper: &Path) -> String {
    format!(
        "GH_REPO={} {} auth token",
        shlex_quote(repository),
        shlex_quote(&auth_wrapper.display().to_string())
    )
}

pub(super) fn resolver_auth_token_command(
    binary: &Path,
    mode: &str,
    global_dir: &Path,
    target: &str,
    repository: &str,
    auth_wrapper: &Path,
) -> String {
    let resolver = format!(
        "{} --mode {} --global-dir {} auth helper-argv --wrapper {} --repo {}",
        shlex_quote(&binary.display().to_string()),
        shlex_quote(mode),
        shlex_quote(&global_dir.display().to_string()),
        shlex_quote(&auth_wrapper.display().to_string()),
        shlex_quote(repository),
    );
    let parser = r#"import json, os, subprocess, sys, unicodedata
def refuse(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)
try:
    value = json.load(sys.stdin)
except Exception:
    refuse("shipyard fleet auth resolver returned malformed JSON")
expected_wrapper, expected_repo = sys.argv[1:3]
required = {"schema_version", "command", "wrapper", "repo", "credential_argv"}
if not isinstance(value, dict) or set(value) != required or type(value.get("schema_version")) is not int or value.get("schema_version") != 1 or value.get("command") != "auth.helper-argv":
    refuse("shipyard fleet auth resolver returned an unsupported contract")
if value.get("wrapper") != expected_wrapper or value.get("repo") != expected_repo:
    refuse("shipyard fleet auth resolver returned mismatched authority")
credential_argv = value.get("credential_argv")
if not isinstance(credential_argv, list) or len(credential_argv) != 4 or not all(isinstance(item, str) for item in credential_argv):
    refuse("shipyard fleet auth resolver returned malformed credential arguments")
app_id = credential_argv[1]
if credential_argv[0] != "--app-id" or not 1 <= len(app_id) <= 20 or not app_id.isascii() or not app_id.isdecimal() or not 0 < int(app_id) <= 18446744073709551615:
    refuse("shipyard fleet auth resolver returned an invalid app id")
private_key = credential_argv[3]
def normalized_absolute_path(item):
    return (
        isinstance(item, str)
        and 2 <= len(item) <= 4096
        and item.startswith("/")
        and all(part not in {"", ".", ".."} for part in item.split("/")[1:])
        and not any(unicodedata.category(character) == "Cc" for character in item)
    )
if credential_argv[2] != "--private-key" or not normalized_absolute_path(private_key) or any(not item or any(unicodedata.category(character) == "Cc" for character in item) for item in credential_argv):
    refuse("shipyard fleet auth resolver returned an invalid private-key path")
environment = {"HOME": os.environ["HOME"], "PATH": os.environ["PATH"]}
try:
    completed = subprocess.run(
        [expected_wrapper, "token", *credential_argv, "--repo", expected_repo],
        check=False, capture_output=True, text=True, timeout=30, env=environment,
    )
except Exception:
    refuse("shipyard fleet auth wrapper could not be executed")
if completed.returncode != 0:
    refuse("shipyard fleet auth wrapper refused the machine-global credential contract")
try:
    helper = json.loads(completed.stdout)
except Exception:
    refuse("shipyard fleet auth wrapper returned malformed JSON")
token = helper.get("token") if isinstance(helper, dict) else None
if not isinstance(token, str) or not token or any(ord(character) <= 32 or ord(character) == 127 for character in token):
    refuse("shipyard fleet auth wrapper returned a malformed token")
sys.stdout.write(token)"#;
    format!(
        "resolver_json=\"$({resolver} | /usr/bin/head -c 16385; resolver_status=${{PIPESTATUS[0]}}; exit \"$resolver_status\")\" || {{ /usr/bin/printf '%s\\n' {} >&2; exit 1; }}; if test \"${{#resolver_json}}\" -gt 16384; then /usr/bin/printf '%s\\n' 'shipyard fleet auth resolver response exceeds 16384 bytes' >&2; exit 1; fi; /usr/bin/printf '%s' \"$resolver_json\" | /usr/bin/python3 -I -c {} {} {}",
        shlex_quote(&format!(
            "shipyard fleet auth resolver unavailable; predeploy {target} with ordinary shipyard update and migrate machine-global github.auth.token_command to the exact ghapp token contract before fleet-update"
        )),
        shlex_quote(parser),
        shlex_quote(&auth_wrapper.display().to_string()),
        shlex_quote(repository),
    )
}

pub(super) fn exact_asset_curl_shim(asset_name: &str) -> String {
    format!(
        "#!/bin/bash\nset -euo pipefail\ncase \"$*\" in\n  *\"/releases/tags/\"*) /usr/bin/printf '{{\"assets\":[{{\"name\":\"{asset_name}\",\"url\":\"file://%s\",\"browser_download_url\":\"file://%s\"}}]}}\\n200\\n' \"$SHIPYARD_FLEET_ASSET_PATH\" \"$SHIPYARD_FLEET_ASSET_PATH\" ;;\n  *) exec /usr/bin/curl \"$@\" ;;\nesac"
    )
}

pub(super) fn remote_pair_probe(
    binary: &Path,
    companion: &Path,
    prefix: &str,
    expected_version: Option<&str>,
    companion_required: bool,
) -> String {
    let binary = shlex_quote(&binary.display().to_string());
    let companion = shlex_quote(&companion.display().to_string());
    let [minimum_major, minimum_minor, minimum_patch] = MIN_PAIRED_BINARY_TARGET;
    let expected = expected_version.map_or_else(String::new, |version| {
        let primary = shlex_quote(&format!("shipyard {version}"));
        let provider = shlex_quote(&format!("{COMPANION_BINARY_NAME} {version}"));
        if companion_required {
            format!(
                "test \"${prefix}_primary_version\" = {primary}\n\
                 test \"${prefix}_companion_version\" = {provider}"
            )
        } else {
            format!(
                "test \"${prefix}_primary_version\" = {primary}\n\
                 test \"${prefix}_companion_version\" = absent"
            )
        }
    });
    let inferred = expected_version.map_or_else(
        || format!(
            "{prefix}_semver=\"${{{prefix}_primary_version#shipyard }}\"\n\
             test \"${prefix}_primary_version\" = \"shipyard ${{{prefix}_semver}}\"\n\
             case \"${{{prefix}_semver}}\" in *.*.*) ;; *) exit 1 ;; esac\n\
             case \"${{{prefix}_semver}}\" in *.*.*.*) exit 1 ;; esac\n\
             IFS=. read -r {prefix}_major {prefix}_minor {prefix}_patch <<EOF\n\
             ${{{prefix}_semver}}\n\
             EOF\n\
             for {prefix}_component in \"${{{prefix}_major}}\" \"${{{prefix}_minor}}\" \"${{{prefix}_patch}}\"; do\n\
               case \"${{{prefix}_component}}\" in *[!0-9]*|'') exit 1 ;; esac\n\
               case \"${{{prefix}_component}}\" in 0|[1-9]*) ;; *) exit 1 ;; esac\n\
               if [ \"${{#{prefix}_component}}\" -gt 20 ] || {{ [ \"${{#{prefix}_component}}\" -eq 20 ] && [ \"${{{prefix}_component}}\" \\> 18446744073709551615 ]; }}; then exit 1; fi\n\
             done\n\
             {prefix}_decimal_gt() {{\n\
               [ \"${{#1}}\" -gt \"${{#2}}\" ] || {{ [ \"${{#1}}\" -eq \"${{#2}}\" ] && [ \"$1\" \\> \"$2\" ]; }}\n\
             }}\n\
             {prefix}_requires=0\n\
             if {prefix}_decimal_gt \"${{{prefix}_major}}\" {minimum_major} || {{ [ \"${{{prefix}_major}}\" = {minimum_major} ] && {{ {prefix}_decimal_gt \"${{{prefix}_minor}}\" {minimum_minor} || {{ [ \"${{{prefix}_minor}}\" = {minimum_minor} ] && {{ [ \"${{{prefix}_patch}}\" = {minimum_patch} ] || {prefix}_decimal_gt \"${{{prefix}_patch}}\" {minimum_patch}; }}; }}; }}; }}; then {prefix}_requires=1; fi\n\
             if [ \"${{{prefix}_requires}}\" -eq 1 ]; then\n\
               test \"${prefix}_companion_version\" = \"{COMPANION_BINARY_NAME} ${{{prefix}_semver}}\"\n\
             else\n\
               test \"${prefix}_companion_version\" = absent\n\
             fi"
        ),
        |_| expected,
    );
    format!(
        "{prefix}_primary_sha256_before=\"$(/usr/bin/shasum -a 256 {binary} | /usr/bin/awk '{{print $1}}')\"\n\
         {prefix}_primary_version=\"$({binary} --version)\"\n\
         {prefix}_primary_sha256=\"$(/usr/bin/shasum -a 256 {binary} | /usr/bin/awk '{{print $1}}')\"\n\
         test \"${prefix}_primary_sha256_before\" = \"${prefix}_primary_sha256\"\n\
         if [ -e {companion} ] || [ -L {companion} ]; then\n\
           test -x {companion}\n\
           {prefix}_companion_sha256_before=\"$(/usr/bin/shasum -a 256 {companion} | /usr/bin/awk '{{print $1}}')\"\n\
           {prefix}_companion_version=\"$({companion} --version)\"\n\
           {prefix}_companion_sha256=\"$(/usr/bin/shasum -a 256 {companion} | /usr/bin/awk '{{print $1}}')\"\n\
           test \"${prefix}_companion_sha256_before\" = \"${prefix}_companion_sha256\"\n\
         else\n\
           {prefix}_companion_version=absent\n\
           {prefix}_companion_sha256=absent\n\
         fi\n\
         {inferred}"
    )
}

pub(super) fn local_update_command(plan: &HostUpdatePlan) -> String {
    let installer_url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        plan.release_authority.repository,
        plan.release_authority.commit_oid,
        plan.release_authority.installer.path
    );
    let release_asset_url = format!(
        "https://api.github.com/repos/{}/releases/assets/{}",
        plan.release_authority.repository, plan.release_authority.platform_asset.id
    );
    let (auth_helper_url, auth_wrapper_url) = auth_support::source_urls(&plan.release_authority);
    let auth_contract = auth_support::wrapper_helper_contract_probe(&plan.auth_helper);
    let curl_shim = exact_asset_curl_shim(&plan.release_authority.platform_asset.name);
    let binary_install_command = format!(
        "SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" /usr/bin/env -i HOME={} PATH={} SHIPYARD_FLEET_ASSET_PATH=\"$release_asset\" {} --mode {} --global-dir {} --state-dir {} --json update --to {} --install-script-url \"file://$installer\" --curl-bin \"$curl_shim\" --unattended-fleet",
        shlex_quote(&home_dir().display().to_string()),
        shlex_quote(&unattended_tool_path().to_string_lossy()),
        shlex_quote(&plan.binary.display().to_string()),
        plan.runtime_mode.as_str(),
        shlex_quote(&plan.global_dir.display().to_string()),
        shlex_quote(&plan.state_dir.display().to_string()),
        shlex_quote(&plan.target),
    );
    let auth_transaction = auth_support::install_transaction(
        &plan.auth_helper,
        &plan.auth_wrapper,
        &plan.binary,
        &plan.companion_binary,
        plan.companion_required,
        tag_supports_auth_resolver(&plan.target),
        "\"$auth_helper_source\"",
        "\"$auth_wrapper_source\"",
        &binary_install_command,
        plan.runtime_mode.as_str(),
        &plan.global_dir,
        &plan.state_dir,
        &plan.release_authority.repository,
        &plan.release_authority,
        false,
    );
    format!(
        "set -euo pipefail; {}; staging_dir=\"$(/usr/bin/mktemp -d)\"; installer=\"$staging_dir/install.sh\"; release_asset=\"$staging_dir/release-asset\"; auth_helper_source=\"$staging_dir/shipyard-github-app-token\"; auth_wrapper_source=\"$staging_dir/ghapp\"; curl_shim=\"$staging_dir/curl-exact-asset\"; trap '/bin/rm -rf \"$staging_dir\"' EXIT; /usr/bin/curl -fsSL --output \"$installer\" {}; test \"$(/usr/bin/shasum -a 256 \"$installer\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/curl -fsSL -H 'Accept: application/octet-stream' --output \"$release_asset\" {}; test \"$(/usr/bin/shasum -a 256 \"$release_asset\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/curl -fsSL --output \"$auth_helper_source\" {}; /usr/bin/curl -fsSL --output \"$auth_wrapper_source\" {}; test \"$(/usr/bin/shasum -a 256 \"$auth_helper_source\" | /usr/bin/awk '{{print $1}}')\" = {}; test \"$(/usr/bin/shasum -a 256 \"$auth_wrapper_source\" | /usr/bin/awk '{{print $1}}')\" = {}; /usr/bin/printf '%s\\n' {} > \"$curl_shim\"; /bin/chmod 700 \"$curl_shim\"; {}; /usr/bin/printf '%s\\n' \"$refresh_receipt\"",
        auth_contract,
        shlex_quote(&installer_url),
        shlex_quote(&plan.release_authority.installer.sha256),
        shlex_quote(&release_asset_url),
        shlex_quote(&plan.release_authority.platform_asset.sha256),
        shlex_quote(&auth_helper_url),
        shlex_quote(&auth_wrapper_url),
        shlex_quote(&plan.release_authority.auth_helper.sha256),
        shlex_quote(&plan.release_authority.auth_wrapper.sha256),
        shlex_quote(&curl_shim),
        auth_transaction,
    )
}

pub(super) fn render_plan<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plans: &[HostUpdatePlan],
    all_hosts: bool,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("plan"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("apply".to_owned(), Value::Bool(false));
        data.insert("all_hosts".to_owned(), Value::Bool(all_hosts));
        data.insert(
            "selected_host_classes".to_owned(),
            serde_json::to_value(plans.iter().map(|plan| &plan.class).collect::<Vec<_>>())
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "hosts".to_owned(),
            Value::Array(
                plans
                    .iter()
                    .map(|plan| {
                        serde_json::json!({
                            "class": plan.class,
                            "ssh": plan.ssh,
                            "binary": plan.binary,
                            "companion_binary": plan.companion_binary,
                            "auth_helper": plan.auth_helper,
                            "auth_wrapper": plan.auth_wrapper,
                            "source_identity": plan.source_identity,
                            "release_authority": plan.release_authority,
                            "companion_required": plan.companion_required,
                            "command": plan.command,
                        })
                    })
                    .collect(),
            ),
        );
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Fleet update plan for {target} ({}):",
            if all_hosts {
                "explicit all-host selection"
            } else {
                "explicit host-class selection"
            }
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for plan in plans {
            let route = plan.ssh.as_deref().unwrap_or("local");
            writeln!(stdout, "  {} ({route}): {}", plan.class, plan.command)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        writeln!(stdout, "Re-run with --apply to execute.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Human and JSON receipts intentionally share one field-complete boundary.
pub(super) fn render_host_result<W: Write>(
    stdout: &mut W,
    json: bool,
    target: &str,
    plan: &HostUpdatePlan,
    ok: bool,
    evidence: Option<&HostUpdateEvidence>,
    error: Option<&str>,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("host_result"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("host_class".to_owned(), Value::from(plan.class.clone()));
        data.insert("ok".to_owned(), Value::Bool(ok));
        data.insert(
            "binary".to_owned(),
            Value::from(plan.binary.display().to_string()),
        );
        data.insert(
            "companion_binary".to_owned(),
            Value::from(plan.companion_binary.display().to_string()),
        );
        data.insert(
            "auth_helper".to_owned(),
            Value::from(plan.auth_helper.display().to_string()),
        );
        data.insert(
            "auth_wrapper".to_owned(),
            Value::from(plan.auth_wrapper.display().to_string()),
        );
        data.insert(
            "source_identity".to_owned(),
            Value::from(plan.source_identity.clone()),
        );
        data.insert(
            "release_authority".to_owned(),
            serde_json::to_value(&plan.release_authority)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "daemon_mode".to_owned(),
            Value::from(plan.runtime_mode.as_str()),
        );
        data.insert(
            "daemon_global_dir".to_owned(),
            Value::from(plan.global_dir.display().to_string()),
        );
        data.insert(
            "daemon_state_dir".to_owned(),
            Value::from(plan.state_dir.display().to_string()),
        );
        if let Some(evidence) = evidence {
            insert_binary_pair_evidence(&mut data, evidence)?;
            data.insert(
                "auth_support_before".to_owned(),
                serde_json::to_value(&evidence.auth_support_before)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "auth_support_after".to_owned(),
                serde_json::to_value(&evidence.auth_support_after)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "executable_sha256".to_owned(),
                Value::from(evidence.executable_sha256.clone()),
            );
            data.insert(
                "cli_version".to_owned(),
                Value::from(evidence.cli_version.clone()),
            );
            data.insert(
                "daemon_version".to_owned(),
                Value::from(evidence.daemon_version.clone()),
            );
            data.insert("daemon_pid".to_owned(), Value::from(evidence.daemon_pid));
            data.insert(
                "configured_repos_before".to_owned(),
                serde_json::to_value(&evidence.configured_repos_before)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "configured_repos_after".to_owned(),
                serde_json::to_value(&evidence.configured_repos_after)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
            data.insert(
                "configured_repos_preserved".to_owned(),
                serde_json::to_value(evidence.configured_repos_preserved)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?,
            );
        }
        if let Some(error) = error {
            data.insert("error".to_owned(), Value::from(error));
        }
        write_json_envelope(stdout, "runner.fleet-update", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if ok {
        writeln!(
            stdout,
            "{}: updated to {target}; primary sha256={}; companion sha256={}; source={}; daemon pid={} version={}; configured repos preserved={}",
            plan.class,
            evidence.map_or("unavailable", |value| value.executable_sha256.as_str()),
            evidence
                .and_then(|value| value.after_pair.companion.as_ref())
                .map_or("absent", |value| value.sha256.as_str()),
            evidence
                .and_then(|value| value.after_pair.primary.source_identity.as_deref())
                .unwrap_or("unavailable"),
            evidence.map_or(0, |value| value.daemon_pid),
            evidence.map_or("unavailable", |value| value.daemon_version.as_str()),
            evidence
                .and_then(|value| value.configured_repos_preserved)
                .map_or("not-applicable", |preserved| if preserved { "true" } else { "false" }),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "{}: FAILED ({})",
            plan.class,
            error.unwrap_or("unknown error")
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn insert_binary_pair_evidence(
    data: &mut BTreeMap<String, Value>,
    evidence: &HostUpdateEvidence,
) -> Result<(), CliFailure> {
    data.insert(
        "release_authority_identity".to_owned(),
        Value::from(evidence.release_authority_identity.clone()),
    );
    data.insert(
        "release_asset_sha256".to_owned(),
        Value::from(evidence.release_asset_sha256.clone()),
    );
    data.insert(
        "binary_pair_before".to_owned(),
        serde_json::to_value(&evidence.before_pair)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    data.insert(
        "binary_pair_after".to_owned(),
        serde_json::to_value(&evidence.after_pair)
            .map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    Ok(())
}
