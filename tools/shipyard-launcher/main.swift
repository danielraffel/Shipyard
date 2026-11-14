// ShipyardLauncher.app: one stable macOS privacy identity for Shipyard.
//
// macOS privacy consent (TCC) is decided for a process's *responsible
// process*. A launchd job is its own responsible process, and every child
// inherits it. A bare command-line binary is identified by its path, and each
// Shipyard release installs under a new auth-generations/<id>/ path, so a
// LaunchAgent that runs the release binary directly is a new identity after
// every install and raises a new "access removable volume" prompt. An app
// bundle is identified by its designated requirement (bundle identifier and
// team), which stays the same across rebuilds.
//
// This executable is the program of those LaunchAgents (or is exec'd into by
// them, which keeps the job's pid). It resolves the current Shipyard release,
// refuses anything not signed with Developer ID by its own team, and spawns
// it as a child, so the bundle stays the responsible process. It never execs
// the release: after exec the pid's identity would be the release binary.
//
// Usage:
//   shipyard-launcher [shipyard arguments...]
//   shipyard-launcher --launcher-script <name> [script arguments...]
//   shipyard-launcher --launcher-verify
//   shipyard-launcher --launcher-active
//   shipyard-launcher --launcher-check-access [--launcher-result <file>]
//   shipyard-launcher --launcher-version
//
// Exit codes of the launcher itself: 64 usage, 70 internal error, 77 refused
// (nothing ran), 126 spawn failed, 127 target missing. Otherwise the child's
// exit status is passed through, and a child killed by signal N kills the
// launcher with N.

import CryptoKit
import Darwin
import Foundation
import Security

let launcherVersion = "1"
let bundleIdentifier = "com.danielraffel.shipyard.launcher"
/// The codesign identifier of every Shipyard release binary.
let targetIdentifier = "shipyard"
/// The only Developer ID team this launcher runs as. A bundle re-signed by any
/// other team refuses everything, even though its signature is valid.
let expectedTeam = "95CX6P84C4"
/// Set in the child's environment so wrappers never route through twice.
let launcherEnv = "SHIPYARD_LAUNCHER_APP"
/// Set when the launcher hands a script back to plain /bin/bash: the reason,
/// so the script neither routes again nor claims the launcher's identity.
let fallbackEnv = "SHIPYARD_LAUNCHER_FALLBACK"
/// Scripts the launcher may run, relative to the account's home directory.
/// They cannot carry a code signature, so they are trusted by this list and
/// by ownership (see requireTrustedChain), the same trust launchd gives them.
let allowedScripts: [String: String] = [
    "steward-tick": ".local/libexec/shipyard-steward-tick.sh",
]
/// Shells read these before the script runs, and exported functions override
/// commands, so none of them may reach a script the launcher runs.
let scriptUnsafeEnv: Set<String> = ["BASH_ENV", "ENV", "SHELLOPTS", "BASHOPTS", "CDPATH", "GLOBIGNORE", "PS4"]

// Signals forwarded to the child. SIGINT and SIGQUIT come from the terminal
// to the whole foreground process group, so they are not forwarded again when
// the launcher is in that group.
let forwardedSignals: [Int32] = [SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGWINCH]
nonisolated(unsafe) var childPid: pid_t = 0
nonisolated(unsafe) var terminalForeground = false

func fail(_ message: String, code: Int32) -> Never {
    FileHandle.standardError.write(Data("shipyard-launcher: \(message)\n".utf8))
    logLine("refused code=\(code) reason=\(message)")
    exit(code)
}

// MARK: environment of this build

func testOverride(_ name: String) -> String? {
#if SHIPYARD_LAUNCHER_TESTING
    return ProcessInfo.processInfo.environment[name]
#else
    return nil
#endif
}

/// The account's home directory from the user database, never $HOME, which
/// the caller controls.
func accountHomeIfKnown() -> String? {
    if let home = testOverride("SHIPYARD_LAUNCHER_TEST_HOME") { return home }
    guard let entry = getpwuid(getuid()), let dir = entry.pointee.pw_dir else { return nil }
    return String(cString: dir)
}

func accountHome() -> String {
    guard let home = accountHomeIfKnown() else { fail("cannot read the account's home directory", code: 70) }
    return home
}

/// One line per launch or refusal in ~/Library/Logs/shipyard-launcher.log
/// (internal disk). Best effort: a log failure never changes the outcome.
func logLine(_ text: String) {
    guard let home = accountHomeIfKnown() else { return }
    let path = home + "/Library/Logs/shipyard-launcher.log"
    let fd = open(path, O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return }
    defer { close(fd) }
    let stamp = ISO8601DateFormatter().string(from: Date())
    let line = Data("\(stamp) pid=\(getpid()) \(text)\n".utf8)
    _ = line.withUnsafeBytes { write(fd, $0.baseAddress, line.count) }
}

/// The .app directory: three levels above this executable's real path
/// (Contents/MacOS/<exe>), taken from the kernel, not from argv[0].
func bundleRoot() -> URL {
    var size: UInt32 = 0
    _ = _NSGetExecutablePath(nil, &size)
    var buffer = [CChar](repeating: 0, count: Int(size) + 1)
    guard _NSGetExecutablePath(&buffer, &size) == 0,
          let executable = canonical(String(cString: buffer)) else { fail("cannot locate the launcher executable", code: 70) }
    return URL(fileURLWithPath: executable)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
}

// MARK: code signatures

/// Verify this launcher's own bundle and return its Team ID. A launcher with
/// no valid Developer ID team refuses everything.
func ownTeam() -> String {
    if testOverride("SHIPYARD_LAUNCHER_TEST_REQUIREMENT") != nil { return "TESTTEAM00" }
    var code: SecStaticCode?
    guard SecStaticCodeCreateWithPath(bundleRoot() as CFURL, SecCSFlags(), &code) == errSecSuccess,
          let code else { fail("cannot inspect the launcher's own signature", code: 77) }
    let flags = SecCSFlags(rawValue: kSecCSStrictValidate | kSecCSCheckAllArchitectures)
    let ownRequirement = "identifier \"\(bundleIdentifier)\" and anchor apple generic"
        + " and certificate 1[field.1.2.840.113635.100.6.2.6] exists"
        + " and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    var requirement: SecRequirement?
    guard SecRequirementCreateWithString(ownRequirement as CFString, SecCSFlags(), &requirement) == errSecSuccess,
          SecStaticCodeCheckValidity(code, flags, requirement) == errSecSuccess else {
        fail("the launcher bundle is not a valid Developer ID signed \(bundleIdentifier)", code: 77)
    }
    var info: CFDictionary?
    guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
          let values = info as? [String: Any],
          let team = values[kSecCodeInfoTeamIdentifier as String] as? String,
          team.range(of: "^[A-Z0-9]{10}$", options: .regularExpression) != nil else {
        fail("the launcher has no Team ID", code: 77)
    }
    guard team == expectedTeam else { fail("the launcher is signed by team \(team), not \(expectedTeam)", code: 77) }
    return team
}

/// Developer ID Application, issued under the Developer ID CA, for `team`,
/// with the Shipyard identifier. Apple Development and other Apple-issued
/// certificates of the same team are refused.
func targetRequirement(team: String) -> SecRequirement {
    let text = testOverride("SHIPYARD_LAUNCHER_TEST_REQUIREMENT")
        ?? ("anchor apple generic"
            + " and certificate 1[field.1.2.840.113635.100.6.2.6] exists"
            + " and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
            + " and certificate leaf[subject.OU] = \"\(team)\""
            + " and identifier \"\(targetIdentifier)\"")
    var requirement: SecRequirement?
    guard SecRequirementCreateWithString(text as CFString, SecCSFlags(), &requirement) == errSecSuccess,
          let requirement else { fail("cannot build the signature requirement", code: 70) }
    return requirement
}

func requireSignedFile(_ path: String, _ requirement: SecRequirement) {
    var code: SecStaticCode?
    guard SecStaticCodeCreateWithPath(URL(fileURLWithPath: path) as CFURL, SecCSFlags(), &code) == errSecSuccess,
          let code else { fail("cannot inspect the signature of \(path)", code: 77) }
    let flags = SecCSFlags(rawValue: kSecCSStrictValidate | kSecCSCheckAllArchitectures)
    let status = SecStaticCodeCheckValidity(code, flags, requirement)
    guard status == errSecSuccess else {
        fail("\(path) is not a Developer ID signed Shipyard of this team (OSStatus \(status))", code: 77)
    }
}

/// Check the code the kernel actually loaded into the suspended child, so a
/// file swapped between the static check and the spawn is caught.
func childSatisfies(_ pid: pid_t, _ requirement: SecRequirement) -> Bool {
    var code: SecCode?
    let attributes = [kSecGuestAttributePid: NSNumber(value: pid)] as CFDictionary
    guard SecCodeCopyGuestWithAttributes(nil, attributes, SecCSFlags(), &code) == errSecSuccess,
          let code else { return false }
    return SecCodeCheckValidity(code, SecCSFlags(), requirement) == errSecSuccess
}

// MARK: target resolution

/// Not a symlink, owned by this user or root, not group- or world-writable.
func requireTrusted(_ path: String) {
    var info = stat()
    guard lstat(path, &info) == 0 else { fail("cannot inspect \(path): \(String(cString: strerror(errno)))", code: 77) }
    guard (info.st_mode & S_IFMT) != S_IFLNK else { fail("\(path) is a symlink", code: 77) }
    guard info.st_uid == getuid() || info.st_uid == 0 else { fail("\(path) is not owned by this user", code: 77) }
    guard info.st_mode & 0o022 == 0 else { fail("\(path) is group- or world-writable", code: 77) }
}

/// `path` and every directory above it up to and including `top`.
func requireTrustedChain(_ path: String, upTo top: String) {
    guard path.hasPrefix(top + "/") else { fail("\(path) is outside \(top)", code: 77) }
    var current = path
    while true {
        requireTrusted(current)
        if current == top { return }
        current = (current as NSString).deletingLastPathComponent
    }
}

func canonical(_ path: String) -> String? {
    guard let resolved = realpath(path, nil) else { return nil }
    defer { free(resolved) }
    return String(cString: resolved)
}

/// The Shipyard binary `~/.local/bin/shipyard` names: a release in
/// auth-generations/<64 hex>/shipyard (fleet-update) or a regular file at the
/// pointer itself (install.sh). Nothing else.
func resolveShipyard(home: String) -> String {
    let pointer = home + "/.local/bin/shipyard"
    var info = stat()
    guard lstat(pointer, &info) == 0 else { fail("\(pointer) does not exist", code: 127) }
    // A symlinked home (or /Users) would make every prefix check below lie.
    guard let realHome = canonical(home), realHome == home else { fail("home \(home) is not a canonical path", code: 77) }
    requireTrustedChain(home + "/.local/bin", upTo: home)
    guard let target = canonical(pointer) else { fail("\(pointer) does not resolve", code: 127) }
    let generations = home + "/.local/share/shipyard/auth-generations/"
    if target != pointer {
        guard target.hasPrefix(generations) else { fail("\(pointer) points outside \(generations)", code: 77) }
        let parts = target.dropFirst(generations.count).split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, parts[1] == "shipyard",
              parts[0].range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil else {
            fail("\(target) is not auth-generations/<id>/shipyard", code: 77)
        }
    }
    requireTrustedChain(target, upTo: home)
    guard lstat(target, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG else { fail("\(target) is not a regular file", code: 77) }
    return target
}

// MARK: scripts

/// Run an allowlisted script under the launcher's identity only when its
/// bytes match the SHA-256 the activation record binds to it (fleet-deploy
/// records it when it installs the script from main). The verified bytes are
/// copied to a private directory and that copy runs, so a write after the
/// check changes nothing. Any doubt (no binding, a different hash, an untrusted
/// or unreadable file) hands the installed script to plain /bin/bash instead,
/// exactly as its LaunchAgent would run it without the launcher.
func runScript(home: String, team: String, name: String, arguments: [String]) -> Never {
    guard let relative = allowedScripts[name] else { fail("script \(name) is not in the launcher's allowlist", code: 77) }
    let path = home + "/" + relative
    func fallback(_ reason: String) -> Never {
        logLine("launcher-script-mismatch script=\(name) reason=\(reason)")
        FileHandle.standardError.write(Data("shipyard-launcher: launcher-script-mismatch: \(reason); running \(path) without the launcher\n".utf8))
        var environment = ProcessInfo.processInfo.environment
        environment.removeValue(forKey: launcherEnv)
        environment[fallbackEnv] = "launcher-script-mismatch"
        let argv = (["/bin/bash", path] + arguments).map { strdup($0) } + [nil]
        let envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        // exec keeps this pid; its identity becomes /bin/bash, as without the launcher.
        execve("/bin/bash", argv, envp)
        fail("cannot exec /bin/bash: \(String(cString: strerror(errno)))", code: 126)
    }
    guard let realHome = canonical(home), realHome == home else { fallback("home is not a canonical path") }
    if let problem = untrustedChain(path, upTo: home) { fallback(problem) }
    let expected: String
    switch boundRecord(home: home, team: team) {
    case .failure(let error): fallback("no bound record: \(error.reason)")
    case .success(let record):
        guard let scripts = record["scripts"] as? [String: Any], let entry = scripts[name] as? [String: Any],
              (entry["path"] as? String) == path, let sha = entry["sha256"] as? String,
              sha.range(of: "^[0-9a-f]{64}$", options: .regularExpression) != nil else {
            fallback("no recorded hash for \(name)")
        }
        expected = sha
    }
    let fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { fallback("cannot open the script") }
    let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
    var info = stat()
    guard fstat(fd, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG, info.st_uid == getuid(),
          info.st_mode & 0o022 == 0 else { fallback("the script is not a trusted regular file") }
    let bytes = handle.readDataToEndOfFile()
    let actual = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    guard actual == expected else { fallback("sha256 \(actual) is not the recorded \(expected)") }
    var template = Array((NSTemporaryDirectory() + "shipyard-launcher-script.XXXXXX").utf8CString)
    guard let made = mkdtemp(&template) else { fail("cannot create a private directory", code: 70) }
    let directory = String(cString: made)
    let copy = directory + "/" + (relative as NSString).lastPathComponent
    let out = open(copy, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o500)
    let wrote = out >= 0 ? bytes.withUnsafeBytes { write(out, $0.baseAddress, bytes.count) } : -1
    if out >= 0 { close(out) }
    guard wrote == bytes.count else {
        try? FileManager.default.removeItem(atPath: directory)
        fail("cannot stage the verified script", code: 70)
    }
    run("/bin/bash", ["--noprofile", "--norc", copy] + arguments, script: true, requirement: nil,
        mode: "script:\(name) sha256=\(actual)", home: home,
        cleanup: { try? FileManager.default.removeItem(atPath: directory) })
}

/// Like requireTrustedChain, but returns the problem instead of exiting.
func untrustedChain(_ path: String, upTo top: String) -> String? {
    guard path.hasPrefix(top + "/") else { return "\(path) is outside \(top)" }
    var current = path
    while true {
        var info = stat()
        guard lstat(current, &info) == 0 else { return "cannot inspect \(current)" }
        if (info.st_mode & S_IFMT) == S_IFLNK { return "\(current) is a symlink" }
        if !(info.st_uid == getuid() || info.st_uid == 0) { return "\(current) is not owned by this user" }
        if info.st_mode & 0o022 != 0 { return "\(current) is group- or world-writable" }
        if current == top { return nil }
        current = (current as NSString).deletingLastPathComponent
    }
}

// MARK: spawning

/// The system tool directories every child starts with.
let systemPath = "/usr/bin:/bin:/usr/sbin:/sbin"

/// The child's environment: the caller's, without `DYLD_*`, with the marker
/// that stops routing twice, and a PATH the launcher sets rather than the
/// caller. A script gets the system directories and the bundle's own
/// directory. Shipyard gets its fixed unattended tool path (Homebrew,
/// ~/.local/bin, then the system directories), the same list its LaunchAgents
/// use, because it resolves the guarded `gh` wrapper by name.
func childEnvironment(script: Bool, home: String) -> [String] {
    var environment = ProcessInfo.processInfo.environment
    for key in environment.keys where key.hasPrefix("DYLD_") { environment.removeValue(forKey: key) }
    if script {
        for key in environment.keys where scriptUnsafeEnv.contains(key) || key.hasPrefix("BASH_FUNC_") {
            environment.removeValue(forKey: key)
        }
        environment["PATH"] = systemPath + ":" + bundleRoot().appendingPathComponent("Contents/MacOS").path
    } else {
        environment["PATH"] = "/opt/homebrew/bin:/usr/local/bin:\(home)/.local/bin:" + systemPath
    }
    environment.removeValue(forKey: fallbackEnv)
    environment[launcherEnv] = bundleRoot().path
    return environment.sorted { $0.key < $1.key }.map { "\($0.key)=\($0.value)" }
}

/// Spawn suspended, with default dispositions and an empty mask for every
/// forwarded signal, in the launcher's own process group.
func spawnSuspended(_ executable: String, _ arguments: [String], _ environment: [String], stdout: Int32? = nil) -> pid_t {
    var attr: posix_spawnattr_t?
    guard posix_spawnattr_init(&attr) == 0 else { fail("cannot initialize spawn attributes", code: 70) }
    defer { posix_spawnattr_destroy(&attr) }
    var defaults = sigset_t(), empty = sigset_t()
    sigemptyset(&defaults); sigemptyset(&empty)
    for number in forwardedSignals { sigaddset(&defaults, number) }
    // Descriptors are inherited as they would be across exec.
    let flags = Int16(POSIX_SPAWN_START_SUSPENDED | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK)
    guard posix_spawnattr_setflags(&attr, flags) == 0,
          posix_spawnattr_setsigdefault(&attr, &defaults) == 0,
          posix_spawnattr_setsigmask(&attr, &empty) == 0 else { fail("cannot set spawn attributes", code: 70) }
    let argv = ([executable] + arguments).map { strdup($0) }
    let envp = environment.map { strdup($0) }
    defer { argv.forEach { free($0) }; envp.forEach { free($0) } }
    var argvPointers = argv + [nil]
    var envpPointers = envp + [nil]
    var actions: posix_spawn_file_actions_t?
    guard posix_spawn_file_actions_init(&actions) == 0 else { fail("cannot initialize spawn file actions", code: 70) }
    defer { posix_spawn_file_actions_destroy(&actions) }
    if let stdout {
        guard posix_spawn_file_actions_adddup2(&actions, stdout, 1) == 0,
              posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0) == 0 else {
            fail("cannot redirect the child's output", code: 70)
        }
    }
    var pid: pid_t = 0
    let result = argvPointers.withUnsafeMutableBufferPointer { av in
        envpPointers.withUnsafeMutableBufferPointer { ep in
            posix_spawn(&pid, executable, &actions, &attr, av.baseAddress, ep.baseAddress)
        }
    }
    guard result == 0 else { fail("cannot spawn \(executable): \(String(cString: strerror(result)))", code: 126) }
    return pid
}

func forward(_ number: Int32) {
    if (number == SIGINT || number == SIGQUIT) && terminalForeground { return }
    if childPid > 0 { kill(childPid, number) }
}

func installForwarding() {
    for number in forwardedSignals {
        var action = sigaction()
        action.__sigaction_u.__sa_handler = { forward($0) }
        sigemptyset(&action.sa_mask)
        action.sa_flags = SA_RESTART
        sigaction(number, &action, nil)
    }
}

func exitLikeChild(_ status: Int32) -> Never {
    let signal = status & 0x7f
    if signal == 0 { exit((status >> 8) & 0xff) }
    if signal == 0x7f { exit(70) }
    // Die of the same signal, without a core file of our own.
    var noCore = rlimit(rlim_cur: 0, rlim_max: 0)
    setrlimit(RLIMIT_CORE, &noCore)
    var action = sigaction()
    action.__sigaction_u.__sa_handler = SIG_DFL
    sigemptyset(&action.sa_mask)
    sigaction(signal, &action, nil)
    var unblock = sigset_t()
    sigemptyset(&unblock); sigaddset(&unblock, signal)
    sigprocmask(SIG_UNBLOCK, &unblock, nil)
    kill(getpid(), signal)
    exit(128 + signal)
}

func run(_ executable: String, _ arguments: [String], script: Bool, requirement: SecRequirement?, mode: String,
         home: String, cleanup: (() -> Void)? = nil) -> Never {
    // Hold the forwarded signals until the child is verified and the handlers
    // know its pid; any that arrive meanwhile are delivered on unblock.
    var blocked = sigset_t(), previous = sigset_t()
    sigemptyset(&blocked)
    for number in forwardedSignals { sigaddset(&blocked, number) }
    sigprocmask(SIG_BLOCK, &blocked, &previous)
    terminalForeground = isatty(0) == 1 && tcgetpgrp(0) == getpgrp()
    let pid = spawnSuspended(executable, arguments, childEnvironment(script: script, home: home))
    if let requirement, !childSatisfies(pid, requirement) {
        kill(pid, SIGKILL)
        var ignored: Int32 = 0
        while waitpid(pid, &ignored, 0) == -1 && errno == EINTR {}
        fail("the spawned \(executable) is not the verified Shipyard; killed it", code: 77)
    }
    childPid = pid
    installForwarding()
    logLine("run mode=\(mode) target=\(executable) child=\(pid)")
    kill(pid, SIGCONT)
    sigprocmask(SIG_SETMASK, &previous, nil)
    var status: Int32 = 0
    while true {
        let result = waitpid(pid, &status, 0)
        if result == pid { cleanup?(); exitLikeChild(status) }
        if result == -1 && errno != EINTR { fail("waitpid failed: \(String(cString: strerror(errno)))", code: 70) }
    }
}

// MARK: read-only checks

/// Whether this process, as its own responsible process, holds Full Disk
/// Access. The system TCC database exists on every Mac and only a process
/// with Full Disk Access may open it: EPERM means no grant. Opening it never
/// raises a prompt, and nothing on /Volumes is touched.
func checkAccess(result: String?) -> Never {
    let database = testOverride("SHIPYARD_LAUNCHER_TEST_TCC_DB") ?? "/Library/Application Support/com.apple.TCC/TCC.db"
    let fd = open(database, O_RDONLY | O_CLOEXEC)
    let error = fd >= 0 ? 0 : errno
    if fd >= 0 { close(fd) }
    let state = fd >= 0 ? "granted" : (error == EPERM || error == EACCES ? "denied" : "unknown")
    let report: [String: Any] = [
        "schema": 1, "launcher_version": launcherVersion, "full_disk_access": state,
        "errno": error, "pid": Int(getpid()), "ppid": Int(getppid()),
    ]
    guard var data = try? JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]) else {
        fail("cannot encode the access report", code: 70)
    }
    data.append(Data("\n".utf8))
    if let result {
        guard result.hasPrefix("/") else { fail("--launcher-result must be an absolute path", code: 64) }
        let out = open(result, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard out >= 0 else { fail("cannot create \(result): \(String(cString: strerror(errno)))", code: 70) }
        _ = data.withUnsafeBytes { write(out, $0.baseAddress, data.count) }
        close(out)
    }
    FileHandle.standardOutput.write(data)
    exit(state == "granted" ? 0 : (state == "denied" ? 1 : 2))
}

// MARK: activation

/// How long a confirmed Full Disk Access grant is trusted before
/// --launcher-active asks macOS again. The recheck never prompts.
let grantCacheSeconds = Double(testOverride("SHIPYARD_LAUNCHER_TEST_GRANT_CACHE_SECS") ?? "") ?? 3600
let grantProbeSeconds = 20.0

/// This bundle's code directory hash, from its own signature.
func ownCDHash() -> String? {
    var code: SecStaticCode?
    guard SecStaticCodeCreateWithPath(bundleRoot() as CFURL, SecCSFlags(), &code) == errSecSuccess, let code else { return nil }
    var info: CFDictionary?
    guard SecCodeCopySigningInformation(code, SecCSFlags(), &info) == errSecSuccess,
          let values = info as? [String: Any],
          let unique = values[kSecCodeInfoUnique as String] as? Data, !unique.isEmpty else { return nil }
    return unique.map { String(format: "%02x", $0) }.joined()
}

func activeReport(_ active: Bool, _ reason: String) -> Never {
    let report: [String: Any] = ["schema": 1, "active": active, "reason": reason]
    if let data = try? JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]) {
        FileHandle.standardOutput.write(data + Data("\n".utf8))
    }
    logLine("active=\(active) reason=\(reason)")
    exit(active ? 0 : 1)
}

/// The problem with `path` as a trusted file, or nil: a regular file, not a
/// symlink, owned by this user, not group- or world-writable.
func untrustedFile(_ path: String) -> String? {
    var info = stat()
    guard lstat(path, &info) == 0 else { return "cannot inspect: \(String(cString: strerror(errno)))" }
    guard (info.st_mode & S_IFMT) == S_IFREG else { return "not a regular file" }
    guard info.st_uid == getuid() else { return "not owned by this user" }
    guard info.st_mode & 0o022 == 0 else { return "group- or world-writable" }
    return nil
}

/// Write `record` over `path` atomically (temporary file, then rename).
func replaceRecord(_ path: String, _ record: [String: Any]) -> Bool {
    guard let data = try? JSONSerialization.data(withJSONObject: record, options: [.sortedKeys, .prettyPrinted]) else { return false }
    let temporary = path + ".\(getpid()).tmp"
    let fd = open(temporary, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
    guard fd >= 0 else { return false }
    let wrote = data.withUnsafeBytes { write(fd, $0.baseAddress, data.count) }
    let synced = fsync(fd) == 0
    close(fd)
    guard wrote == data.count, synced, rename(temporary, path) == 0 else { unlink(temporary); return false }
    return true
}

/// Ask macOS, through LaunchServices, whether this bundle holds Full Disk
/// Access: `open` starts a fresh copy of the bundle as its own responsible
/// process (parent launchd), which answers --launcher-check-access. Only that
/// answer counts; anything else is "unknown".
func probeGrant() -> (String, String) {
    var template = Array((NSTemporaryDirectory() + "shipyard-launcher-grant.XXXXXX").utf8CString)
    guard let made = mkdtemp(&template) else { return ("unknown", "cannot create a private directory") }
    let directory = String(cString: made)
    defer { try? FileManager.default.removeItem(atPath: directory) }
    let result = directory + "/access.json"
    let opener = Process()
    opener.executableURL = URL(fileURLWithPath: testOverride("SHIPYARD_LAUNCHER_TEST_OPEN") ?? "/usr/bin/open")
    opener.arguments = ["-n", "-g", "-a", bundleRoot().path, "--args", "--launcher-check-access", "--launcher-result", result]
    opener.standardInput = FileHandle.nullDevice
    opener.standardOutput = FileHandle.nullDevice
    opener.standardError = FileHandle.nullDevice
    do { try opener.run() } catch { return ("unknown", "cannot run open: \(error)") }
    opener.waitUntilExit()
    guard opener.terminationReason == .exit, opener.terminationStatus == 0 else {
        return ("unknown", "open exited \(opener.terminationStatus)")
    }
    let deadline = Date().addingTimeInterval(grantProbeSeconds)
    while Date() < deadline {
        if let data = FileManager.default.contents(atPath: result),
           let report = try? JSONSerialization.jsonObject(with: data) as? [String: Any] {
            guard let state = report["full_disk_access"] as? String, ["granted", "denied", "unknown"].contains(state) else {
                return ("unknown", "unexpected access report")
            }
            guard (report["ppid"] as? Int) == 1 else { return ("unknown", "the access answer did not come from a launchd-started launcher") }
            return (state, "errno=\(report["errno"] ?? "?")")
        }
        usleep(200_000)
    }
    return ("unknown", "no access report within \(Int(grantProbeSeconds))s")
}

/// Whether Shipyard may route through this launcher now. Active only when the
/// activation record is readable and trusted, names this exact bundle (path,
/// identifier, team, code directory hash), and the Full Disk Access grant was
/// confirmed within grantCacheSeconds or is confirmed again now. A revoked
/// grant demotes the record (renamed to activation.json.demoted-<time>), so
/// nothing routes until --activate runs again. Every other doubt is
/// "not active" and leaves the record alone. Exit 0 active, 1 not.
let activationRelative = "/.local/share/shipyard/launcher/activation.json"

/// The activation record, only when it is trusted (a regular file owned by
/// this user, not group- or world-writable, in private directories up to
/// home), parses as schema 2, and names this exact bundle: path, identifier,
/// team and code directory hash. Otherwise the reason it is not usable.
func boundRecord(home: String, team: String) -> Result<[String: Any], ReasonError> {
    let path = home + activationRelative
    var info = stat()
    guard lstat(path, &info) == 0 else { return .failure(ReasonError("not_activated")) }
    if let problem = untrustedFile(path) { return .failure(ReasonError("record_untrusted: \(problem)")) }
    var directory = (path as NSString).deletingLastPathComponent
    while directory.hasPrefix(home + "/") || directory == home {
        var dirInfo = stat()
        guard lstat(directory, &dirInfo) == 0, (dirInfo.st_mode & S_IFMT) == S_IFDIR,
              dirInfo.st_uid == getuid() || dirInfo.st_uid == 0, dirInfo.st_mode & 0o022 == 0 else {
            return .failure(ReasonError("record_untrusted: \(directory) is not a private directory"))
        }
        if directory == home { break }
        directory = (directory as NSString).deletingLastPathComponent
    }
    let fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return .failure(ReasonError("record_unreadable")) }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: true).readDataToEndOfFile()
    guard let record = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
        return .failure(ReasonError("record_unreadable"))
    }
    guard (record["schema"] as? Int) == 2 else { return .failure(ReasonError("record_schema")) }
    let installed = canonical(home + "/.local/libexec/ShipyardLauncher.app")
    guard let app = record["app"] as? String, app == bundleRoot().path, installed == app else {
        return .failure(ReasonError("record_names_another_bundle"))
    }
    guard (record["identifier"] as? String) == bundleIdentifier else { return .failure(ReasonError("record_identifier_mismatch")) }
    guard (record["team"] as? String) == team else { return .failure(ReasonError("record_team_mismatch")) }
    guard let cdhash = ownCDHash(), (record["cdhash"] as? String) == cdhash else {
        return .failure(ReasonError("bundle_replaced: code directory hash differs from the activated bundle"))
    }
    return .success(record)
}

struct ReasonError: Error { let reason: String; init(_ reason: String) { self.reason = reason } }

func launcherActive(home: String, team: String) -> Never {
    let path = home + activationRelative
    var record: [String: Any]
    switch boundRecord(home: home, team: team) {
    case .success(let value): record = value
    case .failure(let error): activeReport(false, error.reason)
    }
    guard let checked = record["fda_checked_at"] as? Double else { activeReport(false, "record_unreadable") }
    let age = Date().timeIntervalSince1970 - checked
    if age >= 0 && age < grantCacheSeconds { activeReport(true, "grant_cached") }
    let (state, detail) = probeGrant()
    switch state {
    case "granted":
        record["fda_checked_at"] = Date().timeIntervalSince1970
        if !replaceRecord(path, record) { logLine("could not refresh fda_checked_at; probing again next time") }
        activeReport(true, "grant_rechecked")
    case "denied":
        let demoted = path + ".demoted-\(Int(Date().timeIntervalSince1970))"
        let moved = rename(path, demoted) == 0
        activeReport(false, "grant_revoked: \(moved ? "demoted to \(demoted)" : "could not demote the record") (\(detail))")
    default:
        activeReport(false, "grant_unknown: \(detail)")
    }
}

// MARK: release floor
//
// A downgrade guard without an exact pin. Every release that passes the
// signature checks is accepted unless its version is below the newest one this
// launcher has run on the host; a newer release passes at once, so a fresh
// install never waits for anything. (An exact path and cdhash pin would make
// every install fall back until someone re-recorded it, which is the prompt
// window this bundle exists to close.)

let floorRelative = "/.local/share/shipyard/launcher/release-floor.json"
let versionSeconds = 15.0

func parseVersion(_ text: String) -> [Int]? {
    let line = text.split(separator: "\n").first.map(String.init) ?? ""
    guard let match = line.range(of: "^shipyard [0-9]+\\.[0-9]+\\.[0-9]+", options: .regularExpression) else { return nil }
    let parts = line[match].dropFirst("shipyard ".count).split(separator: ".").compactMap { Int($0) }
    return parts.count == 3 ? parts : nil
}

/// The release's own `--version`, run the way the release itself will be run:
/// spawned suspended, the loaded code checked against the requirement, then
/// resumed and bounded. Nil when any of that fails or the output is not
/// `shipyard X.Y.Z`.
func releaseVersion(_ target: String, _ requirement: SecRequirement, home: String) -> [Int]? {
    var fds: [Int32] = [0, 0]
    guard pipe(&fds) == 0 else { return nil }
    let pid = spawnSuspended(target, ["--version"], childEnvironment(script: false, home: home), stdout: fds[1])
    close(fds[1])
    defer { close(fds[0]) }
    guard childSatisfies(pid, requirement) else {
        kill(pid, SIGKILL); var ignored: Int32 = 0; waitpid(pid, &ignored, 0); return nil
    }
    kill(pid, SIGCONT)
    _ = fcntl(fds[0], F_SETFL, O_NONBLOCK)
    var output = Data()
    var buffer = [UInt8](repeating: 0, count: 4096)
    var status: Int32 = 0
    let deadline = Date().addingTimeInterval(versionSeconds)
    while true {
        let count = read(fds[0], &buffer, buffer.count)
        if count > 0 { output.append(buffer, count: count); if output.count > 65536 { break } }
        if waitpid(pid, &status, WNOHANG) == pid { break }
        if Date() >= deadline { kill(pid, SIGKILL); waitpid(pid, &status, 0); return nil }
        if count <= 0 { usleep(20_000) }
    }
    while true {
        let count = read(fds[0], &buffer, buffer.count)
        if count <= 0 { break }
        output.append(buffer, count: count)
    }
    guard status & 0x7f == 0, (status >> 8) & 0xff == 0 else { return nil }
    return parseVersion(String(decoding: output, as: UTF8.self))
}

/// The newest version this launcher ran, nil before the first run, or the
/// reason the floor file cannot be trusted (which fails closed).
func readFloor(home: String) -> Result<[Int]?, ReasonError> {
    let path = home + floorRelative
    var info = stat()
    guard lstat(path, &info) == 0 else { return .success(nil) }
    if let problem = untrustedFile(path) { return .failure(ReasonError("release floor untrusted: \(problem)")) }
    let fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)
    guard fd >= 0 else { return .failure(ReasonError("release floor unreadable")) }
    let data = FileHandle(fileDescriptor: fd, closeOnDealloc: true).readDataToEndOfFile()
    guard let value = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
          (value["schema"] as? Int) == 1, let version = value["version"] as? String,
          let parsed = parseVersion("shipyard " + version) else {
        return .failure(ReasonError("release floor unreadable"))
    }
    return .success(parsed)
}

func versionText(_ version: [Int]) -> String { version.map(String.init).joined(separator: ".") }

/// Nil when `version` may run, else why not.
func belowFloor(_ version: [Int], home: String) -> String? {
    switch readFloor(home: home) {
    case .failure(let error): return error.reason
    case .success(nil): return nil
    case .success(let floor?):
        return version.lexicographicallyPrecedes(floor)
            ? "release_downgrade: \(versionText(version)) is older than \(versionText(floor)), the newest run here" : nil
    }
}

func raiseFloor(_ version: [Int], home: String) {
    if case .success(let floor?) = readFloor(home: home), !floor.lexicographicallyPrecedes(version) { return }
    let path = home + floorRelative
    let directory = (path as NSString).deletingLastPathComponent
    mkdir(directory, 0o700)
    let record: [String: Any] = ["schema": 1, "version": versionText(version), "raised_at": Date().timeIntervalSince1970]
    if !replaceRecord(path, record) { logLine("could not raise the release floor to \(versionText(version))") }
}

/// Run the release without the launcher: exec keeps this pid, so it is
/// exactly what the job would run on its own.
func fallbackShipyard(_ target: String, _ arguments: [String], reason: String) -> Never {
    logLine("launcher-release-fallback reason=\(reason)")
    FileHandle.standardError.write(Data("shipyard-launcher: \(reason); running \(target) without the launcher\n".utf8))
    var environment = ProcessInfo.processInfo.environment
    environment.removeValue(forKey: launcherEnv)
    environment[fallbackEnv] = "launcher-release-fallback"
    let argv = ([target] + arguments).map { strdup($0) } + [nil]
    let envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
    execve(target, argv, envp)
    fail("cannot exec \(target): \(String(cString: strerror(errno)))", code: 126)
}

// MARK: main

let arguments = Array(CommandLine.arguments.dropFirst())
let first = arguments.first ?? ""
if first == "--launcher-version" {
    print("shipyard-launcher \(launcherVersion)")
    exit(0)
}
if first == "--launcher-check-access" {
    var result: String? = nil
    if arguments.count == 3, arguments[1] == "--launcher-result" { result = arguments[2] }
    else if arguments.count != 1 { fail("usage: --launcher-check-access [--launcher-result <file>]", code: 64) }
    checkAccess(result: result)
}
if first.hasPrefix("--launcher-") && !["--launcher-verify", "--launcher-active", "--launcher-script"].contains(first) {
    fail("unknown launcher option \(first)", code: 64)
}
let home = accountHome()
let team = ownTeam()
let requirement = targetRequirement(team: team)
if first == "--launcher-script" {
    guard arguments.count >= 2 else { fail("usage: --launcher-script <name> [arguments...]", code: 64) }
    runScript(home: home, team: team, name: arguments[1], arguments: Array(arguments.dropFirst(2)))
}
let target = resolveShipyard(home: home)
requireSignedFile(target, requirement)
if first == "--launcher-verify" {
    guard arguments.count == 1 else { fail("usage: --launcher-verify", code: 64) }
    let report: [String: Any] = ["schema": 1, "ok": true, "launcher_version": launcherVersion, "team": team, "target": target]
    if let data = try? JSONSerialization.data(withJSONObject: report, options: [.sortedKeys]) {
        FileHandle.standardOutput.write(data + Data("\n".utf8))
    }
    exit(0)
}
let version = releaseVersion(target, requirement, home: home)
if first == "--launcher-active" {
    guard arguments.count == 1 else { fail("usage: --launcher-active", code: 64) }
    guard let version else { activeReport(false, "release_version_unreadable") }
    if let reason = belowFloor(version, home: home) { activeReport(false, reason) }
    launcherActive(home: home, team: team)
}
guard let version else { fallbackShipyard(target, arguments, reason: "release_version_unreadable") }
if let reason = belowFloor(version, home: home) { fallbackShipyard(target, arguments, reason: reason) }
raiseFloor(version, home: home)
#if SHIPYARD_LAUNCHER_TESTING
// Lets a test replace the release between the static check and the spawn,
// the race the check of the suspended child exists for.
if let swap = ProcessInfo.processInfo.environment["SHIPYARD_LAUNCHER_TEST_SWAP_IN"] {
    guard rename(swap, target) == 0 else { fail("test swap failed", code: 70) }
}
#endif
run(target, arguments, script: false, requirement: requirement, mode: "shipyard \(versionText(version))", home: home)
