import Foundation

public struct DevConsoleRelease: Decodable, Sendable, Equatable {
    public struct Asset: Decodable, Sendable, Equatable {
        public let name: String
        public let browserDownloadURL: URL
        public init(name: String, browserDownloadURL: URL) { self.name = name; self.browserDownloadURL = browserDownloadURL }

        enum CodingKeys: String, CodingKey { case name; case browserDownloadURL = "browser_download_url" }
    }

    public let tagName: String
    public let draft: Bool
    public let prerelease: Bool
    public let assets: [Asset]
    public init(tagName: String, draft: Bool, prerelease: Bool, assets: [Asset]) {
        self.tagName = tagName; self.draft = draft; self.prerelease = prerelease; self.assets = assets
    }

    enum CodingKeys: String, CodingKey { case tagName = "tag_name", draft, prerelease, assets }

    public var version: String? { tagName.hasPrefix("v") ? String(tagName.dropFirst()) : tagName }
    public var zip: Asset? { assets.first { $0.name == DevConsoleReleasePolicy.archiveName && DevConsoleReleasePolicy.isTrusted($0.browserDownloadURL) } }
}

public enum DevConsoleReleasePolicy {
    public static let owner = "kmg0308"
    public static let repository = "dev-console"
    public static let archiveName = "DevConsole.zip"
    public static let appName = "DevConsole.app"
    public static let bundleIdentifier = "com.kmg0308.devconsole"
    public static let executableName = "DevConsole"

    public static func isAcceptable(_ release: DevConsoleRelease, currentVersion: String) -> Bool {
        guard !release.draft, !release.prerelease, let version = release.version,
              Version.isValid(version), release.zip != nil else { return false }
        return Version(version) > Version(currentVersion)
    }

    public static func isTrusted(_ url: URL) -> Bool {
        guard url.scheme == "https", let host = url.host else { return false }
        if host == "github.com" { return url.path.hasPrefix("/kmg0308/dev-console/releases/download/") && url.path.hasSuffix("/DevConsole.zip") }
        return host == "objects.githubusercontent.com" || host.hasSuffix(".objects.githubusercontent.com")
    }
}

public struct Version: Comparable, Sendable, Equatable {
    private let components: [Int]

    public init(_ value: String) {
        components = value.split(separator: ".", omittingEmptySubsequences: false).map { Int($0) ?? -1 }
    }
    public static func isValid(_ value: String) -> Bool {
        value.range(of: "^[0-9]+(\\.[0-9]+){1,2}$", options: .regularExpression) != nil
    }

    public static func < (lhs: Version, rhs: Version) -> Bool {
        let count = max(lhs.components.count, rhs.components.count)
        for index in 0..<count {
            let left = index < lhs.components.count ? lhs.components[index] : 0
            let right = index < rhs.components.count ? rhs.components[index] : 0
            if left != right { return left < right }
        }
        return false
    }
}

public struct DevConsoleArchiveIdentity: Sendable, Equatable {
    public let version: String
    public let build: String?
    public init(version: String, build: String? = nil) { self.version = version; self.build = build }
}

public enum DevConsoleArchiveError: Error, LocalizedError {
    case invalid(String)
    public var errorDescription: String? {
        switch self { case .invalid(let message): message }
    }
}

public enum DevConsoleArchiveValidator {
    public static func withValidatedApp<T>(zip: URL, expected: DevConsoleArchiveIdentity? = nil, _ body: (URL) throws -> T) throws -> T {
        try validateArchiveEntries(zip)
        let temporary = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: temporary) }
        let result = try run("/usr/bin/ditto", ["-x", "-k", zip.path, temporary.path])
        guard result.status == 0 else { throw DevConsoleArchiveError.invalid("ZIP 압축을 풀 수 없습니다.") }
        let app = temporary.appendingPathComponent(DevConsoleReleasePolicy.appName)
        try validate(app: app, expected: expected)
        return try body(app)
    }

    public static func validate(zip: URL, expected: DevConsoleArchiveIdentity? = nil) throws {
        try withValidatedApp(zip: zip, expected: expected) { _ in () }
    }

    private static func validateArchiveEntries(_ zip: URL) throws {
        guard FileManager.default.fileExists(atPath: zip.path) else { throw DevConsoleArchiveError.invalid("ZIP 파일이 없습니다.") }
        let listing = try run("/usr/bin/unzip", ["-Z1", zip.path])
        guard listing.status == 0, let text = String(data: listing.output, encoding: .utf8) else { throw DevConsoleArchiveError.invalid("ZIP 목록을 읽을 수 없습니다.") }
        let entries = text.split(separator: "\n").map(String.init)
        guard !entries.isEmpty, entries.allSatisfy({ !$0.hasPrefix("/") && !$0.contains("..") && $0.split(separator: "/").first == Substring(DevConsoleReleasePolicy.appName) }) else { throw DevConsoleArchiveError.invalid("ZIP은 DevConsole.app 하나만 포함해야 합니다.") }
        let details = try run("/usr/bin/zipinfo", ["-l", zip.path])
        guard details.status == 0, let info = String(data: details.output, encoding: .utf8), !info.split(separator: "\n").contains(where: { $0.first == "l" }) else { throw DevConsoleArchiveError.invalid("ZIP 심볼릭 링크는 허용되지 않습니다.") }
    }

    public static func validate(app: URL, expected: DevConsoleArchiveIdentity? = nil) throws {
        let info = app.appendingPathComponent("Contents/Info.plist")
        guard let data = try? Data(contentsOf: info),
              let plist = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any],
              plist["CFBundleIdentifier"] as? String == DevConsoleReleasePolicy.bundleIdentifier,
              plist["CFBundleExecutable"] as? String == DevConsoleReleasePolicy.executableName,
              plist["CFBundlePackageType"] as? String == "APPL",
              let version = plist["CFBundleShortVersionString"] as? String,
              let build = plist["CFBundleVersion"] as? String else {
            throw DevConsoleArchiveError.invalid("DevConsole.app 번들 정보가 올바르지 않습니다.")
        }
        if let expected, (version != expected.version || (expected.build != nil && build != expected.build)) {
            throw DevConsoleArchiveError.invalid("업데이트 번들의 버전 또는 빌드가 일치하지 않습니다.")
        }
        let manager = FileManager.default
        for path in ["Contents/MacOS/DevConsole", "Contents/Helpers/runtime-atlas", "Contents/Helpers/runtime-atlas-supervisor"] {
            let file = app.appendingPathComponent(path)
            guard manager.isExecutableFile(atPath: file.path),
                  (try? file.resourceValues(forKeys: [.isSymbolicLinkKey]).isSymbolicLink) != true,
                  (try? file.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true else {
                throw DevConsoleArchiveError.invalid("필수 실행 파일이 없습니다: \(path)")
            }
        }
        let signature = try run("/usr/bin/codesign", ["--verify", "--deep", "--strict", app.path])
        guard signature.status == 0 else { throw DevConsoleArchiveError.invalid("코드 서명 검증에 실패했습니다.") }
    }

    @discardableResult
    public static func run(_ executable: String, _ arguments: [String]) throws -> (status: Int32, output: Data) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        let output = Pipe()
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        try process.run()
        let data = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, data)
    }
}

public enum DevConsoleInstallerScript {
    public static func make(
        appPID: Int32,
        staged: URL,
        target: URL,
        backup: URL,
        relaunch: Bool = true
    ) -> String {
        let root = staged.deletingLastPathComponent()
        let relaunchCommand = relaunch
            ? "\n/usr/bin/open -n \(shellQuote(target.path))"
            : ""
        return """
        #!/bin/zsh
        set -eu
        trap 'rm -rf \(shellQuote(root.path))' EXIT
        for i in {1..100}; do kill -0 \(appPID) 2>/dev/null || break; sleep 0.1; done
        kill -0 \(appPID) 2>/dev/null && exit 4
        rm -rf \(shellQuote(backup.path))
        if [ -e \(shellQuote(target.path)) ] && ! mv \(shellQuote(target.path)) \(shellQuote(backup.path)); then exit 5; fi
        if ! mv \(shellQuote(staged.path)) \(shellQuote(target.path)) || ! /usr/bin/codesign --verify --deep --strict \(shellQuote(target.path)); then
          rm -rf \(shellQuote(target.path))
          if [ -e \(shellQuote(backup.path)) ]; then mv \(shellQuote(backup.path)) \(shellQuote(target.path)) || exit 7; fi
          exit 6
        fi\(relaunchCommand)
        rm -rf \(shellQuote(backup.path)) || true
        """
    }

    public static func requiresAdministrator(target: URL) -> Bool {
        let manager = FileManager.default
        return !manager.isWritableFile(atPath: target.deletingLastPathComponent().path)
            || (manager.fileExists(atPath: target.path)
                && !manager.isWritableFile(atPath: target.path))
    }

    public static func detachedCommand(script: URL) -> String {
        "/usr/bin/nohup /bin/zsh \(shellQuote(script.path)) </dev/null >/dev/null 2>&1 &"
    }

    private static func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\u{22}'\u{22}'") + "'"
    }
}

public enum DevConsoleFeature: Sendable { case runtimeAtlas, tokenMeter }
public enum DevConsoleShortcutAction: Equatable, Sendable { case switchFeature(forward: Bool), switchWorktree(forward: Bool), commitWorktreeSession }
public struct DevConsoleShortcutState: Equatable, Sendable {
    public var controlQIsDown = false
    public var consumedQIsDown = false
    public var consumedTabIsDown = false
    public var worktreeSessionIsActive = false
    public init() {}
}
public enum DevConsoleShortcutEvent: Sendable { case keyDown(key: String, control: Bool, shift: Bool), keyUp(key: String, control: Bool), flagsChanged(control: Bool) }
public struct DevConsoleShortcutResult: Equatable, Sendable { public let state: DevConsoleShortcutState; public let action: DevConsoleShortcutAction?; public let consumed: Bool }

public enum DevConsoleShortcutReducer {
    public static func reduce(_ state: DevConsoleShortcutState, feature: DevConsoleFeature, _ event: DevConsoleShortcutEvent) -> DevConsoleShortcutResult {
        var next = state
        switch event {
        case .keyDown(let key, _, _) where key.lowercased() == "q" && state.consumedQIsDown:
            return .init(state: next, action: nil, consumed: true)
        case .keyDown(let key, let control, _ ) where key.lowercased() == "q" && control:
            next.controlQIsDown = true
            next.consumedQIsDown = true
            return .init(state: next, action: nil, consumed: true)
        case .keyUp(let key, _) where key.lowercased() == "q" && state.consumedQIsDown:
            next.controlQIsDown = false
            next.consumedQIsDown = false
            let commit = next.worktreeSessionIsActive
            next.worktreeSessionIsActive = false
            return .init(state: next, action: commit ? .commitWorktreeSession : nil, consumed: true)
        case .keyUp(let key, _) where key.lowercased() == "control" && state.controlQIsDown:
            next.controlQIsDown = false
            let commit = next.worktreeSessionIsActive
            next.worktreeSessionIsActive = false
            return .init(state: next, action: commit ? .commitWorktreeSession : nil, consumed: true)
        case .flagsChanged(control: false) where state.controlQIsDown:
            next.controlQIsDown = false
            let commit = next.worktreeSessionIsActive
            next.worktreeSessionIsActive = false
            return .init(state: next, action: commit ? .commitWorktreeSession : nil, consumed: true)
        case .keyUp(let key, _) where key.lowercased() == "tab" && state.consumedTabIsDown:
            next.consumedTabIsDown = false
            return .init(state: next, action: nil, consumed: true)
        case .keyDown(let key, let control, let shift) where key.lowercased() == "tab" && control:
            next.consumedTabIsDown = true
            if state.controlQIsDown && feature == .runtimeAtlas {
                next.worktreeSessionIsActive = true
                return .init(state: next, action: .switchWorktree(forward: !shift), consumed: true)
            }
            if state.controlQIsDown { return .init(state: next, action: nil, consumed: true) }
            return .init(state: next, action: .switchFeature(forward: !shift), consumed: true)
        default:
            return .init(state: next, action: nil, consumed: false)
        }
    }
    public static func reduce(_ state: DevConsoleShortcutState, _ event: DevConsoleShortcutEvent) -> DevConsoleShortcutResult { reduce(state, feature: .runtimeAtlas, event) }
}
