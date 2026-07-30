import Foundation
import SwiftUI
import DevConsoleCore

@MainActor
final class UpdateModel: ObservableObject {
    private static let autoCheckIntervalNanoseconds: UInt64 = 21_600_000_000_000
    private var previousInstallError = DevConsoleUpdateService.previousInstallError()

    @Published private(set) var release: DevConsoleRelease?
    @Published private(set) var errorMessage: String?
    @Published private(set) var status = ""
    @Published private(set) var isChecking = false
    @Published private(set) var isInstalling = false

    init() {
        errorMessage = previousInstallError
    }

    func runAutoChecks() async {
        await check()
        while !Task.isCancelled {
            try? await Task.sleep(nanoseconds: Self.autoCheckIntervalNanoseconds)
            guard !Task.isCancelled else { return }
            await check()
        }
    }

    func check() async {
        guard !isChecking else { return }
        isChecking = true; defer { isChecking = false }
        do {
            release = try await DevConsoleUpdateService.latestAcceptableRelease(currentVersion: DevConsoleUpdateService.installedVersion())
            errorMessage = previousInstallError
            status = release == nil ? "최신 버전입니다." : "업데이트를 설치할 수 있습니다."
        }
        catch { errorMessage = error.localizedDescription }
    }
    func installAvailable() async -> Bool {
        guard let release, !isInstalling else { return false }
        previousInstallError = nil
        errorMessage = nil
        isInstalling = true; status = "업데이트를 준비 중입니다."; defer { isInstalling = false }
        do {
            if try await DevConsoleUpdateService.install(release) {
                status = "업데이트를 설치하기 위해 앱을 종료합니다."
                return true
            }
        } catch { errorMessage = error.localizedDescription }
        return false
    }
}

enum DevConsoleUpdateService {
    private static let errorReport = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("DevConsole/update-error.txt")

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
    static func previousInstallError() -> String? {
        try? String(contentsOf: errorReport, encoding: .utf8)
    }

    /// Returns true only after a detached launcher is ready; the caller must then terminate via AppKit.
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
        let root = staged.deletingLastPathComponent()
        let installer = root.appendingPathComponent("install.sh")
        let launcher = root.appendingPathComponent("launch.sh")
        try FileManager.default.createDirectory(at: errorReport.deletingLastPathComponent(), withIntermediateDirectories: true)
        try DevConsoleInstallerScript.make(
            staged: staged,
            target: target,
            backup: backup
        ).write(to: installer, atomically: true, encoding: .utf8)
        try DevConsoleInstallerScript.makeLauncher(
            appPID: appPID,
            installer: installer,
            target: target,
            root: root,
            errorReport: errorReport,
            requiresAdministrator: DevConsoleInstallerScript.requiresAdministrator(target: target)
        ).write(to: launcher, atomically: true, encoding: .utf8)
        let command = DevConsoleInstallerScript.detachedCommand(script: launcher)
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/zsh")
        process.arguments = ["-c", command]
        try process.run(); process.waitUntilExit()
        guard process.terminationStatus == 0 else { throw DevConsoleArchiveError.invalid("업데이트 설치 도우미를 시작하지 못했습니다.") }
    }
}
