import Foundation
import SwiftUI
import AppKit
import DevConsoleCore

@MainActor
final class UpdateModel: ObservableObject {
    @Published private(set) var release: DevConsoleRelease?
    @Published private(set) var errorMessage: String?
    @Published private(set) var status = ""
    @Published private(set) var isChecking = false
    @Published private(set) var isInstalling = false

    func check() async {
        guard !isChecking else { return }
        errorMessage = nil
        isChecking = true; defer { isChecking = false }
        do { release = try await DevConsoleUpdateService.latestAcceptableRelease(currentVersion: DevConsoleUpdateService.installedVersion()); status = release == nil ? "최신 버전입니다." : "업데이트를 설치할 수 있습니다." }
        catch { errorMessage = error.localizedDescription }
    }
    func installAvailable() async {
        guard let release, !isInstalling else { return }
        errorMessage = nil
        isInstalling = true; status = "업데이트를 준비 중입니다."; defer { isInstalling = false }
        do {
            if try await DevConsoleUpdateService.install(release) {
                status = "업데이트를 설치하기 위해 앱을 종료합니다."
                NSApplication.shared.terminate(nil)
            }
        } catch { errorMessage = error.localizedDescription }
    }
}

enum DevConsoleUpdateService {
    static func latestRelease() async throws -> DevConsoleRelease {
        let url = URL(string: "https://api.github.com/repos/\(DevConsoleReleasePolicy.owner)/\(DevConsoleReleasePolicy.repository)/releases/latest")!
        let (data, response) = try await URLSession.shared.data(from: url)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw URLError(.badServerResponse) }
        return try JSONDecoder().decode(DevConsoleRelease.self, from: data)
    }
    static func latestAcceptableRelease(currentVersion: String) async throws -> DevConsoleRelease? {
        let release = try await latestRelease()
        return DevConsoleReleasePolicy.isAcceptable(release, currentVersion: currentVersion) ? release : nil
    }
    static func installedVersion() -> String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0" }

    /// Returns true only after a detached installer is ready; the caller must then terminate via AppKit.
    static func install(_ release: DevConsoleRelease) async throws -> Bool {
        guard Bundle.main.bundleIdentifier == DevConsoleReleasePolicy.bundleIdentifier,
              Bundle.main.bundleURL.lastPathComponent == DevConsoleReleasePolicy.appName,
              DevConsoleReleasePolicy.isAcceptable(release, currentVersion: installedVersion()), let asset = release.zip,
              let version = release.version else { throw DevConsoleArchiveError.invalid("DevConsole 전용 업데이트가 아닙니다.") }
        let (archive, response) = try await URLSession.shared.data(from: asset.browserDownloadURL)
        guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw URLError(.badServerResponse) }
        let root = URL(fileURLWithPath: "/tmp", isDirectory: true).appendingPathComponent("devconsole-update-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        var scheduled = false
        defer { if !scheduled { try? FileManager.default.removeItem(at: root) } }
        let zip = root.appendingPathComponent(DevConsoleReleasePolicy.archiveName)
        try archive.write(to: zip, options: .atomic)
        let installed = Bundle.main.bundleURL
        let target = installed.path.contains("/AppTranslocation/") ? URL(fileURLWithPath: "/Applications/DevConsole.app") : installed
        let backup = target.deletingLastPathComponent().appendingPathComponent("DevConsole.previous.app")
        try DevConsoleArchiveValidator.withValidatedApp(zip: zip, expected: .init(version: version)) { candidate in
            let staged = root.appendingPathComponent("DevConsole.next.app")
            try FileManager.default.copyItem(at: candidate, to: staged)
            try scheduleSwap(appPID: ProcessInfo.processInfo.processIdentifier, staged: staged, target: target, backup: backup)
            scheduled = true
        }
        return true
    }

    private static func scheduleSwap(appPID: Int32, staged: URL, target: URL, backup: URL) throws {
        let script = staged.deletingLastPathComponent().appendingPathComponent("install.sh")
        let body = DevConsoleInstallerScript.make(
            appPID: appPID,
            staged: staged,
            target: target,
            backup: backup
        )
        try body.write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
        let command = DevConsoleInstallerScript.detachedCommand(script: script)
        let process = Process()
        if !DevConsoleInstallerScript.requiresAdministrator(target: target) {
            process.executableURL = URL(fileURLWithPath: "/bin/zsh")
            process.arguments = ["-c", command]
        } else {
            process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
            process.arguments = ["-e", "do shell script " + appleScriptQuote(command) + " with administrator privileges"]
        }
        try process.run(); process.waitUntilExit()
        guard process.terminationStatus == 0 else { throw DevConsoleArchiveError.invalid("업데이트 설치 도우미를 시작하지 못했습니다.") }
    }

    private static func appleScriptQuote(_ value: String) -> String { "\"" + value.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\"" }
}
