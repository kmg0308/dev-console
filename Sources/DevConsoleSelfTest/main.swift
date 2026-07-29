import Foundation
import DevConsoleCore

func require(_ condition: @autoclosure () -> Bool, _ message: String) {
    if !condition() { fputs("FAIL: \(message)\n", stderr); exit(1) }
}

let asset = DevConsoleRelease.Asset(name: "DevConsole.zip", browserDownloadURL: URL(string: "https://github.com/kmg0308/dev-console/releases/download/v1.2.0/DevConsole.zip")!)
let release = DevConsoleRelease(tagName: "v1.2.0", draft: false, prerelease: false, assets: [asset])
require(DevConsoleReleasePolicy.isAcceptable(release, currentVersion: "1.1.9"), "new release is accepted")
require(!DevConsoleReleasePolicy.isAcceptable(release, currentVersion: "1.2.0"), "same release is rejected")
require(!DevConsoleReleasePolicy.isTrusted(URL(string: "http://github.com/kmg0308/dev-console/releases/download/v1.2.0/DevConsole.zip")!), "update URL requires HTTPS")
require(!DevConsoleReleasePolicy.isTrusted(URL(string: "https://github.com/other/dev-console/releases/download/v1.2.0/DevConsole.zip")!), "update URL requires the DevConsole repository")
require(Version("1.10") > Version("1.9.9"), "version comparison")

let fixture = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
let app = fixture.appendingPathComponent("DevConsole.app")
try! FileManager.default.createDirectory(at: app.appendingPathComponent("Contents/MacOS"), withIntermediateDirectories: true)
try! FileManager.default.createDirectory(at: app.appendingPathComponent("Contents/Helpers"), withIntermediateDirectories: true)
for path in ["Contents/MacOS/DevConsole", "Contents/Helpers/runtime-atlas", "Contents/Helpers/runtime-atlas-supervisor"] {
    try! FileManager.default.copyItem(atPath: "/usr/bin/true", toPath: app.appendingPathComponent(path).path)
}
let plist = """
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.kmg0308.devconsole</string><key>CFBundleExecutable</key><string>DevConsole</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>1.2.0</string><key>CFBundleVersion</key><string>7</string></dict></plist>
"""
try! plist.write(to: app.appendingPathComponent("Contents/Info.plist"), atomically: true, encoding: .utf8)
_ = try! DevConsoleArchiveValidator.run("/usr/bin/codesign", ["--force", "--deep", "--sign", "-", app.path])
try! DevConsoleArchiveValidator.validate(app: app, expected: .init(version: "1.2.0", build: "7"))
let zip = fixture.appendingPathComponent("DevConsole.zip")
_ = try! DevConsoleArchiveValidator.run("/bin/zsh", ["-c", "cd '\(fixture.path)' && /usr/bin/ditto -c -k --norsrc --noextattr --noqtn --keepParent DevConsole.app DevConsole.zip"])
_ = try! DevConsoleArchiveValidator.validate(zip: zip, expected: .init(version: "1.2.0", build: "7"))

let successDestination = fixture.appendingPathComponent("success")
let successRoot = fixture.appendingPathComponent("success-staging")
let successTarget = successDestination.appendingPathComponent("DevConsole.app")
let successBackup = successDestination.appendingPathComponent("DevConsole.previous.app")
let successStaged = successRoot.appendingPathComponent("DevConsole.next.app")
try! FileManager.default.createDirectory(at: successTarget, withIntermediateDirectories: true)
try! Data("old".utf8).write(to: successTarget.appendingPathComponent("marker"))
try! FileManager.default.createDirectory(at: successRoot, withIntermediateDirectories: true)
try! FileManager.default.copyItem(at: app, to: successStaged)
let successScript = successRoot.appendingPathComponent("install.sh")
try! DevConsoleInstallerScript.make(
    appPID: Int32.max,
    staged: successStaged,
    target: successTarget,
    backup: successBackup,
    relaunch: false
).write(to: successScript, atomically: true, encoding: .utf8)
try! FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: successScript.path)
let successInstall = try! DevConsoleArchiveValidator.run("/bin/zsh", [successScript.path])
require(successInstall.status == 0, "installer swaps a validated app")
require(FileManager.default.fileExists(atPath: successTarget.appendingPathComponent("Contents/Info.plist").path), "installer places candidate at target")
require(!FileManager.default.fileExists(atPath: successBackup.path), "installer removes successful backup")

let rollbackDestination = fixture.appendingPathComponent("rollback")
let rollbackRoot = fixture.appendingPathComponent("rollback-staging")
let rollbackTarget = rollbackDestination.appendingPathComponent("DevConsole.app")
let rollbackBackup = rollbackDestination.appendingPathComponent("DevConsole.previous.app")
let rollbackStaged = rollbackRoot.appendingPathComponent("DevConsole.next.app")
try! FileManager.default.createDirectory(at: rollbackTarget, withIntermediateDirectories: true)
try! Data("old".utf8).write(to: rollbackTarget.appendingPathComponent("marker"))
try! FileManager.default.createDirectory(at: rollbackRoot, withIntermediateDirectories: true)
try! FileManager.default.copyItem(at: app, to: rollbackStaged)
try! Data("tampered".utf8).write(
    to: rollbackStaged.appendingPathComponent("Contents/Info.plist")
)
let rollbackScript = rollbackRoot.appendingPathComponent("install.sh")
try! DevConsoleInstallerScript.make(
    appPID: Int32.max,
    staged: rollbackStaged,
    target: rollbackTarget,
    backup: rollbackBackup,
    relaunch: false
).write(to: rollbackScript, atomically: true, encoding: .utf8)
try! FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: rollbackScript.path)
let rollbackInstall = try! DevConsoleArchiveValidator.run("/bin/zsh", [rollbackScript.path])
require(rollbackInstall.status == 6, "installer reports candidate verification failure")
require(FileManager.default.fileExists(atPath: rollbackTarget.appendingPathComponent("marker").path), "installer restores previous app")
require(!FileManager.default.fileExists(atPath: rollbackBackup.path), "rollback consumes backup")

let lockedParent = fixture.appendingPathComponent("locked")
try! FileManager.default.createDirectory(at: lockedParent, withIntermediateDirectories: true)
try! FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: lockedParent.path)
require(
    DevConsoleInstallerScript.requiresAdministrator(
        target: lockedParent.appendingPathComponent("DevConsole.app")
    ),
    "installer requests elevation for a non-writable destination"
)
try! FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: lockedParent.path)

let detachedRoot = fixture.appendingPathComponent("detached launcher")
let detachedScript = detachedRoot.appendingPathComponent("launch.sh")
let detachedMarker = detachedRoot.appendingPathComponent("finished")
try! FileManager.default.createDirectory(at: detachedRoot, withIntermediateDirectories: true)
try! """
#!/bin/zsh
sleep 0.4
/usr/bin/touch "\(detachedMarker.path)"
""".write(to: detachedScript, atomically: true, encoding: .utf8)
let detachedLaunch = try! DevConsoleArchiveValidator.run(
    "/bin/zsh",
    ["-c", DevConsoleInstallerScript.detachedCommand(script: detachedScript)]
)
require(detachedLaunch.status == 0, "detached installer launcher starts")
require(!FileManager.default.fileExists(atPath: detachedMarker.path), "detached launcher returns before installer completion")
for _ in 0..<50 where !FileManager.default.fileExists(atPath: detachedMarker.path) {
    Thread.sleep(forTimeInterval: 0.02)
}
require(FileManager.default.fileExists(atPath: detachedMarker.path), "detached installer continues after launcher returns")

try! plist.replacingOccurrences(of: "com.kmg0308.devconsole", with: "invalid").write(to: app.appendingPathComponent("Contents/Info.plist"), atomically: true, encoding: .utf8)
do { try DevConsoleArchiveValidator.validate(app: app); require(false, "invalid app is rejected") } catch { }
try? FileManager.default.removeItem(at: fixture)

var state = DevConsoleShortcutState()
var result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "tab", control: true, shift: false))
require(result.action == .switchFeature(forward: true) && result.consumed, "control tab")
result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "tab", control: true, shift: true))
require(result.action == .switchFeature(forward: false), "control shift tab")
result = DevConsoleShortcutReducer.reduce(state, feature: .tokenMeter, .keyDown(key: "q", control: true, shift: false)); state = result.state
result = DevConsoleShortcutReducer.reduce(state, feature: .tokenMeter, .keyDown(key: "tab", control: true, shift: false))
require(result.consumed && result.action == nil, "token meter control q tab is inert")
state = result.state
result = DevConsoleShortcutReducer.reduce(state, feature: .tokenMeter, .keyUp(key: "q", control: true))
require(result.consumed && result.action == nil, "token meter control q release is inert")
state = result.state
result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "q", control: true, shift: false)); state = result.state
require(result.consumed && result.action == nil, "control q is inert")
result = DevConsoleShortcutReducer.reduce(state, .flagsChanged(control: false)); state = result.state
require(result.consumed && result.action == nil, "control q alone has no commit")
result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "q", control: false, shift: false))
require(result.consumed && result.action == nil, "consumed q repeat does not leak")
result = DevConsoleShortcutReducer.reduce(state, .keyUp(key: "q", control: false)); state = result.state
require(result.consumed && result.action == nil, "consumed q release does not leak")

result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "q", control: true, shift: false)); state = result.state
result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "tab", control: true, shift: false))
require(result.action == .switchWorktree(forward: true), "control q tab")
state = result.state
result = DevConsoleShortcutReducer.reduce(state, .keyUp(key: "tab", control: false)); state = result.state
require(result.consumed, "tab release remains consumed after modifier release")
result = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "tab", control: true, shift: true)); state = result.state
require(result.action == .switchWorktree(forward: false), "control q shift tab")
result = DevConsoleShortcutReducer.reduce(state, .keyUp(key: "q", control: true)); state = result.state
require(result.consumed && result.action == .commitWorktreeSession && !state.controlQIsDown, "control q release commits")
require(!DevConsoleShortcutReducer.reduce(state, .keyDown(key: "q", control: false, shift: false)).consumed, "plain q passes through")
require(!DevConsoleShortcutReducer.reduce(state, .keyUp(key: "q", control: false)).consumed, "plain q release passes through")

state = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "q", control: true, shift: false)).state
state = DevConsoleShortcutReducer.reduce(state, .keyDown(key: "tab", control: true, shift: false)).state
result = DevConsoleShortcutReducer.reduce(state, .keyUp(key: "control", control: false)); state = result.state
require(result.consumed && result.action == .commitWorktreeSession, "control release commits session")
result = DevConsoleShortcutReducer.reduce(state, .keyUp(key: "q", control: false)); state = result.state
require(result.consumed && result.action == nil, "q release remains consumed after control release")

if CommandLine.arguments.dropFirst().first == "--validate-update-archive" {
    guard CommandLine.arguments.count == 3 else { exit(2) }
    do { _ = try DevConsoleArchiveValidator.validate(zip: URL(fileURLWithPath: CommandLine.arguments[2])) }
    catch { fputs("FAIL update archive: \(error.localizedDescription)\n", stderr); exit(1) }
}
print("DevConsoleSelfTest passed")
